//! One writing session per workspace (design: "Settled boundaries"):
//! parallel writers must use separate worktrees.
//!
//! [`WorkspaceLock`] is an exclusive `flock` on a lock file keyed by the
//! workspace's canonical path. The lease is the open file: it ends when the
//! holder drops the lock or its process dies (no stale locks to clean up
//! after a crash). Within one process the lock is shared: every agent of the
//! process (a lead and its sub-agents, a server's sessions) writes through
//! the same runtime-level coordination, so a second `acquire` of the same
//! workspace returns the same lease. Another process gets
//! [`LockError::Held`], naming the holder.
//!
//! Advisory: only processes that take the lock respect it, and it is only as
//! reliable as `flock` on the file system holding the lock directory.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LockError {
    #[error(
        "workspace {workspace} is in use by another writing session ({holder}); \
         run parallel writing sessions in separate git worktrees"
    )]
    Held { workspace: String, holder: String },
    #[error("workspace lock {path}: {message}")]
    Io { path: String, message: String },
}

#[derive(Debug)]
struct Lease {
    _file: File,
    path: PathBuf,
}

/// A held workspace lock (shared within the process; see the module docs).
#[derive(Debug, Clone)]
pub struct WorkspaceLock {
    lease: Arc<Lease>,
    workspace: PathBuf,
}

fn held() -> &'static Mutex<BTreeMap<PathBuf, Weak<Lease>>> {
    static HELD: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Lease>>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// Lock file name for a workspace (its path, made file-name safe).
fn key(workspace: &Path) -> String {
    let s: String =
        workspace.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("{s}.lock")
}

impl WorkspaceLock {
    /// Take the lock for `workspace`; the lock file lives in `dir`.
    pub fn acquire(workspace: &Path, dir: &Path) -> Result<WorkspaceLock, LockError> {
        let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let path = dir.join(key(&workspace));
        let io = |e: std::io::Error| LockError::Io { path: path.display().to_string(), message: e.to_string() };
        let mut map = held().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lease) = map.get(&workspace).and_then(Weak::upgrade) {
            return Ok(WorkspaceLock { lease, workspace });
        }
        std::fs::create_dir_all(dir).map_err(io)?;
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(io)?;
        if !try_lock(&file).map_err(io)? {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            let holder = holder.lines().next().unwrap_or("unknown holder").to_string();
            return Err(LockError::Held { workspace: workspace.display().to_string(), holder });
        }
        file.set_len(0).map_err(io)?;
        file.rewind().map_err(io)?;
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        writeln!(file, "pid {} since {since} (unix time)", std::process::id()).map_err(io)?;
        let lease = Arc::new(Lease { _file: file, path });
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(workspace.clone(), Arc::downgrade(&lease));
        Ok(WorkspaceLock { lease, workspace })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.lease.path
    }
}

/// Non-blocking exclusive lock; `Ok(false)` when another holder has it.
#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: plain syscall on a valid fd owned by `file`.
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Not supported on this platform (native Windows is a non-goal): no lock.
#[cfg(not(unix))]
fn try_lock(_file: &File) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_in_process_released_on_drop() {
        let ws = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = WorkspaceLock::acquire(ws.path(), dir.path()).unwrap();
        let b = WorkspaceLock::acquire(ws.path(), dir.path()).unwrap();
        assert_eq!(a.path(), b.path());
        let text = std::fs::read_to_string(a.path()).unwrap();
        assert!(text.starts_with(&format!("pid {} ", std::process::id())), "{text}");
        // Another open file description (what another process holds) conflicts.
        let other = File::open(a.path()).unwrap();
        assert!(!try_lock(&other).unwrap());
        drop((a, b));
        // The release can lag while a concurrently forked child execs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !try_lock(&other).unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(try_lock(&other).unwrap(), "released with the last holder");
    }

    #[test]
    fn held_elsewhere_is_a_clear_error() {
        let ws = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Another process holding the lock: a separate open file description.
        let canonical = std::fs::canonicalize(ws.path()).unwrap();
        let path = dir.path().join(key(&canonical));
        std::fs::write(&path, "pid 4242 since 0 (unix time)\n").unwrap();
        let other = File::open(&path).unwrap();
        assert!(try_lock(&other).unwrap());
        let e = WorkspaceLock::acquire(ws.path(), dir.path()).unwrap_err();
        assert_eq!(
            e,
            LockError::Held { workspace: canonical.display().to_string(), holder: "pid 4242 since 0 (unix time)".into() }
        );
        assert!(e.to_string().contains("separate git worktrees"), "{e}");
        drop(other);
        // A child forked concurrently by another test shares the descriptor
        // until it execs (close-on-exec), so the release can lag briefly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = WorkspaceLock::acquire(ws.path(), dir.path());
        while got.is_err() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            got = WorkspaceLock::acquire(ws.path(), dir.path());
        }
        assert!(got.is_ok(), "the lease ended with its holder");
    }
}
