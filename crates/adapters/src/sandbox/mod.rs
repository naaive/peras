//! OS sandboxes. [`probe`] detects what the platform offers; [`detect`] picks
//! the best implementation: on Linux bubblewrap, else the landlock + seccomp
//! fallback ([`LandlockSandbox`], always offline), else the unisolated
//! [`DirectExec`] (which reports `available = false`); seatbelt on macOS.
//!
//! Also here: the [`EgressProxy`] (allowlisted HTTP CONNECT / plain HTTP),
//! copy-based isolated execution ([`IsolatedCopy`], [`apply_isolated_changes`])
//! and the disposable [`Container`].

pub mod bwrap;
pub mod container;
pub mod direct;
pub mod egress;
pub mod isolate;
pub mod landlock_seccomp;
pub mod seatbelt;

pub use bwrap::BwrapSandbox;
pub use container::Container;
pub use direct::DirectExec;
pub use egress::{Allowlist, EgressEvent, EgressProxy};
pub use isolate::{apply_isolated_changes, Change, ChangeKind, IsolatedCopy, IsolatedRun};
pub use landlock_seccomp::LandlockSandbox;
pub use seatbelt::{seatbelt_profile, SeatbeltSandbox};

use agent_runtime::{ExecOutput, SandboxPort, SandboxReport};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Probe platform sandbox capability (what `agent doctor` reports).
pub fn probe() -> SandboxReport {
    select().1
}

/// The best available sandbox for this platform.
pub fn detect() -> Arc<dyn SandboxPort> {
    select().0
}

/// Linux preference: bubblewrap > landlock + seccomp > direct. The chosen
/// report carries the reasons earlier candidates were rejected.
fn select() -> (Arc<dyn SandboxPort>, SandboxReport) {
    #[cfg(target_os = "macos")]
    {
        let r = SeatbeltSandbox::probe_report();
        if r.available {
            return (Arc::new(SeatbeltSandbox::new()), r);
        }
        let d = DirectExec::new(r.notes);
        let r = d.report();
        return (Arc::new(d), r);
    }
    #[allow(unreachable_code)]
    {
        let mut notes = vec![];
        if cfg!(target_os = "linux") {
            match BwrapSandbox::probe() {
                Ok(b) => {
                    let r = b.report();
                    return (Arc::new(b), r);
                }
                Err(n) => notes.push(n),
            }
            match LandlockSandbox::probe() {
                Ok(l) => {
                    let mut r = l.report();
                    notes.push("falling back to landlock + seccomp".into());
                    notes.append(&mut r.notes);
                    r.notes = notes;
                    return (Arc::new(l), r);
                }
                Err(n) => notes.push(n),
            }
        } else {
            notes.push(format!("no sandbox backend for {}", std::env::consts::OS));
        }
        notes.push("commands run without isolation; offline-only guarantees are not enforced".into());
        let d = DirectExec::new(notes);
        let r = d.report();
        (Arc::new(d), r)
    }
}

/// Find an executable in `PATH`.
pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Run a trial command synchronously (probing); true on exit 0.
pub(crate) fn trial(bin: &Path, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub(crate) const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub(crate) fn env_with_path(env: &[(String, String)]) -> Vec<(String, String)> {
    let mut v = env.to_vec();
    if !v.iter().any(|(k, _)| k == "PATH") {
        v.push(("PATH".into(), DEFAULT_PATH.into()));
    }
    v
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: plain syscall; the child leads its own process group.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// What stopped a command early.
pub(crate) enum Stopped {
    TimedOut,
    Cancelled,
}

/// Spawn `cmd` in its own process group, capture output, enforce the timeout
/// (0 = none) and cancellation by killing the whole group. `on_stop` runs
/// before the group is killed (e.g. `docker kill`).
pub(crate) async fn exec(
    mut cmd: tokio::process::Command,
    timeout_ms: u64,
    cancel: CancellationToken,
    on_stop: impl FnOnce(),
) -> Result<ExecOutput, String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let pid = child.id();
    let mut so = child.stdout.take().expect("stdout");
    let mut se = child.stderr.take().expect("stderr");
    let out_task = tokio::spawn(async move {
        let mut b = vec![];
        let _ = so.read_to_end(&mut b).await;
        b
    });
    let err_task = tokio::spawn(async move {
        let mut b = vec![];
        let _ = se.read_to_end(&mut b).await;
        b
    });
    let deadline = async {
        if timeout_ms == 0 {
            std::future::pending::<()>().await
        } else {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await
        }
    };
    let (status, stopped) = tokio::select! {
        st = child.wait() => (st.map_err(|e| e.to_string())?, None),
        _ = deadline => (stop(&mut child, pid, on_stop).await, Some(Stopped::TimedOut)),
        _ = cancel.cancelled() => (stop(&mut child, pid, on_stop).await, Some(Stopped::Cancelled)),
    };
    if matches!(stopped, Some(Stopped::Cancelled)) {
        out_task.abort();
        err_task.abort();
        return Err("cancelled".into());
    }
    // Escaped grandchildren may hold the pipes open: bound the wait.
    let grab = |t: tokio::task::JoinHandle<Vec<u8>>| async move {
        tokio::time::timeout(Duration::from_secs(2), t).await.ok().and_then(Result::ok).unwrap_or_default()
    };
    let stdout = grab(out_task).await;
    let stderr = grab(err_task).await;
    Ok(ExecOutput {
        status: status.code(),
        stdout,
        stderr,
        timed_out: matches!(stopped, Some(Stopped::TimedOut)),
        overlay_changes: vec![],
    })
}

async fn stop(child: &mut tokio::process::Child, pid: Option<u32>, on_stop: impl FnOnce()) -> std::process::ExitStatus {
    on_stop();
    kill_group(pid);
    let _ = child.start_kill();
    match child.wait().await {
        Ok(s) => s,
        Err(_) => exit_status_none(),
    }
}

#[cfg(unix)]
fn exit_status_none() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(9)
}
#[cfg(not(unix))]
fn exit_status_none() -> std::process::ExitStatus {
    std::process::Command::new("cmd").status().expect("status")
}
