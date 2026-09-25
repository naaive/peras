//! macOS seatbelt (`sandbox-exec`) sandbox. Profile generation is portable and
//! unit-tested everywhere; running requires macOS.

use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn quote(p: &Path) -> String {
    // Seatbelt matches canonical paths (/tmp -> /private/tmp).
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{s}\"")
}

/// Generate the SBPL profile for a spec. Reads are allowed everywhere (like
/// the bubblewrap `--ro-bind / /`), writes only to the writable paths, and
/// network only when the spec lists endpoints.
pub fn seatbelt_profile(spec: &SandboxSpec) -> String {
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
    if !spec.network.is_empty() {
        // Seatbelt cannot filter by remote host; endpoints are enforced by
        // the egress proxy, so network is all-or-nothing here.
        s.push_str("(allow network*)\n(allow system-socket)\n");
    }
    s
}

#[derive(Debug, Clone, Default)]
pub struct SeatbeltSandbox;

impl SeatbeltSandbox {
    pub fn new() -> Self {
        SeatbeltSandbox
    }

    pub fn probe_report() -> SandboxReport {
        let ok = cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists();
        SandboxReport {
            implementation: "seatbelt".into(),
            available: ok,
            egress_proxy: false,
            isolation: false,
            notes: if ok {
                vec!["no overlay isolation on macOS: Opaque commands need approval first".into()]
            } else {
                vec!["sandbox-exec not available".into()]
            },
        }
    }

    /// Full command line (`sandbox-exec -p <profile> argv...`).
    pub fn command_line(argv: &[String], spec: &SandboxSpec) -> Vec<String> {
        let mut v = vec![SANDBOX_EXEC.to_string(), "-p".into(), seatbelt_profile(spec)];
        v.extend(argv.iter().cloned());
        v
    }
}

#[async_trait]
impl SandboxPort for SeatbeltSandbox {
    fn report(&self) -> SandboxReport {
        Self::probe_report()
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if !cfg!(target_os = "macos") {
            return Err("seatbelt is only available on macOS".into());
        }
        if spec.isolated {
            return Err("isolated execution is not supported on macOS".into());
        }
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let line = Self::command_line(argv, spec);
        let mut cmd = tokio::process::Command::new(&line[0]);
        cmd.args(&line[1..]).env_clear().envs(super::env_with_path(&spec.env));
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        super::exec(cmd, spec.timeout_ms, cancel, || {}).await
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
        let p = seatbelt_profile(&spec);
        assert!(p.starts_with("(version 1)\n(deny default)\n"));
        assert!(p.contains("(allow file-write* (subpath \"/nonexistent/w \\\"q\\\"\"))\n"), "{p}");
        assert!(!p.contains("network"));
        let p = seatbelt_profile(&SandboxSpec { network: vec!["x:1".into()], ..Default::default() });
        assert!(p.contains("(allow network*)"));
    }
}
