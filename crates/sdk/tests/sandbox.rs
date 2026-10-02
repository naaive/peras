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

#[agent::test]
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

#[agent::test]
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

#[agent::test]
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

/// The platform sandbox when it supports isolated execution (landlock or
/// bubblewrap); `None` skips the test.
fn isolating_sandbox() -> Option<Arc<dyn SandboxPort>> {
    let s = agent::adapters::detect();
    let r = s.report();
    if r.available && r.isolation {
        Some(s)
    } else {
        eprintln!("no sandbox with isolated execution ({}); skipping", r.implementation);
        None
    }
}

/// Wraps a shared sandbox port (`Agent::sandbox` takes it by value).
struct Shared(Arc<dyn SandboxPort>);

#[async_trait]
impl SandboxPort for Shared {
    fn report(&self) -> SandboxReport {
        self.0.report()
    }
    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        self.0.run(argv, spec, cancel).await
    }
    async fn run_staged(
        &self,
        key: &str,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        self.0.run_staged(key, argv, spec, cancel).await
    }
    async fn merge(&self, key: &str, apply: bool) -> Result<Vec<String>, String> {
        self.0.merge(key, apply).await
    }
}

/// Runs an Opaque command (variable expansion) in `dir` and answers the
/// review of its changes with `answer`, after `before_answer` ran. `None`:
/// skipped (no isolating sandbox).
async fn opaque_run(
    dir: &std::path::Path,
    answer: impl Fn(&Ask),
    before_answer: impl Fn(&std::path::Path),
) -> Option<(Vec<agent::proto::Question>, String)> {
    let s = isolating_sandbox()?;
    let cmd = "X=new; echo $X > out.txt; rm README.md";
    let model = Script::new().call(Bash, json!({ "command": cmd })).say("ran");
    let agent = Agent::new(model).workspace(dir).tools((Bash,)).sandbox(Shared(s)).without_checkpoints();
    let mut run = agent.run("run it");
    let (mut asks, mut result) = (vec![], String::new());
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => {
                // Asked after the command ran: nothing reached the workspace yet.
                assert!(!dir.join("out.txt").exists());
                assert!(dir.join("README.md").exists());
                before_answer(dir);
                asks.push(a.question.clone());
                answer(&a);
            }
            Update::Tool { result: r, .. } => result = format!("{:?}", r.content),
            _ => {}
        }
    }
    Some((asks, result))
}

#[agent::test]
async fn opaque_command_runs_isolated_then_its_diff_is_approved() {
    let d = ws();
    let Some((asks, result)) = opaque_run(d.path(), |a| a.allow(), |_| {}).await else { return };
    // One ask, about the change list; none before running.
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(!asks[0].rules.contains(&"invariant:unknown_effect".to_string()), "{asks:?}");
    assert!(asks[0].prompt.contains("README.md, out.txt"), "{}", asks[0].prompt);
    assert_eq!(std::fs::read_to_string(d.path().join("out.txt")).unwrap(), "new\n");
    assert!(!d.path().join("README.md").exists());
    assert!(result.contains("changes applied: README.md, out.txt"), "{result}");
}

#[agent::test]
async fn denied_diff_never_reaches_the_workspace() {
    let d = ws();
    let Some((asks, result)) = opaque_run(d.path(), |a| a.deny("no"), |_| {}).await else { return };
    assert_eq!(asks.len(), 1);
    assert!(!d.path().join("out.txt").exists());
    assert_eq!(std::fs::read_to_string(d.path().join("README.md")).unwrap(), "hello foo\n");
    assert!(result.contains("changes discarded (no)"), "{result}");
}

#[agent::test]
async fn approved_diff_over_a_concurrent_edit_is_not_applied() {
    let d = ws();
    let user_edit = |dir: &std::path::Path| std::fs::write(dir.join("README.md"), "edited by the user\n").unwrap();
    let Some((_, result)) = opaque_run(d.path(), |a| a.allow(), user_edit).await else { return };
    assert_eq!(std::fs::read_to_string(d.path().join("README.md")).unwrap(), "edited by the user\n");
    assert!(!d.path().join("out.txt").exists(), "nothing applied");
    assert!(result.contains("changes NOT applied") && result.contains("README.md"), "{result}");
}

/// Records where it was told to keep staged runs.
#[derive(Default, Clone)]
struct StagingProbe(Arc<Mutex<Option<std::path::PathBuf>>>);

#[async_trait]
impl SandboxPort for StagingProbe {
    fn report(&self) -> SandboxReport {
        SandboxReport { implementation: "probe".into(), available: true, isolation: true, ..Default::default() }
    }
    async fn run(&self, _argv: &[String], _spec: &SandboxSpec, _cancel: CancellationToken) -> Result<ExecOutput, String> {
        Ok(ExecOutput::default())
    }
    fn stage_in(&self, dir: &std::path::Path) {
        *self.0.lock().unwrap() = Some(dir.to_path_buf());
    }
}

#[agent::test]
async fn staged_runs_are_kept_in_the_data_directory() {
    let probe = StagingProbe::default();
    let agent = Agent::new(Script::new().say("ok")).tools((Bash,)).sandbox(probe.clone());
    agent.check().await.unwrap();
    let data = agent::tools::testing::scope().unwrap().data_dir;
    assert_eq!(probe.0.lock().unwrap().clone(), Some(data.join("staged")), "durable across restarts, keyed by call");
}
