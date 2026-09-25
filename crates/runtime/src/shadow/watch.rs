//! File-system watcher that feeds the shadow checkpointer's incremental
//! checkpoints (inotify on Linux, FSEvents on macOS, via the `notify` crate).
//!
//! The watcher only collects *dirty paths* (absolute) between checkpoints;
//! it never interprets them. The checkpointer re-stats each dirty path, so an
//! extra or stale path costs one `stat` and can never produce a wrong result.
//! What must hold is the converse: every change since the last checkpoint is
//! under some dirty path. That breaks when events are lost (queue overflow,
//! watcher error, too many pending paths); the loss is recorded and the next
//! checkpoint falls back to a full metadata scan.
//!
//! **Fence**: events are delivered asynchronously by the watcher's thread, so
//! before draining, [`FsWatch::fence`] creates a uniquely named file in a
//! private fence directory (outside the workspace) and waits until its event
//! arrives. The backend delivers events in order, so every change that
//! completed before the checkpoint started has been collected by then.
//!
//! **Per-directory vs recursive**: on Linux (and other non-FSEvents,
//! non-Windows platforms) each tracked directory, plus each top-level ignored
//! directory, gets a non-recursive watch, added by the checkpointer as it
//! walks the workspace. Ignored trees (`target/`, `node_modules/`) are thus
//! never descended into, and the inotify watch count stays proportional to
//! the tracked directories. FSEvents and `ReadDirectoryChangesW` are natively
//! recursive and cheap, so there the workspace root is watched recursively
//! and the checkpointer maps events deep inside an ignored directory to that
//! directory.

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// More pending paths than this: treat as lost (a full scan is as cheap).
const MAX_DIRTY: usize = 65_536;
/// How long a checkpoint waits for its fence event before falling back.
const FENCE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Pending {
    /// Absolute path -> `deep` (the path may be a new / moved directory whose
    /// whole subtree must be walked).
    dirty: HashMap<PathBuf, bool>,
    /// Why events were lost since the last drain.
    lost: Option<String>,
    fence_seen: u64,
}

#[derive(Default)]
struct Shared {
    m: Mutex<Pending>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Paths collected since the previous drain.
pub(super) struct Drained {
    pub dirty: HashMap<PathBuf, bool>,
    /// Events were lost: the caller must do a full scan.
    pub lost: Option<String>,
}

/// Adding a watch failed.
pub(super) enum AddError {
    /// The platform's watch limit is exhausted (e.g. inotify
    /// `max_user_watches`): watching is not viable for this workspace.
    Limit(String),
    Other(String),
}

pub(super) struct FsWatch {
    watcher: RecommendedWatcher,
    shared: Arc<Shared>,
    fence_dir: PathBuf,
    fence_next: u64,
    per_dir: bool,
}

fn handle(root: &Path, fence_dir: &Path, s: &Shared, res: notify::Result<Event>) {
    let mut p = s.lock();
    let ev = match res {
        Ok(ev) => ev,
        Err(e) => {
            p.lost.get_or_insert_with(|| format!("watcher error: {e}"));
            p.dirty.clear();
            return;
        }
    };
    if ev.need_rescan() {
        p.lost.get_or_insert_with(|| "event queue overflow".into());
        p.dirty.clear();
    }
    let access = matches!(ev.kind, EventKind::Access(_));
    // Content / metadata changes concern the path itself; anything else
    // (create, remove, rename, unknown) may bring a whole subtree.
    let deep = !matches!(ev.kind, EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_)));
    for path in ev.paths {
        if path.parent() == Some(fence_dir) {
            if let Some(n) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u64>().ok()) {
                if n > p.fence_seen {
                    p.fence_seen = n;
                    s.cv.notify_all();
                }
            }
            continue;
        }
        if access || p.lost.is_some() || !path.starts_with(root) {
            continue;
        }
        *p.dirty.entry(path).or_insert(false) |= deep;
        if p.dirty.len() > MAX_DIRTY {
            p.lost = Some(format!("more than {MAX_DIRTY} changed paths"));
            p.dirty.clear();
        }
    }
}

impl FsWatch {
    /// Starts watching. `root` (canonical) is the workspace; `store` is the
    /// shadow store, used for the fence directory only when the system temp
    /// directory lies inside the workspace. Per-directory watches are added
    /// later through [`FsWatch::watch_dir`].
    pub fn start(root: &Path, store: &Path) -> Result<FsWatch, String> {
        let name = format!("agent-shadow-fence-{}", ulid::Ulid::new());
        let mut base = fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        if base.starts_with(root) {
            base = store.to_path_buf();
        }
        let fence_dir = base.join(name);
        fs::create_dir_all(&fence_dir).map_err(|e| format!("fence dir: {e}"))?;
        let fence_dir = fs::canonicalize(&fence_dir).unwrap_or(fence_dir);
        let per_dir = !cfg!(any(target_os = "macos", target_os = "windows"));
        let shared = Arc::new(Shared::default());
        let started = (|| {
            let (r, f, s) = (root.to_path_buf(), fence_dir.clone(), shared.clone());
            let mut watcher = notify::recommended_watcher(move |res| handle(&r, &f, &s, res)).map_err(|e| e.to_string())?;
            watcher.watch(&fence_dir, RecursiveMode::NonRecursive).map_err(|e| e.to_string())?;
            if !per_dir {
                watcher.watch(root, RecursiveMode::Recursive).map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(watcher)
        })();
        match started {
            Ok(watcher) => Ok(FsWatch { watcher, shared, fence_dir, fence_next: 0, per_dir }),
            Err(e) => {
                let _ = fs::remove_dir_all(&fence_dir);
                Err(e)
            }
        }
    }

    /// Ensures `dir` (a tracked or top-level ignored directory) is watched.
    /// Called before the directory is listed, so a change is either seen by
    /// the listing or produces an event. A no-op in recursive mode.
    pub fn watch_dir(&mut self, dir: &Path) -> Result<(), AddError> {
        if !self.per_dir {
            return Ok(());
        }
        match self.watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => Ok(()),
            Err(e) => match e.kind {
                // Vanished meanwhile: its parent's event covers it.
                notify::ErrorKind::PathNotFound => Ok(()),
                notify::ErrorKind::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound => Ok(()),
                notify::ErrorKind::MaxFilesWatch => Err(AddError::Limit(format!("watch limit reached at {}", dir.display()))),
                _ => Err(AddError::Other(format!("watch {}: {e}", dir.display()))),
            },
        }
    }

    /// Waits until every event queued before this call has been collected.
    /// `false` on timeout or failure (the caller must not trust the drain).
    pub fn fence(&mut self) -> bool {
        self.fence_next += 1;
        let n = self.fence_next;
        let f = self.fence_dir.join(n.to_string());
        if fs::write(&f, b"").is_err() {
            return false;
        }
        let deadline = Instant::now() + FENCE_TIMEOUT;
        let mut p = self.shared.lock();
        let ok = loop {
            if p.fence_seen >= n {
                break true;
            }
            let now = Instant::now();
            if now >= deadline {
                break false;
            }
            p = self.shared.cv.wait_timeout(p, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        };
        drop(p);
        let _ = fs::remove_file(&f);
        ok
    }

    /// Takes everything collected so far.
    pub fn drain(&self) -> Drained {
        let mut p = self.shared.lock();
        Drained { dirty: std::mem::take(&mut p.dirty), lost: p.lost.take() }
    }

    /// Test hook: drops the pending paths as if the event queue overflowed.
    pub fn inject_loss(&self, reason: &str) {
        let mut p = self.shared.lock();
        p.lost = Some(reason.to_string());
        p.dirty.clear();
    }
}

impl Drop for FsWatch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.fence_dir);
    }
}
