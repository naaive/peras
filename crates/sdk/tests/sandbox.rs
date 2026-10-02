//! Sandbox wiring: the bash tool adapts to the sandbox the agent runs with,
//! and `[sandbox] require` refuses to run without one.

use agent::prelude::*;
use agent::runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Reports an OS sandbox (so commands are classified) but runs them
/// directly, recording every spec.
#[derive(Default, Clone)]
struct FakeSandbox {
    specs: Arc<Mutex<Vec<SandboxSpec>>>,
}

#[async_trait]
impl SandboxPort for FakeSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport { implementation: "fake".into(), available: true, ..Default::default() }
    }
    async fn run(&self, argv: &[String], spec: &SandboxSpec, _cancel: CancellationToken) -> Result<ExecOutput, String> {
        self.specs.lock().unwrap().push(spec.clone());
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&spec.cwd)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(ExecOutput { status: out.status.code(), stdout: out.stdout, stderr: out.stderr, ..Default::default() })
    }
}

fn ws() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("README.md"), "hello foo\n").unwrap();
    d
}

#[tokio::test]
async fn bash_is_classified_when_the_agent_has_a_sandbox() {
    let d = ws();
    let sandbox = FakeSandbox::default();
    let model = Script::new().call(Bash, json!({ "command": "grep -c foo README.md" })).say("counted");
    let agent = Agent::new(model).workspace(d.path()).tools((Bash,)).sandbox(sandbox.clone());
    let mut run = agent.run("count");
    let (mut asks, mut output) = (0, String::new());
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => {
                asks += 1;
                a.deny("no");
            }
            Update::Tool { result, .. } => output = format!("{:?}", result.content),
            _ => {}
        }
    }
    assert_eq!(asks, 0, "a read-only command needs no approval in a sandbox");
    assert!(output.contains("1\\n[exit code 0]"), "{output}");
    // It ran read-only and offline.
    let spec = sandbox.specs.lock().unwrap()[0].clone();
    assert!(spec.writable.is_empty() && spec.network.is_empty(), "{spec:?}");
}

#[tokio::test]
async fn without_a_sandbox_every_command_asks() {
    let d = ws();
    let model = Script::new().call(Bash, json!({ "command": "grep -c foo README.md" })).say("counted");
    let agent = Agent::new(model)
        .workspace(d.path())
        .tools((Bash,))
        .sandbox(agent::adapters::DirectExec::default());
    let mut run = agent.run("count");
    let mut rules = vec![];
    while let Some(u) = run.next().await {
        if let Update::Ask(a) = u {
            rules.extend(a.question.rules.clone());
            a.deny("no");
        }
    }
    assert!(rules.contains(&"invariant:unknown_effect".to_string()), "{rules:?}");
}

#[tokio::test]
async fn require_refuses_to_run_without_a_sandbox() {
    let d = ws();
    let policy = d.path().join("agent.toml");
    std::fs::write(&policy, "[sandbox]\nrequire = true\n").unwrap();
    let agent = Agent::new(Script::new().say("hi"))
        .workspace(d.path())
        .tools((Bash,))
        .policy(&policy)
        .sandbox(agent::adapters::DirectExec::default());
    let e = agent.check().await.unwrap_err();
    assert!(matches!(&e, Error::Config(m) if m.contains("sandbox.require")), "{e}");
    let e = agent.run("hi").await.unwrap_err();
    assert!(e.to_string().contains("sandbox.require"), "{e}");
    // With a sandbox it runs.
    let agent = Agent::new(Script::new().say("hi")).workspace(d.path()).policy(&policy).sandbox(FakeSandbox::default());
    assert_eq!(agent.run("hi").await.unwrap(), "hi");
}
