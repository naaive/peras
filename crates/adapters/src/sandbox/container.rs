//! Framework-launched disposable environment (docker / podman).
//!
//! Each run copies the workspace (`spec.cwd`) into a scratch directory,
//! mounts that copy at `/workspace` in a fresh `--rm` container and discards
//! it afterwards. Nothing is written back: the files the command changed are
//! reported in `overlay_changes`. [`Container::is_disposable`] is always true,
//! which lets the SDK treat everything inside as reversible.

use super::{exec, which};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_IMAGE: &str = "docker.io/library/debian:stable-slim";
pub const MOUNT: &str = "/workspace";

#[derive(Debug, Clone)]
pub struct Container {
    runtime: Option<PathBuf>,
    image: String,
}

static RUN_N: AtomicU64 = AtomicU64::new(0);

impl Container {
    /// Detect docker (preferred) or podman in PATH; default image.
    pub fn ephemeral() -> Self {
        Container { runtime: which("docker").or_else(|| which("podman")), image: DEFAULT_IMAGE.into() }
    }
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }
    pub fn runtime(mut self, bin: impl Into<PathBuf>) -> Self {
        self.runtime = Some(bin.into());
        self
    }
    /// Everything inside is thrown away after each run.
    pub fn is_disposable(&self) -> bool {
        true
    }

    /// `run` arguments (after the runtime binary).
    pub fn args(&self, name: &str, argv: &[String], spec: &SandboxSpec, copy: &Path) -> Vec<String> {
        let mut a = vec!["run".to_string(), "--rm".into(), "--name".into(), name.into(), "--init".into()];
        if spec.network.is_empty() {
            a.extend(["--network".into(), "none".into()]);
        }
        a.extend(["-v".into(), format!("{}:{MOUNT}", copy.display()), "-w".into(), MOUNT.into()]);
        for (k, v) in &spec.env {
            a.extend(["-e".into(), format!("{k}={v}")]);
        }
        a.push(self.image.clone());
        a.extend(argv.iter().cloned());
        a
    }
}

fn hash_tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(root, &p, out);
            } else if ft.is_file() {
                if let (Ok(rel), Ok(b)) = (p.strip_prefix(root), std::fs::read(&p)) {
                    out.insert(rel.to_string_lossy().into_owned(), hex::encode(Sha256::digest(&b)));
                }
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let ft = e.file_type()?;
        let dst = to.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &dst)?;
        } else if ft.is_file() {
            std::fs::copy(e.path(), &dst)?;
        }
    }
    Ok(())
}

/// Workspace-relative paths that differ between two tree hashes.
pub(crate) fn diff_trees(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let mut v: Vec<String> = after.iter().filter(|(k, h)| before.get(*k) != Some(h)).map(|(k, _)| k.clone()).collect();
    v.extend(before.keys().filter(|k| !after.contains_key(*k)).cloned());
    v.sort();
    v
}

#[async_trait]
impl SandboxPort for Container {
    fn report(&self) -> SandboxReport {
        SandboxReport {
            implementation: "container".into(),
            available: self.runtime.is_some(),
            egress_proxy: false,
            isolation: true,
            notes: vec![match &self.runtime {
                Some(r) => format!("disposable container via {} ({})", r.display(), self.image),
                None => "no docker/podman in PATH".into(),
            }],
        }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        let rt = self.runtime.clone().ok_or("no container runtime (docker/podman) found")?;
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        let copy = scratch.path().join("ws");
        let cwd = spec.cwd.clone();
        let copy2 = copy.clone();
        let before = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            if cwd.as_os_str().is_empty() {
                std::fs::create_dir_all(&copy2)?;
            } else {
                copy_tree(&cwd, &copy2)?;
            }
            Ok(hash_tree(&copy2))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("workspace copy: {e}"))?;
        let name = format!("agent-{}-{}", std::process::id(), RUN_N.fetch_add(1, Ordering::Relaxed));
        let mut cmd = tokio::process::Command::new(&rt);
        cmd.args(self.args(&name, argv, spec, &copy));
        let (rt2, name2) = (rt.clone(), name.clone());
        let mut out = exec(cmd, spec.timeout_ms, cancel, move || {
            let _ = std::process::Command::new(&rt2).args(["kill", &name2]).output();
        })
        .await?;
        let copy3 = copy.clone();
        let after = tokio::task::spawn_blocking(move || hash_tree(&copy3)).await.map_err(|e| e.to_string())?;
        out.overlay_changes = diff_trees(&before, &after);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_and_disposable() {
        let c = Container::ephemeral().image("img");
        assert!(c.is_disposable());
        let spec = SandboxSpec { env: vec![("K".into(), "V".into())], ..Default::default() };
        let a = c.args("n", &["make".into()], &spec, Path::new("/tmp/c")).join(" ");
        assert_eq!(a, "run --rm --name n --init --network none -v /tmp/c:/workspace -w /workspace -e K=V img make");
    }

    #[test]
    fn tree_diff() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "1").unwrap();
        std::fs::write(d.path().join("b"), "1").unwrap();
        let before = hash_tree(d.path());
        std::fs::write(d.path().join("a"), "2").unwrap();
        std::fs::remove_file(d.path().join("b")).unwrap();
        std::fs::write(d.path().join("c"), "1").unwrap();
        assert_eq!(diff_trees(&before, &hash_tree(d.path())), vec!["a", "b", "c"]);
    }
}
