//! Configuration watcher for hot reload: watches configuration files and
//! directories (inotify / FSEvents through `notify`) and, once changes have
//! settled, calls back with the changed paths. The embedder recompiles its
//! profile and sends `Control::Reconfigure` to its sessions; the kernel applies
//! it at the next idle point, so tool definitions never change mid-turn.

use futures::future::BoxFuture;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// One watched location.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WatchSpec {
    pub path: PathBuf,
    /// Watch a directory's whole subtree (skills, commands, plugins...).
    pub recursive: bool,
}

impl WatchSpec {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        WatchSpec { path: path.into(), recursive: false }
    }
    pub fn tree(path: impl Into<PathBuf>) -> Self {
        WatchSpec { path: path.into(), recursive: true }
    }
}

type OnChange = Arc<dyn Fn(Vec<PathBuf>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Watches configuration locations until dropped.
pub struct ConfigWatcher {
    _watcher: Arc<Mutex<RecommendedWatcher>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Arm a watch for `spec`: files are watched through their directory (editors
/// replace files by renaming); a location that does not exist yet is watched
/// through its nearest existing ancestor so its creation is seen.
fn arm(w: &mut RecommendedWatcher, spec: &WatchSpec, armed: &mut BTreeSet<(PathBuf, bool)>) {
    let target = if spec.path.is_dir() {
        (spec.path.clone(), spec.recursive)
    } else {
        match spec.path.ancestors().skip(1).find(|a| a.is_dir()) {
            Some(a) => (a.to_path_buf(), false),
            None => return,
        }
    };
    if armed.contains(&target) || (!target.1 && armed.contains(&(target.0.clone(), true))) {
        return;
    }
    let mode = if target.1 { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    match w.watch(&target.0, mode) {
        Ok(()) => {
            armed.insert(target);
        }
        Err(e) => tracing::debug!(path = %target.0.display(), error = %e, "config watch not armed"),
    }
}

impl ConfigWatcher {
    /// Watch `specs`. Changes to paths for which `relevant` holds are collected
    /// until nothing changed for `debounce`, then `on_change` runs (on the
    /// current tokio runtime) with the changed paths. Locations created later
    /// are picked up on the next change.
    pub fn spawn(
        specs: Vec<WatchSpec>,
        relevant: impl Fn(&Path) -> bool + Send + Sync + 'static,
        debounce: Duration,
        on_change: impl Fn(Vec<PathBuf>) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Result<ConfigWatcher, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    let _ = tx.send(p);
                }
            }
        })
        .map_err(|e| e.to_string())?;
        let watcher = Arc::new(Mutex::new(watcher));
        let mut armed = BTreeSet::new();
        {
            let mut w = watcher.lock().unwrap();
            for s in &specs {
                arm(&mut w, s, &mut armed);
            }
        }
        let on_change: OnChange = Arc::new(on_change);
        let w2 = watcher.clone();
        let task = tokio::spawn(async move {
            loop {
                let Some(first) = rx.recv().await else { return };
                let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
                if relevant(&first) {
                    changed.insert(first);
                }
                // Settle: keep collecting until quiet for `debounce`.
                loop {
                    match tokio::time::timeout(debounce, rx.recv()).await {
                        Ok(Some(p)) => {
                            if relevant(&p) {
                                changed.insert(p);
                            }
                        }
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                {
                    let mut w = w2.lock().unwrap();
                    for s in &specs {
                        arm(&mut w, s, &mut armed);
                    }
                }
                if !changed.is_empty() {
                    on_change(changed.into_iter().collect()).await;
                }
            }
        });
        Ok(ConfigWatcher { _watcher: watcher, task })
    }
}
