//! Framework-launched disposable environment (docker / podman).
//!
//! Each run copies the workspace (`spec.cwd`) into a scratch directory
//! ([`IsolatedCopy`]), mounts that copy at `/workspace` in a fresh `--rm`
//! container and discards it afterwards. What reaches the workspace:
//!
//! - plain runs ([`SandboxPort::run`]): changes inside the declared writable
//!   paths are written back (they were authorized before the run); anything
//!   else is discarded and listed in `overlay_changes`;
//! - staged runs ([`SandboxPort::run_staged`], Opaque commands): nothing until
//!   the change list is reviewed and [`SandboxPort::merge`]d, like any other
//!   isolated run.
//!
//! [`Container::is_disposable`] is always true: the environment may vouch for
//! commands executed inside it (invariant-level asks when unattended), never
//! for host-side tools or for merging changes back into the workspace.
//!
//! The container always runs with `--network none`. When `spec.network` is
//! non-empty and the `agent-netbridge` helper is available, a per-run egress
//! proxy listens on a unix socket mounted into the container and the command
//! runs under the bridge (mounted read-only as the entrypoint), so only
//! allowlisted `host:port`s are reachable. The helper is a host binary: the
//! image must be able to execute it (same architecture, compatible libc).

use super::egress::{find_netbridge, proxy_env, EgressProxy, BRIDGE_LISTEN};
use super::exec;
use super::isolate::{apply_isolated_changes, IsolatedCopy, IsolatedRun, Staging};
use super::which;
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_IMAGE: &str = "docker.io/library/debian:stable-slim";
pub const MOUNT: &str = "/workspace";
/// Where the original `.git/objects` is mounted (read-only).
pub const GIT_OBJECTS: &str = "/.agent/git-objects";
const EGRESS_DIR: &str = "/.agent/egress";
const BRIDGE: &str = "/.agent/netbridge";
const EGRESS_SOCK: &str = "egress.sock";

#[derive(Debug, Clone)]
pub struct Container {
    runtime: Option<PathBuf>,
    image: String,
    bridge: Option<PathBuf>,
    staging: Arc<Staging>,
}

/// Extra mounts for one container run.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerMounts<'a> {
    /// Original `.git/objects` (mounted read-only at [`GIT_OBJECTS`]).
    pub git_objects: Option<&'a Path>,
    /// Egress proxy socket directory and bridge helper.
    pub egress: Option<(&'a Path, &'a Path)>,
}

static RUN_N: AtomicU64 = AtomicU64::new(0);

impl Container {
    /// Detect docker (preferred) or podman in PATH; default image.
    pub fn ephemeral() -> Self {
        Container {
            runtime: which("docker").or_else(|| which("podman")),
            image: DEFAULT_IMAGE.into(),
            bridge: find_netbridge(),
            staging: Arc::default(),
        }
    }
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }
    pub fn runtime(mut self, bin: impl Into<PathBuf>) -> Self {
        self.runtime = Some(bin.into());
        self
    }
    /// The `agent-netbridge` helper used for allowlisted network (`None` =
    /// always offline).
    pub fn bridge(mut self, bin: Option<PathBuf>) -> Self {
        self.bridge = bin;
        self
    }
    /// Everything inside is thrown away after each run.
    pub fn is_disposable(&self) -> bool {
        true
    }

    /// Check that the runtime answers (daemon running / podman usable).
    pub fn runtime_ready(&self) -> Result<(), String> {
        let rt = self.runtime.as_ref().ok_or("no docker/podman in PATH")?;
        let out = std::process::Command::new(rt)
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("runtime not ready").to_string())
        }
    }

    /// `run` arguments (after the runtime binary).
    pub fn args(
        &self,
        name: &str,
        argv: &[String],
        spec: &SandboxSpec,
        copy: &Path,
        m: ContainerMounts<'_>,
    ) -> Vec<String> {
        let mut a = vec!["run".to_string(), "--rm".into(), "--name".into(), name.into(), "--init".into()];
        a.extend(["--network".into(), "none".into()]);
        a.extend(["-v".into(), format!("{}:{MOUNT}", copy.display()), "-w".into(), MOUNT.into()]);
        if let Some(g) = m.git_objects {
            a.extend(["-v".into(), format!("{}:{GIT_OBJECTS}:ro", g.display())]);
        }
        let mut env = spec.env.clone();
        if let Some((dir, bridge)) = m.egress {
            a.extend([
                "-v".into(),
                format!("{}:{EGRESS_DIR}", dir.display()),
                "-v".into(),
                format!("{}:{BRIDGE}:ro", bridge.display()),
                "--entrypoint".into(),
                BRIDGE.into(),
            ]);
            env.retain(|(k, _)| !k.to_ascii_lowercase().ends_with("_proxy"));
            env.extend(proxy_env(BRIDGE_LISTEN));
        }
        for (k, v) in &env {
            a.extend(["-e".into(), format!("{k}={v}")]);
        }
        a.push(self.image.clone());
        if m.egress.is_some() {
            a.extend([
                "--listen".into(),
                BRIDGE_LISTEN.into(),
                "--unix".into(),
                format!("{EGRESS_DIR}/{EGRESS_SOCK}"),
                "--".into(),
            ]);
        }
        a.extend(argv.iter().cloned());
        a
    }

    /// Run on a fresh workspace copy and keep the copy for merging.
    pub async fn run_isolated(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<IsolatedRun, String> {
        let rt = self.runtime.clone().ok_or("no container runtime (docker/podman) found")?;
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let cwd = spec.cwd.clone();
        let copy = tokio::task::spawn_blocking(move || -> std::io::Result<IsolatedCopy> {
            if cwd.as_os_str().is_empty() {
                let empty = tempfile::tempdir()?;
                IsolatedCopy::create(empty.path(), None)
            } else {
                IsolatedCopy::create(&cwd, Some(Path::new(GIT_OBJECTS)))
            }
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("workspace copy: {e}"))?;
        let objects = copy.workspace().join(".git/objects");
        let git_objects = objects.is_dir().then_some(objects.as_path());
        // Isolated runs are offline; plain runs may use the egress proxy.
        let mut _proxy = None;
        let egress_dir = copy.scratch().join("egress");
        let egress = match (&self.bridge, spec.isolated || spec.network.is_empty()) {
            (Some(b), false) => {
                std::fs::create_dir_all(&egress_dir).map_err(|e| e.to_string())?;
                _proxy = Some(
                    EgressProxy::start_unix(&egress_dir.join(EGRESS_SOCK), &spec.network)
                        .map_err(|e| format!("egress proxy: {e}"))?,
                );
                Some((egress_dir.as_path(), b.as_path()))
            }
            _ => None,
        };
        let name = format!("agent-{}-{}", std::process::id(), RUN_N.fetch_add(1, Ordering::Relaxed));
        let mut cmd = tokio::process::Command::new(&rt);
        cmd.args(self.args(&name, argv, spec, copy.path(), ContainerMounts { git_objects, egress }));
        let (rt2, name2) = (rt.clone(), name.clone());
        let out = exec(cmd, spec.timeout_ms, cancel, move || {
            let _ = std::process::Command::new(&rt2).args(["kill", &name2]).output();
        })
        .await?;
        tokio::task::spawn_blocking(move || IsolatedRun::finish(out, copy)).await.map_err(|e| e.to_string())?
    }
}

#[async_trait]
impl SandboxPort for Container {
    fn report(&self) -> SandboxReport {
        let mut notes = vec![match &self.runtime {
            Some(r) => format!("disposable container via {} ({})", r.display(), self.image),
            None => "no docker/podman in PATH".into(),
        }];
        match &self.bridge {
            Some(b) => notes.push(format!(
                "--network none; allowlisted host:ports via the egress proxy ({} must run in the image)",
                b.display()
            )),
            None => notes.push("--network none (no agent-netbridge helper: always offline)".into()),
        }
        SandboxReport {
            implementation: "container".into(),
            available: self.runtime.is_some(),
            egress_proxy: self.runtime.is_some() && self.bridge.is_some(),
            isolation: true,
            notes,
        }
    }

    /// Plain runs write back what the command changed inside the declared
    /// writable paths (those writes were authorized before the run); other
    /// changes are discarded and listed in `overlay_changes`. Isolated runs
    /// write nothing back (see [`SandboxPort::run_staged`]).
    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        let run = self.run_isolated(argv, spec, cancel).await?;
        if spec.isolated {
            return Ok(run.output);
        }
        tokio::task::spawn_blocking({
            let writable = spec.writable.clone();
            move || write_back(run, &writable)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn run_staged(
        &self,
        key: &str,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        let spec = SandboxSpec { isolated: true, ..spec.clone() };
        let run = self.run_isolated(argv, &spec, cancel).await?;
        Ok(self.staging.stage(key, run))
    }

    async fn merge(&self, key: &str, apply: bool) -> Result<Vec<String>, String> {
        let staging = self.staging.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || staging.merge(&key, apply)).await.map_err(|e| e.to_string())?
    }
}

/// Apply the changes of a container run that fall inside `writable`; the
/// rest stay in `overlay_changes` (discarded with the copy).
fn write_back(run: IsolatedRun, writable: &[PathBuf]) -> Result<ExecOutput, String> {
    let IsolatedRun { mut output, changes, copy } = run;
    let ws = copy.workspace().to_path_buf();
    let inside = |rel: &str| {
        let p = ws.join(rel);
        writable.iter().any(|w| {
            let w = std::fs::canonicalize(w).unwrap_or_else(|_| w.clone());
            p.starts_with(&w)
        })
    };
    let (keep, drop): (Vec<_>, Vec<_>) = changes.into_iter().partition(|c| inside(&c.path));
    if !keep.is_empty() {
        apply_isolated_changes(copy.path(), &ws, &keep).map_err(|e| format!("writing back: {e}"))?;
    }
    output.overlay_changes = drop.into_iter().map(|c| c.path).collect();
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_runs_write_back_declared_writes_only() {
        let ws = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a"), "1").unwrap();
        std::fs::write(root.join("b"), "1").unwrap();
        let copy = IsolatedCopy::create(&root, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(copy.path().join("src/a"), "2").unwrap();
        std::fs::write(copy.path().join("src/new"), "n").unwrap();
        std::fs::write(copy.path().join("b"), "2").unwrap();
        let run = IsolatedRun::finish(ExecOutput { status: Some(0), ..Default::default() }, copy).unwrap();
        let out = write_back(run, &[root.join("src")]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("src/a")).unwrap(), "2");
        assert_eq!(std::fs::read_to_string(root.join("src/new")).unwrap(), "n");
        assert_eq!(std::fs::read_to_string(root.join("b")).unwrap(), "1", "undeclared write discarded");
        assert_eq!(out.overlay_changes, vec!["b"]);
    }

    #[test]
    fn args_and_disposable() {
        let c = Container::ephemeral().image("img");
        assert!(c.is_disposable());
        let spec = SandboxSpec { env: vec![("K".into(), "V".into())], ..Default::default() };
        let a = c.args("n", &["make".into()], &spec, Path::new("/tmp/c"), ContainerMounts::default()).join(" ");
        assert_eq!(a, "run --rm --name n --init --network none -v /tmp/c:/workspace -w /workspace -e K=V img make");
        let m = ContainerMounts {
            git_objects: Some(Path::new("/w/.git/objects")),
            egress: Some((Path::new("/s"), Path::new("/b/nb"))),
        };
        let spec = SandboxSpec { network: vec!["h:443".into()], ..spec };
        let a = c.args("n", &["curl".into()], &spec, Path::new("/tmp/c"), m).join(" ");
        assert!(a.contains("--network none"), "{a}");
        assert!(a.contains("-v /w/.git/objects:/.agent/git-objects:ro"), "{a}");
        assert!(a.contains("-v /s:/.agent/egress -v /b/nb:/.agent/netbridge:ro --entrypoint /.agent/netbridge"), "{a}");
        assert!(a.contains("-e HTTPS_PROXY=http://127.0.0.1:3128"), "{a}");
        assert!(a.ends_with("img --listen 127.0.0.1:3128 --unix /.agent/egress/egress.sock -- curl"), "{a}");
    }
}
