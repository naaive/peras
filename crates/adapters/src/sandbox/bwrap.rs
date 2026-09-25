//! bubblewrap sandbox (Linux, unprivileged user namespaces).

use super::{env_with_path, exec, trial, which};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct BwrapSandbox {
    bwrap: PathBuf,
    overlay: bool,
    notes: Vec<String>,
}

/// Overlay directories for an isolated run.
#[derive(Debug, Clone)]
pub struct OverlayDirs {
    pub upper: PathBuf,
    pub work: PathBuf,
}

impl BwrapSandbox {
    /// Use a specific binary without probing (capabilities assumed).
    pub fn new(bwrap: impl Into<PathBuf>, overlay: bool) -> Self {
        BwrapSandbox { bwrap: bwrap.into(), overlay, notes: vec![] }
    }

    /// Find `bwrap` and check that it can actually create a sandbox with the
    /// flags we use. `Err` carries the reason (for the report notes).
    pub fn probe() -> Result<Self, String> {
        let bwrap = which("bwrap").ok_or("bubblewrap (bwrap) not found in PATH")?;
        let base = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-pid", "--unshare-net"];
        let mut args: Vec<&str> = base.to_vec();
        args.push("true");
        trial(&bwrap, &args).map_err(|e| {
            format!("bwrap cannot create a sandbox (unprivileged user namespaces restricted?): {e}")
        })?;
        let mut notes = vec![];
        let overlay = match tempfile::tempdir() {
            Ok(d) => {
                let p = d.path().to_string_lossy().to_string();
                let r = trial(&bwrap, &["--ro-bind", "/", "/", "--overlay-src", &p, "--tmp-overlay", &p, "true"]);
                if let Err(e) = &r {
                    notes.push(format!("overlay isolation unavailable: {e}"));
                }
                r.is_ok()
            }
            Err(e) => {
                notes.push(format!("overlay probe failed: {e}"));
                false
            }
        };
        notes.push("network is all-or-nothing (no egress proxy bridge)".into());
        Ok(BwrapSandbox { bwrap, overlay, notes })
    }

    /// Compile a spec into the bwrap command line (without the binary).
    pub fn args(&self, argv: &[String], spec: &SandboxSpec, overlay: Option<&OverlayDirs>) -> Vec<String> {
        let s = |x: &str| x.to_string();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let mut a: Vec<String> = vec![
            s("--die-with-parent"),
            s("--new-session"),
            s("--unshare-pid"),
            s("--unshare-ipc"),
            s("--unshare-uts"),
            s("--ro-bind"),
            s("/"),
            s("/"),
            s("--dev"),
            s("/dev"),
            s("--proc"),
            s("/proc"),
            s("--tmpfs"),
            s("/tmp"),
        ];
        if spec.network.is_empty() {
            a.push(s("--unshare-net"));
        }
        // Readable paths under /tmp would be hidden by the tmpfs: re-bind.
        for r in &spec.readable {
            a.extend([s("--ro-bind-try"), p(r), p(r)]);
        }
        match overlay {
            Some(o) => {
                // The workspace (cwd) becomes an overlay; other writable
                // paths stay read-only in isolated runs.
                a.extend([s("--overlay-src"), p(&spec.cwd), s("--overlay"), p(&o.upper), p(&o.work), p(&spec.cwd)]);
            }
            None => {
                for w in &spec.writable {
                    a.extend([s("--bind-try"), p(w), p(w)]);
                }
            }
        }
        a.push(s("--clearenv"));
        for (k, v) in env_with_path(&spec.env) {
            a.extend([s("--setenv"), k, v]);
        }
        if !spec.cwd.as_os_str().is_empty() {
            a.extend([s("--chdir"), p(&spec.cwd)]);
        }
        a.push(s("--"));
        a.extend(argv.iter().cloned());
        a
    }
}

/// Files changed in an overlay upper dir (workspace-relative). Whiteouts
/// (deletions) are character devices.
fn overlay_changes(upper: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut v = vec![];
    walk(upper, upper, &mut v);
    v.sort();
    v
}

#[async_trait]
impl SandboxPort for BwrapSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport {
            implementation: "bubblewrap".into(),
            available: true,
            egress_proxy: false,
            isolation: self.overlay,
            notes: self.notes.clone(),
        }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let dirs = if spec.isolated {
            if !self.overlay {
                return Err("overlay isolation is not available".into());
            }
            Some(tempfile::tempdir().map_err(|e| e.to_string())?)
        } else {
            None
        };
        let od = dirs.as_ref().map(|d| {
            let o = OverlayDirs { upper: d.path().join("upper"), work: d.path().join("work") };
            let _ = std::fs::create_dir_all(&o.upper);
            let _ = std::fs::create_dir_all(&o.work);
            o
        });
        let mut cmd = tokio::process::Command::new(&self.bwrap);
        cmd.args(self.args(argv, spec, od.as_ref())).env_clear();
        let mut out = exec(cmd, spec.timeout_ms, cancel, || {}).await?;
        if let Some(o) = &od {
            // Timed-out isolated runs discard their changes.
            out.overlay_changes = if out.timed_out { vec![] } else { overlay_changes(&o.upper) };
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_spec() {
        let b = BwrapSandbox::new("/usr/bin/bwrap", true);
        let spec = SandboxSpec {
            cwd: "/w".into(),
            readable: vec!["/r".into()],
            writable: vec!["/w".into()],
            network: vec![],
            env: vec![("A".into(), "1".into())],
            timeout_ms: 0,
            isolated: false,
        };
        let a = b.args(&["ls".into(), "-l".into()], &spec, None).join(" ");
        assert!(a.starts_with("--die-with-parent "), "{a}");
        assert!(a.contains("--ro-bind / / "));
        assert!(a.contains("--unshare-net"));
        assert!(a.contains("--bind-try /w /w"));
        assert!(a.contains("--clearenv --setenv A 1 --setenv PATH"));
        assert!(a.ends_with("--chdir /w -- ls -l"), "{a}");
        let spec = SandboxSpec { network: vec!["proxy:3128".into()], ..spec };
        let od = OverlayDirs { upper: "/o/u".into(), work: "/o/w".into() };
        let a = b.args(&["true".into()], &spec, Some(&od)).join(" ");
        assert!(!a.contains("--unshare-net"));
        assert!(a.contains("--overlay-src /w --overlay /o/u /o/w /w"));
        assert!(!a.contains("--bind-try /w"));
    }

    #[test]
    fn overlay_change_list() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "x").unwrap();
        std::fs::write(d.path().join("b"), "y").unwrap();
        assert_eq!(overlay_changes(d.path()), vec!["b".to_string(), "src/a.rs".to_string()]);
    }
}
