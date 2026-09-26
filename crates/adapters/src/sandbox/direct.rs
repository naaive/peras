//! No isolation: runs commands directly (with a clean environment, the
//! timeout and cancellation still enforced). Reports `available = false`.

use super::{env_with_path, exec};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct DirectExec {
    notes: Vec<String>,
}

impl DirectExec {
    pub fn new(notes: Vec<String>) -> Self {
        DirectExec { notes }
    }
}

#[async_trait]
impl SandboxPort for DirectExec {
    fn report(&self) -> SandboxReport {
        let mut notes = self.notes.clone();
        notes.push("direct execution: no filesystem or network isolation".into());
        SandboxReport { implementation: "none".into(), available: false, egress_proxy: false, isolation: false, notes }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if spec.isolated {
            return Err("isolated execution is not available without a sandbox".into());
        }
        let (bin, args) = argv.split_first().ok_or("empty argv")?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args).env_clear().envs(env_with_path(&spec.env));
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        exec(cmd, spec.timeout_ms, cancel, || {}).await
    }
}
