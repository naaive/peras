//! macOS seatbelt (`sandbox-exec`) sandbox. Profile generation is portable and
//! unit-tested everywhere; running requires macOS.
//!
//! - **Network**: seatbelt cannot filter by remote host name, only by
//!   address. A run with `spec.network` starts a per-run [`EgressProxy`] on
//!   loopback (allowlisting exactly `spec.network`) and the profile allows
//!   outbound connections to that one port only; the command finds it through
//!   the `*_PROXY` variables. Without `spec.network` the run is offline.
//! - **Isolation**: copy-based, like the Linux fallback ([`IsolatedCopy`]):
//!   the command runs on a scratch copy of the workspace (APFS clones where
//!   available), writable only there and in a private `TMPDIR`, offline. No
//!   overlayfs is needed.
//!
//! NOT VERIFIED on macOS by the test suite (it runs on Linux): the SBPL
//! `(remote ip "localhost:<port>")` filter and isolated runs under
//! `sandbox-exec`. The profile text is unit-tested.

use super::egress::{proxy_env, EgressProxy};
use super::isolate::{IsolatedCopy, IsolatedRun, Staging};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn quote(p: &Path) -> String {
    // Seatbelt matches canonical paths (/tmp -> /private/tmp).
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{s}\"")
}

/// Generate the SBPL profile for a spec. Reads are allowed everywhere (like
/// the bubblewrap `--ro-bind / /`), writes only to the writable paths. The
/// network is closed except, with `proxy_port`, outbound TCP to the egress
/// proxy on loopback (which enforces the `spec.network` allowlist).
pub fn seatbelt_profile(spec: &SandboxSpec, proxy_port: Option<u16>) -> String {
    let mut s = String::from(
        "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n\
         (allow signal (target same-sandbox))\n(allow sysctl-read)\n(allow mach-lookup)\n\
         (allow ipc-posix-shm)\n(allow file-read*)\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\"))\n",
    );
    if !spec.writable.is_empty() {
        s.push_str("(allow file-write*");
        for w in &spec.writable {
            s.push_str(&format!(" (subpath {})", quote(w)));
        }
        s.push_str(")\n");
    }
    if let Some(port) = proxy_port.filter(|_| !spec.network.is_empty() && !spec.isolated) {
        s.push_str(&format!("(allow network-outbound (remote ip \"localhost:{port}\"))\n(allow system-socket)\n"));
    }
    s
}

#[derive(Debug, Clone, Default)]
pub struct SeatbeltSandbox {
    staging: Arc<Staging>,
}

impl SeatbeltSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn probe_report() -> SandboxReport {
        let ok = cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists();
        SandboxReport {
            implementation: "seatbelt".into(),
            available: ok,
            egress_proxy: ok,
            isolation: ok,
            notes: if ok {
                vec![
                    "network: allowlisted host:ports only, through a loopback egress proxy".into(),
                    "isolated execution is copy-based (workspace copied to a scratch dir)".into(),
                ]
            } else {
                vec!["sandbox-exec not available".into()]
            },
        }
    }

    /// Full command line (`sandbox-exec -p <profile> argv...`).
    pub fn command_line(argv: &[String], spec: &SandboxSpec, proxy_port: Option<u16>) -> Vec<String> {
        let mut v = vec![SANDBOX_EXEC.to_string(), "-p".into(), seatbelt_profile(spec, proxy_port)];
        v.extend(argv.iter().cloned());
        v
    }

    /// Run with copy-based isolation (offline) and keep the copy.
    pub async fn run_isolated(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<IsolatedRun, String> {
        let cwd = spec.cwd.clone();
        let copy = tokio::task::spawn_blocking(move || IsolatedCopy::create(&cwd, None))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("workspace copy: {e}"))?;
        let mut env = spec.env.clone();
        env.retain(|(k, _)| k != "TMPDIR");
        env.push(("TMPDIR".into(), copy.tmp().to_string_lossy().into_owned()));
        let confined = SandboxSpec {
            cwd: copy.path().to_path_buf(),
            writable: vec![copy.path().to_path_buf(), copy.tmp().to_path_buf()],
            network: vec![],
            env,
            isolated: true,
            ..spec.clone()
        };
        let out = self.exec(argv, &confined, None, cancel).await?;
        tokio::task::spawn_blocking(move || IsolatedRun::finish(out, copy)).await.map_err(|e| e.to_string())?
    }

    async fn exec(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        proxy_port: Option<u16>,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        if !cfg!(target_os = "macos") {
            return Err("seatbelt is only available on macOS".into());
        }
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let line = Self::command_line(argv, spec, proxy_port);
        let mut env = super::env_with_path(&spec.env);
        if let Some(port) = proxy_port {
            env.retain(|(k, _)| !k.to_ascii_lowercase().ends_with("_proxy"));
            env.extend(proxy_env(&format!("127.0.0.1:{port}")));
        }
        let mut cmd = tokio::process::Command::new(&line[0]);
        cmd.args(&line[1..]).env_clear().envs(env);
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        super::exec(cmd, spec.timeout_ms, cancel, || {}).await
    }
}

#[async_trait]
impl SandboxPort for SeatbeltSandbox {
    fn report(&self) -> SandboxReport {
        Self::probe_report()
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if spec.isolated {
            return Ok(self.run_isolated(argv, spec, cancel).await?.output);
        }
        // Allowlisted network: a per-run proxy on loopback, the only
        // destination the profile lets the command reach.
        let proxy = if spec.network.is_empty() || !cfg!(target_os = "macos") {
            None
        } else {
            Some(EgressProxy::start(&spec.network).map_err(|e| format!("egress proxy: {e}"))?)
        };
        let port = proxy.as_ref().and_then(|p| p.addr()).map(|a| a.port());
        self.exec(argv, spec, port, cancel).await
    }

    async fn run_staged(
        &self,
        key: &str,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        let run = self.run_isolated(argv, spec, cancel).await?;
        super::isolate::stage_blocking(&self.staging, key, run).await
    }

    fn stage_in(&self, dir: &std::path::Path) {
        self.staging.set_root(dir);
    }

    async fn merge(&self, key: &str, apply: bool) -> Result<Vec<String>, String> {
        let staging = self.staging.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || staging.merge(&key, apply)).await.map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_text() {
        let spec = SandboxSpec {
            writable: vec!["/nonexistent/w \"q\"".into()],
            ..Default::default()
        };
        let p = seatbelt_profile(&spec, None);
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        assert!(p.contains("(allow file-write* (subpath \"/nonexistent/w \\\"q\\\"\"))\n"), "{p}");
        assert!(!p.contains("network"));
        // Network only to the proxy port, never wholesale.
        let net = SandboxSpec { network: vec!["x:1".into()], ..Default::default() };
        let p = seatbelt_profile(&net, Some(40123));
        assert!(p.contains("(allow network-outbound (remote ip \"localhost:40123\"))"), "{p}");
        assert!(!p.contains("(allow network*)"), "{p}");
        assert!(!seatbelt_profile(&net, None).contains("network"), "no proxy: offline");
        let iso = SandboxSpec { isolated: true, ..net };
        assert!(!seatbelt_profile(&iso, Some(1)).contains("network"), "isolated runs are offline");
    }

    #[test]
    fn report_matches_platform() {
        let r = SeatbeltSandbox::probe_report();
        assert_eq!(r.implementation, "seatbelt");
        if !cfg!(target_os = "macos") {
            assert!(!r.available && !r.isolation && !r.egress_proxy);
        }
    }

    #[tokio::test]
    async fn runs_only_on_macos() {
        if cfg!(target_os = "macos") {
            return;
        }
        let s = SeatbeltSandbox::new();
        let e = s.run(&["true".into()], &SandboxSpec::default(), CancellationToken::new()).await.unwrap_err();
        assert!(e.contains("only available on macOS"), "{e}");
        assert!(s.merge("k", true).await.unwrap_err().contains("no longer available"));
    }
}
