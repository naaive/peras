//! bubblewrap sandbox (Linux, unprivileged user namespaces).

use super::egress::{find_netbridge, proxy_env, EgressProxy, BRIDGE_LISTEN, NETBRIDGE_BIN};
use super::isolate::{IsolatedCopy, IsolatedRun};
use super::{env_with_path, exec, trial, which};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct BwrapSandbox {
    bwrap: PathBuf,
    overlay: bool,
    bridge: Option<PathBuf>,
    notes: Vec<String>,
}

/// Overlay directories for an isolated run.
#[derive(Debug, Clone)]
pub struct OverlayDirs {
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// How the workspace (`spec.cwd`) is mounted.
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceMount<'a> {
    /// Writable paths bound read-write (normal runs).
    Direct,
    /// Kernel overlay on the workspace (bwrap >= 0.10).
    Overlay(&'a OverlayDirs),
    /// A scratch copy bound at the workspace path; `git_objects` is the
    /// original `.git/objects`, bound read-only at [`GIT_OBJECTS_ALIAS`].
    Copy { copy: &'a Path, git_objects: Option<&'a Path> },
}

/// Egress wiring: the proxy socket directory and the bridge helper.
#[derive(Debug, Clone, Copy)]
pub struct EgressMount<'a> {
    pub socket_dir: &'a Path,
    pub bridge: &'a Path,
}

/// Where the original `.git/objects` appears inside isolated sandboxes.
pub const GIT_OBJECTS_ALIAS: &str = "/tmp/.agent-git-objects";
const EGRESS_DIR: &str = "/tmp/.agent-egress";
const BRIDGE_PATH: &str = "/tmp/.agent-netbridge";
const EGRESS_SOCK: &str = "egress.sock";

impl BwrapSandbox {
    /// Use a specific binary without probing (capabilities assumed).
    pub fn new(bwrap: impl Into<PathBuf>, overlay: bool) -> Self {
        BwrapSandbox { bwrap: bwrap.into(), overlay, bridge: None, notes: vec![] }
    }

    /// Route allowlisted network through the egress proxy using this
    /// `agent-netbridge` binary (found automatically by [`BwrapSandbox::probe`]).
    pub fn with_bridge(mut self, bridge: impl Into<PathBuf>) -> Self {
        self.bridge = Some(bridge.into());
        self
    }

    /// Find `bwrap` and check that it can actually create a sandbox with the
    /// flags we use. `Err` carries the reason (for the report notes).
    pub fn probe() -> Result<Self, String> {
        let bwrap = which("bwrap").ok_or("bubblewrap (bwrap) not found in PATH")?;
        let base = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-pid", "--unshare-net"];
        let mut args: Vec<&str> = base.to_vec();
        args.push("true");
        trial(&bwrap, &args)
            .map_err(|e| format!("bwrap cannot create a sandbox (unprivileged user namespaces restricted?): {e}"))?;
        let mut notes = vec![];
        let overlay = match tempfile::tempdir() {
            Ok(d) => {
                let p = d.path().to_string_lossy().to_string();
                let r = trial(&bwrap, &["--ro-bind", "/", "/", "--overlay-src", &p, "--tmp-overlay", &p, "true"]);
                if let Err(e) = &r {
                    let e = e.lines().next().unwrap_or_default();
                    notes.push(format!("overlay isolation unavailable ({e}); using copy-based isolation"));
                }
                r.is_ok()
            }
            Err(e) => {
                notes.push(format!("overlay probe failed ({e}); using copy-based isolation"));
                false
            }
        };
        let bridge = find_netbridge();
        match &bridge {
            Some(b) => notes.push(format!(
                "network: private namespace; allowlisted host:ports reachable only via the egress proxy ({})",
                b.display()
            )),
            None => notes.push(format!("{NETBRIDGE_BIN} helper not found: sandbox is always offline")),
        }
        Ok(BwrapSandbox { bwrap, overlay, bridge, notes })
    }

    /// Compile a spec into the bwrap command line (without the binary).
    /// The network is always unshared; with `egress` the command runs under
    /// the bridge, which forwards loopback `BRIDGE_LISTEN` to the proxy.
    pub fn args(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        ws: WorkspaceMount<'_>,
        egress: Option<EgressMount<'_>>,
    ) -> Vec<String> {
        let s = |x: &str| x.to_string();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        let mut a: Vec<String> = vec![
            s("--die-with-parent"),
            s("--new-session"),
            s("--unshare-pid"),
            s("--unshare-ipc"),
            s("--unshare-uts"),
            s("--unshare-net"),
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
        // Readable paths under /tmp would be hidden by the tmpfs: re-bind.
        for r in &spec.readable {
            a.extend([s("--ro-bind-try"), p(r), p(r)]);
        }
        match ws {
            WorkspaceMount::Overlay(o) => {
                // The workspace (cwd) becomes an overlay; other writable
                // paths stay read-only in isolated runs.
                a.extend([s("--overlay-src"), p(&spec.cwd), s("--overlay"), p(&o.upper), p(&o.work), p(&spec.cwd)]);
            }
            WorkspaceMount::Copy { copy, git_objects } => {
                a.extend([s("--bind"), p(copy), p(&spec.cwd)]);
                if let Some(g) = git_objects {
                    a.extend([s("--ro-bind"), p(g), s(GIT_OBJECTS_ALIAS)]);
                }
            }
            WorkspaceMount::Direct => {
                for w in &spec.writable {
                    a.extend([s("--bind-try"), p(w), p(w)]);
                }
            }
        }
        let mut env = env_with_path(&spec.env);
        if let Some(e) = egress {
            a.extend([s("--ro-bind"), p(e.socket_dir), s(EGRESS_DIR), s("--ro-bind"), p(e.bridge), s(BRIDGE_PATH)]);
            env.retain(|(k, _)| !k.to_ascii_lowercase().ends_with("_proxy"));
            env.extend(proxy_env(BRIDGE_LISTEN));
        }
        a.push(s("--clearenv"));
        for (k, v) in env {
            a.extend([s("--setenv"), k, v]);
        }
        if !spec.cwd.as_os_str().is_empty() {
            a.extend([s("--chdir"), p(&spec.cwd)]);
        }
        a.push(s("--"));
        if egress.is_some() {
            a.extend([
                s(BRIDGE_PATH),
                s("--listen"),
                s(BRIDGE_LISTEN),
                s("--unix"),
                format!("{EGRESS_DIR}/{EGRESS_SOCK}"),
                s("--"),
            ]);
        }
        a.extend(argv.iter().cloned());
        a
    }

    /// Run with copy-based isolation (offline) and keep the copy.
    pub async fn run_isolated(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<IsolatedRun, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let cwd = spec.cwd.clone();
        let copy = tokio::task::spawn_blocking(move || IsolatedCopy::create(&cwd, Some(Path::new(GIT_OBJECTS_ALIAS))))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("workspace copy: {e}"))?;
        let objects = copy.workspace().join(".git/objects");
        let git_objects = objects.is_dir().then_some(objects.as_path());
        let ws = WorkspaceMount::Copy { copy: copy.path(), git_objects };
        let mut cmd = tokio::process::Command::new(&self.bwrap);
        cmd.args(self.args(argv, spec, ws, None)).env_clear();
        let out = exec(cmd, spec.timeout_ms, cancel, || {}).await?;
        tokio::task::spawn_blocking(move || IsolatedRun::finish(out, copy)).await.map_err(|e| e.to_string())?
    }
}

/// Files changed in an overlay upper dir (workspace-relative). Whiteouts
/// (deletions) are character devices.
fn overlay_changes(upper: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
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
            egress_proxy: self.bridge.is_some(),
            // Overlay when bwrap supports it, else copy-based.
            isolation: true,
            notes: self.notes.clone(),
        }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        if spec.isolated {
            // Isolated runs are always offline.
            if !self.overlay {
                return Ok(self.run_isolated(argv, spec, cancel).await?.output);
            }
            let d = tempfile::tempdir().map_err(|e| e.to_string())?;
            let o = OverlayDirs { upper: d.path().join("upper"), work: d.path().join("work") };
            std::fs::create_dir_all(&o.upper).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&o.work).map_err(|e| e.to_string())?;
            let mut cmd = tokio::process::Command::new(&self.bwrap);
            cmd.args(self.args(argv, spec, WorkspaceMount::Overlay(&o), None)).env_clear();
            let mut out = exec(cmd, spec.timeout_ms, cancel, || {}).await?;
            // Timed-out isolated runs discard their changes.
            out.overlay_changes = if out.timed_out { vec![] } else { overlay_changes(&o.upper) };
            return Ok(out);
        }
        // Allowlisted network: private netns + bridge to a per-run proxy.
        let mut proxy_dir = None;
        let mut _proxy = None;
        if let (false, Some(_)) = (spec.network.is_empty(), &self.bridge) {
            let d = tempfile::Builder::new().prefix("agent-egress-").tempdir().map_err(|e| e.to_string())?;
            _proxy = Some(
                EgressProxy::start_unix(&d.path().join(EGRESS_SOCK), &spec.network)
                    .map_err(|e| format!("egress proxy: {e}"))?,
            );
            proxy_dir = Some(d);
        }
        let egress = match (&proxy_dir, &self.bridge) {
            (Some(d), Some(b)) => Some(EgressMount { socket_dir: d.path(), bridge: b }),
            _ => None,
        };
        let mut cmd = tokio::process::Command::new(&self.bwrap);
        cmd.args(self.args(argv, spec, WorkspaceMount::Direct, egress)).env_clear();
        exec(cmd, spec.timeout_ms, cancel, || {}).await
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
        let a = b.args(&["ls".into(), "-l".into()], &spec, WorkspaceMount::Direct, None).join(" ");
        assert!(a.starts_with("--die-with-parent "), "{a}");
        assert!(a.contains("--ro-bind / / "));
        assert!(a.contains("--unshare-net"));
        assert!(a.contains("--bind-try /w /w"));
        assert!(a.contains("--clearenv --setenv A 1 --setenv PATH"));
        assert!(a.ends_with("--chdir /w -- ls -l"), "{a}");
        let spec = SandboxSpec { network: vec!["proxy:3128".into()], ..spec };
        let od = OverlayDirs { upper: "/o/u".into(), work: "/o/w".into() };
        let a = b.args(&["true".into()], &spec, WorkspaceMount::Overlay(&od), None).join(" ");
        assert!(a.contains("--unshare-net"), "network is always unshared");
        assert!(a.contains("--overlay-src /w --overlay /o/u /o/w /w"));
        assert!(!a.contains("--bind-try /w"));
        let ws = WorkspaceMount::Copy { copy: Path::new("/c"), git_objects: Some(Path::new("/w/.git/objects")) };
        let a = b.args(&["true".into()], &spec, ws, None).join(" ");
        assert!(a.contains("--bind /c /w --ro-bind /w/.git/objects /tmp/.agent-git-objects"), "{a}");
        let eg = EgressMount { socket_dir: Path::new("/s"), bridge: Path::new("/b/agent-netbridge") };
        let a = b.args(&["curl".into()], &spec, WorkspaceMount::Direct, Some(eg)).join(" ");
        assert!(
            a.contains("--ro-bind /s /tmp/.agent-egress --ro-bind /b/agent-netbridge /tmp/.agent-netbridge"),
            "{a}"
        );
        assert!(a.contains("--setenv HTTPS_PROXY http://127.0.0.1:3128"), "{a}");
        assert!(
            a.ends_with(
                "-- /tmp/.agent-netbridge --listen 127.0.0.1:3128 --unix /tmp/.agent-egress/egress.sock -- curl"
            ),
            "{a}"
        );
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
