//! Configuration-driven extensions end to end: long-term memory at session
//! start, subdirectory instructions on access, slash commands, hot reload,
//! observers and hooks (model, sub-agent and MCP executors) from settings.

use agent::prelude::*;
use agent::proto::{Envelope, Event};
use agent::runtime::MemoryStore;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A trusted project directory and a CLI-layer settings file (`extra` appended).
fn project(extra: &str) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    std::fs::create_dir_all(d.path().join(".agent")).unwrap();
    std::fs::write(d.path().join("README.md"), "hello\n").unwrap();
    let policy = d.path().join("cli.toml");
    std::fs::write(&policy, format!("[security]\nworkspace_trusted = true\n{extra}")).unwrap();
    (d, policy)
}

fn discover(dir: &Path, policy: &Path, model: Script) -> Agent {
    Agent::discover(dir).model(model).policy(policy).without_checkpoints()
}

async fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn user_texts(events: &[Envelope<Event>]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[agent::test]
async fn long_term_memory_is_loaded_at_session_start() {
    let d = tempfile::tempdir().unwrap();
    let mem = agent::tools::testing::MemMemory::default();
    mem.remember("project", "style", "tabs, not spaces").await.unwrap();
    let model = Script::new().say("ok");
    let agent = Agent::new(model.clone()).workspace(d.path()).memory(mem);
    let chat = agent.session("mem");
    assert_eq!(chat.send("hi").await.unwrap(), "ok");
    let events = chat.events().await.unwrap();
    let loaded = events.iter().find(|e| matches!(e.body, Event::MemoryLoaded { .. })).expect("memory loaded");
    let first_turn = events.iter().position(|e| matches!(e.body, Event::TurnStarted { .. })).unwrap();
    assert!(loaded.seq < first_turn as u64, "at session start, before the first turn");
    let req = serde_json::to_string(&model.requests()[0].body).unwrap();
    assert!(req.contains("project/style: tabs, not spaces"));
    assert_eq!(events.iter().filter(|e| matches!(e.body, Event::MemoryLoaded { .. })).count(), 1);
}

#[agent::test]
async fn subdirectory_instructions_are_injected_on_access() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("sub")).unwrap();
    std::fs::write(d.path().join("sub/AGENTS.md"), "In sub/, use tabs.").unwrap();
    std::fs::write(d.path().join("sub/x.rs"), "fn x() {}").unwrap();
    let model = Script::new().call(read, json!({ "file": "sub/x.rs" })).say("ok");
    let agent = Agent::new(model.clone()).workspace(d.path()).tools((read,)).instructions_on_access(true);
    let chat = agent.session("instr");
    assert_eq!(chat.send("read it").await.unwrap(), "ok");
    let events = chat.events().await.unwrap();
    let injected: Vec<&String> = events
        .iter()
        .filter_map(|e| match &e.body {
            Event::InstructionsInjected { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(injected.len(), 1);
    assert!(injected[0].ends_with("sub/AGENTS.md"));
    let last = serde_json::to_string(&model.requests().last().unwrap().body).unwrap();
    assert!(last.contains("In sub/, use tabs."));
}

#[agent::test]
async fn slash_commands_expand_into_their_template() {
    let (d, policy) = project("");
    std::fs::create_dir_all(d.path().join(".agent/commands")).unwrap();
    std::fs::write(d.path().join(".agent/commands/review.md"), "---\ndescription: Review code\n---\nReview $ARGUMENTS carefully.").unwrap();
    let agent = discover(d.path(), &policy, Script::new().say("reviewed").say("plain"));
    let cmds = agent.commands().await.unwrap();
    assert_eq!(cmds[0].name, "review");
    let chat = agent.session("slash");
    assert_eq!(chat.send("/review src/lib.rs").await.unwrap(), "reviewed");
    assert_eq!(chat.send("/nope stays as typed").await.unwrap(), "plain");
    let texts = user_texts(&chat.events().await.unwrap());
    assert_eq!(texts, vec!["Review src/lib.rs carefully.".to_string(), "/nope stays as typed".to_string()]);
}

#[agent::test]
async fn configuration_changes_reconfigure_live_sessions() {
    let (d, policy) = project("");
    let agent = discover(d.path(), &policy, Script::new().say("one").say("two")).hot_reload();
    let chat = agent.session("reload");
    assert_eq!(chat.send("hi").await.unwrap(), "one");
    std::fs::write(d.path().join(".agent/settings.toml"), "system = [\"Always answer in French.\"]\n").unwrap();
    let rt = agent.runtime().await.unwrap();
    let h = rt.session(chat.id()).unwrap();
    let changed = wait_for(|| {
        h.with_state(|s| agent::kernel::config(s).is_some_and(|c| c.system.iter().any(|x| x.contains("French"))))
    })
    .await;
    assert!(changed, "the idle session took the new configuration");
    let events = chat.events().await.unwrap();
    assert!(events.iter().any(|e| matches!(e.body, Event::ConfigChanged { .. })));
    assert!(agent.profile().await.unwrap().kernel.system.iter().any(|x| x.contains("French")), "new sessions too");
    assert_eq!(chat.send("again").await.unwrap(), "two");
}

#[agent::test]
async fn configured_observer_sees_events_and_can_only_signal() {
    let out = tempfile::tempdir().unwrap();
    let seen = out.path().join("seen.json");
    let script = format!(
        "cat > {}; echo '{{\"signal\":{{\"kind\":\"notify\",\"source\":\"ci\",\"key\":\"ci\",\"text\":\"build is red\"}}}}'",
        seen.display()
    );
    let extra = format!(
        "[[observers]]\nname = \"ci\"\nevents = [\"turn_ended\"]\nexecutor = {{ command = \"sh\", args = [\"-c\", {}] }}\n",
        toml::Value::String(script)
    );
    let (d, policy) = project(&extra);
    let agent = discover(d.path(), &policy, Script::new().say("one").say("two"));
    let chat = agent.session("observed");
    assert_eq!(chat.send("hi").await.unwrap(), "one");
    assert!(wait_for(|| std::fs::read_to_string(&seen).is_ok_and(|s| s.contains("turn_ended"))).await);
    let rt = agent.runtime().await.unwrap();
    let journal = rt.env().journal.clone();
    let id = chat.id().clone();
    let mut queued = false;
    for _ in 0..500 {
        let evs = journal.load(&id, 0).await.unwrap();
        if evs.iter().any(|e| serde_json::to_string(&e.body).unwrap().contains("build is red")) {
            queued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(queued, "the observer's signal reached the session");
    assert_eq!(chat.send("next").await.unwrap(), "two");
    let events = chat.events().await.unwrap();
    assert!(events.iter().any(|e| matches!(&e.body, Event::Injected { text, .. } if text == "build is red")));
}

fn denied_text(events: &[Envelope<Event>]) -> Option<String> {
    events.iter().find_map(|e| match &e.body {
        Event::ToolResulted { result, .. } if result.is_error => Some(format!("{:?}", result.content)),
        _ => None,
    })
}

#[agent::test]
async fn model_call_hook_judges_tool_calls() {
    let extra = "[[hooks]]\npoint = \"pre_tool\"\nmatcher = \"read\"\nexecutor = { prompt = \"Deny reads of README.\" }\n";
    let (d, policy) = project(extra);
    let model = Script::new()
        .call(read, json!({ "file": "README.md" }))
        .say("{\"decision\": \"deny\", \"reason\": \"judged unsafe\"}")
        .say("ok");
    let agent = discover(d.path(), &policy, model.clone());
    let chat = agent.session("judged");
    assert_eq!(chat.send("read").await.unwrap(), "ok");
    assert!(denied_text(&chat.events().await.unwrap()).unwrap().contains("judged unsafe"));
    let judge = serde_json::to_string(&model.requests()[1].body).unwrap();
    assert!(judge.contains("Deny reads of README.") && judge.contains("\\\"pre_tool\\\""), "{judge}");
    assert_hook_spend_charged(&agent, &chat).await;
}

/// The judging hook's consumption is journaled and charged to the session.
async fn assert_hook_spend_charged(agent: &Agent, chat: &Chat) {
    let events = chat.events().await.unwrap();
    let charged: u64 = events
        .iter()
        .filter_map(|e| match &e.body {
            Event::UsageCharged { source, spend } if source == "hooks:pre_tool" => Some(spend.tokens),
            _ => None,
        })
        .sum();
    assert!(charged > 0, "hook spend journaled");
    let own: u64 = events
        .iter()
        .filter_map(|e| match &e.body {
            Event::AssistantReplied { message, .. } => Some(agent::proto::Spend::of(&message.usage).tokens),
            _ => None,
        })
        .sum();
    let h = agent.runtime().await.unwrap().session(chat.id()).unwrap();
    assert_eq!(h.with_state(agent::kernel::usage).0, own + charged, "charged to the session's budget");
}

#[agent::test]
async fn subagent_hook_judges_tool_calls() {
    let extra = "[[hooks]]\npoint = \"pre_tool\"\nmatcher = \"read\"\nexecutor = { agent = \"judge\" }\n";
    let (d, policy) = project(extra);
    std::fs::create_dir_all(d.path().join(".agent/agents")).unwrap();
    std::fs::write(d.path().join(".agent/agents/judge.md"), "---\ntools: read\n---\nYou judge tool calls.").unwrap();
    let model = Script::new()
        .call(read, json!({ "file": "README.md" }))
        .say("{\"decision\": \"deny\", \"reason\": \"the judge said no\"}")
        .say("ok");
    let agent = discover(d.path(), &policy, model);
    let chat = agent.session("judged-by-agent");
    assert_eq!(chat.send("read").await.unwrap(), "ok");
    assert!(denied_text(&chat.events().await.unwrap()).unwrap().contains("the judge said no"));
    assert_hook_spend_charged(&agent, &chat).await;
}

/// A minimal Streamable HTTP MCP server with one tool, `check`, answering a
/// deny verdict.
async fn guard_server() -> String {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut s = BufReader::new(s);
                let mut len = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    if s.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let l = line.trim_end().to_ascii_lowercase();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(v) = l.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                s.read_exact(&mut body).await.unwrap();
                let msg: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let result = match msg["method"].as_str() {
                    Some("initialize") => json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": { "name": "guard" } }),
                    Some("tools/list") => json!({ "tools": [{ "name": "check", "inputSchema": { "type": "object" } }] }),
                    Some("tools/call") => {
                        assert_eq!(msg["params"]["arguments"]["point"], "pre_tool");
                        json!({ "content": [{ "type": "text", "text": "{\"decision\":\"deny\",\"reason\":\"mcp guard says no\"}" }] })
                    }
                    _ => {
                        let _ = s.get_mut().write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                        return;
                    }
                };
                let out = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }).to_string();
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}", out.len());
                let _ = s.get_mut().write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[agent::test]
async fn mcp_hook_and_remote_mcp_tools() {
    let url = guard_server().await;
    let extra = format!(
        "[mcp.guard]\nurl = \"{url}\"\n[[hooks]]\npoint = \"pre_tool\"\nmatcher = \"read\"\nexecutor = {{ mcp = \"guard/check\" }}\n"
    );
    let (d, policy) = project(&extra);
    let model = Script::new().call(read, json!({ "file": "README.md" })).say("ok");
    let agent = discover(d.path(), &policy, model);
    let profile = agent.profile().await.unwrap();
    assert!(profile.kernel.tools.iter().any(|t| t.name == "mcp__guard__check"), "remote MCP tools are registered");
    let chat = agent.session("mcp-guarded");
    assert_eq!(chat.send("read").await.unwrap(), "ok");
    assert!(denied_text(&chat.events().await.unwrap()).unwrap().contains("mcp guard says no"));
}

#[agent::test]
async fn the_user_layer_comes_from_the_tests_own_home() {
    // Inside `#[agent::test]` the user layer (`~/.agent`) is the test's own
    // empty home, never the developer's: what is planted there is read...
    let home = agent::tools::testing::scope().unwrap().home;
    assert_ne!(Some(home.clone()), std::env::var_os("HOME").map(PathBuf::from));
    std::fs::create_dir_all(home.join(".agent")).unwrap();
    std::fs::write(home.join(".agent/settings.toml"), "system = [\"From the user layer.\"]\n").unwrap();
    let (d, policy) = project("");
    let p = discover(d.path(), &policy, Script::new()).profile().await.unwrap();
    assert!(p.kernel.system.iter().any(|s| s == "From the user layer."), "{:?}", p.kernel.system);
    // ...and an explicit home replaces it (`None`: no user layer at all).
    let other = tempfile::tempdir().unwrap();
    let p = discover(d.path(), &policy, Script::new()).home(Some(other.path())).profile().await.unwrap();
    assert!(!p.kernel.system.iter().any(|s| s == "From the user layer."));
    let p = discover(d.path(), &policy, Script::new()).home(None::<&Path>).profile().await.unwrap();
    assert!(!p.kernel.system.iter().any(|s| s == "From the user layer."));
}
