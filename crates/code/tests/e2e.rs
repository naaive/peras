//! The coding agent end to end with a scripted model: its tools, permission
//! modes, built-in definitions, print mode and the REPL.

use agent::prelude::*;
use agent::proto::{Event, SessionId};
use agent_code::mode::PermissionMode;
use agent_code::print::OutputFormat;
use agent_code::repl::{Input, Repl};
use agent_code::{Coding, Options};
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn workspace() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "foo one\nfoo two\n").unwrap();
    d
}

fn options(ws: &tempfile::TempDir, model: Script) -> Options {
    let mut o = Options::new(ws.path());
    o.port = Some(Arc::new(model));
    o.home = Some(None);
    o
}

/// Drive a run; `allow` answers every question.
async fn drive(mut run: Run, allow: bool) -> (Option<TurnOutcome>, usize) {
    let mut out = None;
    let mut asked = 0;
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => {
                asked += 1;
                if allow {
                    a.allow()
                } else {
                    a.deny("no")
                }
            }
            Update::Done(o) => out = Some(o),
            _ => {}
        }
    }
    (out, asked)
}

fn done(t: &str) -> Option<TurnOutcome> {
    Some(TurnOutcome::Done { text: t.into() })
}

async fn results(c: &Coding, id: &SessionId) -> Vec<(String, bool, String)> {
    let events = c.agent.runtime().await.unwrap().env().journal.load(id, 0).await.unwrap();
    events
        .into_iter()
        .filter_map(|e| match e.body {
            Event::ToolResulted { call, result } => Some((call.name.clone(), result.is_error, agent_code::render::result_text(&result))),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn edit_replace_all_and_multi_edit() {
    let ws = workspace();
    let model = Script::new()
        .call("edit", json!({"file": "a.txt", "old": "foo", "new": "bar"}))
        .call("edit", json!({"file": "a.txt", "old": "foo", "new": "bar", "replace_all": true}))
        .call("multi_edit", json!({"file": "a.txt", "edits": [
            {"old": "bar one", "new": "first"},
            {"old": "bar two", "new": "second"}
        ]}))
        .call("multi_edit", json!({"file": "a.txt", "edits": [
            {"old": "first", "new": "1st"},
            {"old": "missing", "new": "x"}
        ]}))
        .say("done");
    let mut o = options(&ws, model);
    o.mode = PermissionMode::AcceptEdits;
    let c = o.build();
    let run = c.agent.run("rename foo");
    let id = run.session_id().clone();
    assert_eq!(drive(run, false).await, (done("done"), 0), "accept-edits approves workspace edits");
    assert_eq!(std::fs::read_to_string(ws.path().join("a.txt")).unwrap(), "first\nsecond\n", "the failed multi_edit wrote nothing");
    let r = results(&c, &id).await;
    assert!(r[0].1 && r[0].2.contains("occurs 2 times"), "{r:?}");
    assert_eq!((r[1].1, r[1].2.as_str()), (false, "Edited a.txt (2 replacements)"));
    assert!(!r[2].1);
    assert!(r[3].1 && r[3].2.contains("edit 2 of 2"), "{r:?}");
}

#[tokio::test]
async fn plan_mode_refuses_changes_until_the_plan_is_approved() {
    let ws = workspace();
    let model = Script::new()
        .call("read", json!({"file": "a.txt"}))
        .call("write", json!({"file": "b.txt", "content": "x"}))
        .call("exit_plan_mode", json!({"plan": "1. write b.txt"}))
        .call("write", json!({"file": "b.txt", "content": "x"}))
        .say("implemented");
    let mut o = options(&ws, model);
    o.mode = PermissionMode::Plan;
    let c = o.build();
    let run = c.agent.run("add b.txt");
    let id = run.session_id().clone();
    let (outcome, asked) = drive(run, true).await;
    assert_eq!(outcome, done("implemented"));
    assert_eq!(asked, 2, "the plan, then the write (default mode after the plan)");
    let r = results(&c, &id).await;
    assert!(!r[0].1, "reading is allowed in plan mode");
    assert!(r[1].1 && r[1].2.contains("Plan mode is active"), "{r:?}");
    assert!(r[2].2.contains("approved the plan"), "{r:?}");
    assert!(!r[3].1);
    assert_eq!(c.permissions.mode(), PermissionMode::Default);
    assert!(ws.path().join("b.txt").exists());
}

#[tokio::test]
async fn a_rejected_plan_keeps_plan_mode() {
    let ws = workspace();
    let model = Script::new().call("exit_plan_mode", json!({"plan": "do it"})).say("revising");
    let mut o = options(&ws, model);
    o.mode = PermissionMode::Plan;
    let c = o.build();
    assert_eq!(drive(c.agent.run("plan"), false).await, (done("revising"), 1));
    assert_eq!(c.permissions.mode(), PermissionMode::Plan);
}

#[tokio::test]
async fn allowed_tools_answer_their_questions() {
    let ws = workspace();
    let model = Script::new()
        .call("write", json!({"file": "w.txt", "content": "w"}))
        .call("edit", json!({"file": "a.txt", "old": "foo one", "new": "x"}))
        .say("ok");
    let mut o = options(&ws, model);
    o.allowed_tools = vec!["write".into()];
    let c = o.build();
    let run = c.agent.run("go");
    let id = run.session_id().clone();
    assert_eq!(drive(run, false).await, (done("ok"), 1), "only the edit asked");
    let r = results(&c, &id).await;
    assert!(!r[0].1 && r[1].1, "{r:?}");
    assert!(ws.path().join("w.txt").exists());
}

#[tokio::test]
async fn disallowed_tools_are_denied_by_policy() {
    let ws = workspace();
    let model = Script::new().call("write", json!({"file": "w.txt", "content": "w"})).say("ok");
    let mut o = options(&ws, model);
    o.disallowed_tools = vec!["write".into()];
    o.mode = PermissionMode::BypassPermissions;
    let c = o.build();
    let run = c.agent.run("go");
    let id = run.session_id().clone();
    assert_eq!(drive(run, true).await, (done("ok"), 0));
    let r = results(&c, &id).await;
    assert!(r[0].1 && r[0].2.contains("denied by policy"), "{r:?}");
    assert!(!ws.path().join("w.txt").exists());
}

#[tokio::test]
async fn todos_ls_and_the_tool_set() {
    let ws = workspace();
    std::fs::create_dir(ws.path().join("src")).unwrap();
    let model = Script::new()
        .call("todo_write", json!({"todos": [
            {"content": "Look around", "status": "in_progress"},
            {"content": "Report", "status": "pending"}
        ]}))
        .call("ls", json!({"dir": "."}))
        .say("ok");
    let c = options(&ws, model).build();
    let run = c.agent.run("go");
    let id = run.session_id().clone();
    assert_eq!(drive(run, false).await, (done("ok"), 0));
    assert_eq!(c.todos.get(&id).len(), 2);
    let r = results(&c, &id).await;
    assert_eq!(r[1].2, ".agent/\nsrc/\na.txt", "{r:?}");

    let rt = c.agent.runtime().await.unwrap();
    let names: Vec<String> = rt.tools().specs().into_iter().map(|s| s.name).collect();
    for t in ["read", "write", "edit", "multi_edit", "ls", "glob", "grep", "bash", "web_fetch", "todo_write", "exit_plan_mode", "task_output", "explore", "plan", "general-purpose"] {
        assert!(names.contains(&t.to_string()), "missing {t}: {names:?}");
    }
    let edit_spec = rt.tools().specs().into_iter().find(|s| s.name == "edit").unwrap();
    assert!(edit_spec.input_schema.to_string().contains("replace_all"), "the coding edit replaces the built-in");
}

#[tokio::test]
async fn builtin_definitions_yield_to_project_files() {
    let ws = workspace();
    std::fs::create_dir_all(ws.path().join(".agent/commands")).unwrap();
    std::fs::write(ws.path().join(".agent/commands/review.md"), "---\ndescription: house review\n---\nReview per HOUSE rules: $ARGUMENTS").unwrap();
    let script = Arc::new(Script::new().say("hi"));
    let mut o = options(&ws, Script::new());
    o.port = Some(script.clone());
    o.trusted = true;
    let c = o.build();
    let p = c.agent.profile().await.unwrap();
    let agents: Vec<&str> = p.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(agents, ["explore", "general-purpose", "plan"]);
    let explore = p.agent("explore").unwrap();
    assert_eq!(explore.tools.as_deref().unwrap(), ["read", "grep", "glob", "ls", "web_fetch"]);
    assert!(p.expand_slash("/review src").unwrap().starts_with("Review per HOUSE rules: src"));
    assert!(p.expand_slash("/init").unwrap().contains("AGENTS.md"));
    assert!(p.kernel.security.workspace_trusted);
    assert_eq!(c.agent.run("hello").await.unwrap(), "hi");
    let system = script.requests()[0].body["system"].to_string();
    assert!(system.contains("You are Peras"), "the coding prompt replaces the base prompt: {system}");
    assert!(system.contains("Working directory:"), "{system}");
    assert!(!system.contains("You are a coding agent working in the user's workspace"), "{system}");
}

#[tokio::test]
async fn print_mode_json_and_history() {
    let ws = workspace();
    let c = options(&ws, Script::new().say("The answer.")).build();
    let mut out = Vec::new();
    let r = agent_code::print::run(&c, None, "question?".into(), OutputFormat::Json, &mut out).await;
    assert_eq!(r.exit_code(), 0);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["result"], "The answer.");
    assert_eq!(v["subtype"], "success");
    assert_eq!(v["session_id"], r.session.as_str());
    let h = c.history().list();
    assert_eq!((h.len(), h[0].title.as_str()), (1, "question?"));
}

#[tokio::test]
async fn print_mode_denies_open_questions() {
    let ws = workspace();
    let model = Script::new().call("write", json!({"file": "w.txt", "content": "w"})).say("could not");
    let mut o = options(&ws, model);
    o.unattended = Some(OnAsk::Deny);
    let c = o.build();
    let mut out = Vec::new();
    let r = agent_code::print::run(&c, None, "write".into(), OutputFormat::StreamJson, &mut out).await;
    assert_eq!(r.text(), "could not");
    assert!(!ws.path().join("w.txt").exists());
    let lines: Vec<serde_json::Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.first().unwrap()["type"], "system");
    assert!(lines.iter().any(|l| l["type"] == "tool_result"));
    assert_eq!(lines.last().unwrap()["type"], "result");
}

// ------------------------------------------------------------------ REPL

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

async fn repl(c: Coding, lines: &[&str], first: Option<&str>) -> String {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ready_tx, mut ready) = mpsc::unbounded_channel();
    let (_itx, irx) = mpsc::unbounded_channel();
    let buf = Buf::default();
    let r = Repl::new(c, None, rx, irx, Box::new(buf.clone())).on_ready(ready_tx);
    // One line each time the REPL waits for the user, then the end of input.
    let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let feeder = tokio::spawn(async move {
        for l in lines {
            if ready.recv().await.is_none() {
                return;
            }
            let _ = tx.send(Input::Line(l));
        }
        if ready.recv().await.is_some() {
            let _ = tx.send(Input::Eof);
        }
    });
    if tokio::time::timeout(std::time::Duration::from_secs(20), r.run(first.map(String::from))).await.is_err() {
        panic!("the REPL did not finish; output:\n{}", buf.text());
    }
    feeder.abort();
    buf.text()
}

#[tokio::test]
async fn repl_conversation_and_commands() {
    let ws = workspace();
    let model = Script::new()
        .call("todo_write", json!({"todos": [{"content": "Say hi", "status": "completed"}]}))
        .say("Hi there");
    let c = options(&ws, model).build();
    let out = repl(c.clone(), &["hello", "/todos", "/cost", "/mode plan", "/status", "/bogus", "/exit"], None).await;
    assert!(out.contains("Hi there"), "{out}");
    assert!(out.contains("● todo_write"), "{out}");
    assert!(out.contains("☒ Say hi"), "{out}");
    assert!(out.contains("Total cost:"), "{out}");
    assert!(out.contains("Permission mode: plan"), "{out}");
    assert!(out.contains("Mode:       plan"), "{out}");
    assert!(out.contains("Unknown command /bogus"), "{out}");
    assert!(out.contains("Resume this conversation with: peras --resume"), "{out}");
    assert_eq!(c.history().list().len(), 1);
}

#[tokio::test]
async fn repl_approval_with_dont_ask_again_switches_to_accept_edits() {
    let ws = workspace();
    let model = Script::new()
        .call("write", json!({"file": "n.txt", "content": "1"}))
        .call("write", json!({"file": "m.txt", "content": "2"}))
        .say("written");
    let c = options(&ws, model).build();
    let out = repl(c.clone(), &["2", "/exit"], Some("write files")).await;
    assert!(out.contains("Permission required"), "{out}");
    assert!(out.contains("don't ask again for file edits"), "{out}");
    assert!(out.contains("written"), "{out}");
    assert_eq!(out.matches("Permission required").count(), 1, "the second write was approved by the mode: {out}");
    assert_eq!(c.permissions.mode(), PermissionMode::AcceptEdits);
    assert!(ws.path().join("n.txt").exists() && ws.path().join("m.txt").exists());
}

#[tokio::test]
async fn repl_denial_carries_the_users_instruction() {
    let ws = workspace();
    let model = Script::new().call("write", json!({"file": "n.txt", "content": "1"})).say("ok, not writing");
    let c = options(&ws, model).build();
    let out = repl(c.clone(), &["3", "use a different name", "/exit"], Some("write it")).await;
    assert!(out.contains("ok, not writing"), "{out}");
    assert!(!ws.path().join("n.txt").exists());
    let id = SessionId::new(c.history().last().unwrap().id);
    let r = results(&c, &id).await;
    assert!(r[0].1 && r[0].2.contains("use a different name"), "{r:?}");
}

#[tokio::test]
async fn repl_shell_escape_and_notes() {
    let ws = workspace();
    let model = Script::new().say("seen");
    let c = options(&ws, model).build();
    let out = repl(c.clone(), &["!echo from-shell", "#use tabs", "what did I run?", "/exit"], None).await;
    assert!(out.contains("from-shell"), "{out}");
    assert!(std::fs::read_to_string(ws.path().join("AGENTS.md")).unwrap().contains("- use tabs"));
    let id = SessionId::new(c.history().last().unwrap().id);
    let events = c.agent.runtime().await.unwrap().env().journal.load(&id, 0).await.unwrap();
    let first = events.iter().find_map(|e| match &e.body {
        Event::UserMessage { text, .. } => Some(text.clone()),
        _ => None,
    });
    let first = first.unwrap();
    assert!(first.contains("<bash-input>echo from-shell</bash-input>") && first.ends_with("what did I run?"), "{first}");
    let _ = std::io::stdout().flush();
}
