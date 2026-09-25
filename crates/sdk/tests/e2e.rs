//! End-to-end: scripted model -> real kernel -> runtime -> real tools.

use agent::prelude::*;
use std::sync::{Arc, Mutex};

fn allow_all(p: &Proposal) -> Verdict {
    let _ = p;
    Verdict::Allow
}

fn ws() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("README.md"), "hello foo\n").unwrap();
    d
}

/// The design doc's example test.
#[agent::test]
async fn edits_readme() -> anyhow::Result<()> {
    let dir = agent::tools::testing::workspace();
    std::fs::write(dir.join("README.md"), "hello foo\n")?;
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "bar" }))
        .say("Done");
    let out = Agent::new(model)
        .workspace(&dir)
        .tools((edit,))
        .allow("**")
        .run("Edit README")
        .await?;
    assert_eq!(out, "Done");
    assert_eq!(std::fs::read_to_string(dir.join("README.md"))?, "hello bar\n");
    Ok(())
}

#[tokio::test]
async fn stream_updates_and_ask_answered_from_code() {
    let d = ws();
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "baz" }))
        .say("ok");
    let agent = Agent::new(model).workspace(d.path()).tools((edit, read));
    let mut run = agent.run("edit it");
    let mut asked = 0;
    let mut tools = 0;
    let mut done = None;
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => {
                asked += 1;
                a.allow();
            }
            Update::Tool { result, .. } => {
                tools += 1;
                assert!(!result.is_error, "{result:?}");
            }
            Update::Done(o) => done = Some(o),
            _ => {}
        }
    }
    assert!(asked >= 1, "a write with no matching allow rule must ask");
    assert_eq!(tools, 1);
    assert_eq!(done, Some(TurnOutcome::Done { text: "ok".into() }));
    assert_eq!(std::fs::read_to_string(d.path().join("README.md")).unwrap(), "hello baz\n");
}

#[tokio::test]
async fn awaited_run_denies_unhandled_asks() {
    let d = ws();
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "qux" }))
        .say("tried");
    let out = Agent::new(model).workspace(d.path()).tools((edit,)).run("edit").await.unwrap();
    assert_eq!(out, "tried");
    assert_eq!(std::fs::read_to_string(d.path().join("README.md")).unwrap(), "hello foo\n");
}

#[tokio::test]
async fn code_gate_denies_and_observer_sees_failure() {
    let d = ws();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "zzz" }))
        .say("blocked");
    let out = Agent::new(model)
        .workspace(d.path())
        .tools((edit,))
        .gate(|p: &Proposal| if p.writes("README.md") { Verdict::deny("hands off README") } else { Verdict::Allow })
        .observe(move |e: &ToolFailed| s2.lock().unwrap().push(e.to_string()))
        .run("edit")
        .await
        .unwrap();
    assert_eq!(out, "blocked");
    assert_eq!(std::fs::read_to_string(d.path().join("README.md")).unwrap(), "hello foo\n");
    // Observers are asynchronous: give the cursor task a moment.
    for _ in 0..50 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|s| s.contains("hands off README")), "{seen:?}");
}

#[tokio::test]
async fn json_output() {
    #[derive(serde::Deserialize, schemars::JsonSchema, Debug, PartialEq)]
    struct Plan {
        tasks: Vec<String>,
    }
    let d = ws();
    let model = Script::new().say(r#"{"tasks":["a","b"]}"#);
    let plan: Plan = Agent::new(model).workspace(d.path()).run("Break it into tasks").json().await.unwrap();
    assert_eq!(plan, Plan { tasks: vec!["a".into(), "b".into()] });
}

#[tokio::test]
async fn chat_session_persists_across_turns() {
    let d = ws();
    let model = Script::new().say("one").say("two");
    let agent = Agent::new(model).workspace(d.path());
    let chat = agent.session("pr-1234");
    assert_eq!(chat.send("first").await.unwrap(), "one");
    assert_eq!(chat.send("second").await.unwrap(), "two");
    let events = chat.events().await.unwrap();
    let users = events.iter().filter(|e| matches!(e.body, agent::proto::Event::UserMessage { .. })).count();
    assert_eq!(users, 2);
}

#[tokio::test]
async fn subagent_is_a_tool() {
    let d = ws();
    let child_model = Script::new().say("looks good");
    let reviewer = Agent::new(child_model).workspace(d.path()).tools((read,)).named("reviewer").describe("Review the diff");
    let lead_model = Script::new().call("reviewer", json!({ "task": "review the README" })).say("reviewed");
    let lead = Agent::new(lead_model)
        .workspace(d.path())
        .tools((read, reviewer))
        .gate(allow_all)
        .auto_answer(agent::runtime::FnRule::new("allow-reviewer", |_r, _q| {
            Some(Answer::Allow { remember: false })
        }));
    let mut run = lead.run("review");
    let mut tool_text = String::new();
    let mut out = None;
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => a.allow(),
            Update::Tool { result, .. } => tool_text = format!("{:?}", result.content),
            Update::Done(o) => out = Some(o),
            _ => {}
        }
    }
    assert!(tool_text.contains("looks good"), "{tool_text}");
    assert_eq!(out, Some(TurnOutcome::Done { text: "reviewed".into() }));
}
