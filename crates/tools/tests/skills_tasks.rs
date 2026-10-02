//! `load_skill` beyond the project directory (user skills, a compiled
//! catalog), the background task tools and background bash commands.

use agent_proto::{Access, ResourceUri, SessionId, Trust};
use agent_runtime::{AccessCtx, MemBlobStore, TaskRegistry, TaskStatus, Tool};
use agent_tools::testing::{call_granted, ctx, text_of};
use agent_tools::{SkillLoader, TaskKill, TaskList, TaskOutput};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn user_skills_and_catalog_entries_load() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let user = home.path().join(".agent/skills");
    std::fs::create_dir_all(user.join("deploy")).unwrap();
    std::fs::write(user.join("deploy/SKILL.md"), "# Deploy (user)").unwrap();
    std::fs::create_dir_all(ws.path().join(".agent/skills/lint")).unwrap();
    std::fs::write(ws.path().join(".agent/skills/lint/SKILL.md"), "# Lint (project)").unwrap();
    let plugin = home.path().join("plugin/skills/review/SKILL.md");
    std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    std::fs::write(&plugin, "# Review (plugin)").unwrap();

    let loader = SkillLoader::new().with_user_dir(Some(user.clone())).with_catalog([("review".to_string(), plugin.clone())]);
    let actx = AccessCtx { workspace: ws.path().to_path_buf() };

    // Project skill: a workspace read, as before.
    let acc = loader.access(&json!({"name": "lint"}), &actx).unwrap();
    assert_eq!(acc, vec![Access::read(ResourceUri::fs(&format!("{}/.agent/skills/lint/SKILL.md", ws.path().display())))]);
    assert_eq!(text_of(&call_granted(&loader, json!({"name": "lint"}), ws.path()).await.unwrap()), "# Lint (project)");

    // User skill: declared as a read of the file outside the workspace.
    let acc = loader.access(&json!({"name": "deploy"}), &actx).unwrap();
    assert_eq!(acc, vec![Access::read(ResourceUri::fs(&user.join("deploy/SKILL.md").display().to_string()))]);
    assert_eq!(text_of(&call_granted(&loader, json!({"name": "deploy"}), ws.path()).await.unwrap()), "# Deploy (user)");
    // Not granted: refused.
    let r = loader.call(json!({"name": "deploy"}), ctx(ws.path(), vec![])).await;
    assert!(r.is_err());

    // Catalog entry (e.g. a plugin's skill).
    assert_eq!(text_of(&call_granted(&loader, json!({"name": "review"}), ws.path()).await.unwrap()), "# Review (plugin)");
    assert!(call_granted(&loader, json!({"name": "missing"}), ws.path()).await.is_err());
}

#[tokio::test]
async fn task_tools_see_only_the_sessions_tasks() {
    let reg = Arc::new(TaskRegistry::new(Arc::new(MemBlobStore::new())));
    let me = SessionId::new("test-session");
    let done = reg.spawn_for(Some(me.clone()), "build", None, |_| async { Ok(b"built ok".to_vec()) });
    reg.set_trust(done, Trust::Untrusted { source: "subagent:x".into() });
    let slow = reg.spawn_for(Some(me.clone()), "watch", None, |c| async move {
        c.cancelled().await;
        Ok(vec![])
    });
    let other = reg.spawn_for(Some(SessionId::new("other")), "theirs", None, |_| async { Ok(b"secret".to_vec()) });
    assert!(matches!(reg.wait(done).await, Some(TaskStatus::Done { .. })));
    let mut c = ctx("/w", vec![]);
    c.tasks = Some(reg.clone());

    let list = text_of(&TaskList.call(json!({}), c.clone()).await.unwrap());
    assert!(list.contains("build") && list.contains("watch") && !list.contains("theirs"), "{list}");
    let out = TaskOutput.call(json!({"id": done}), c.clone()).await.unwrap();
    assert_eq!(text_of(&out), "built ok");
    assert!(out.trust.unwrap().is_untrusted(), "the task's label travels with its output");
    assert!(TaskOutput.call(json!({"id": other}), c.clone()).await.is_err());
    assert!(TaskKill.call(json!({"id": other}), c.clone()).await.is_err());
    TaskKill.call(json!({"id": slow}), c.clone()).await.unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), reg.wait(slow)).await.unwrap(), Some(TaskStatus::Killed));
    // Without a registry: a tool failure, not a crash.
    assert!(TaskList.call(json!({}), ctx("/w", vec![])).await.is_err());
}

/// A `ToolCtx` granting `bash`'s declared accesses, with a task registry.
fn bash_ctx(tool: &agent_tools::BashTool, input: &serde_json::Value, ws: &std::path::Path, reg: &Arc<TaskRegistry>) -> agent_runtime::ToolCtx {
    let grants = tool.access(input, &AccessCtx { workspace: ws.to_path_buf() }).unwrap();
    let mut c = ctx(ws, grants);
    c.tasks = Some(reg.clone());
    c
}

fn started_id(out: &agent_runtime::ToolOutput) -> u64 {
    let t = text_of(out);
    let rest = t.strip_prefix("Started as background task ").unwrap_or_else(|| panic!("{t}"));
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn background_bash_runs_as_a_task_of_the_session() {
    let ws = tempfile::tempdir().unwrap();
    let reg = Arc::new(TaskRegistry::new(Arc::new(MemBlobStore::new())));
    let me = SessionId::new("test-session");
    let bash = agent_tools::BashTool::default();

    // Output (stdout, stderr, exit code) is stored as the task's blob.
    let input = json!({ "command": "sleep 0.2; echo out; echo err >&2; touch made; exit 3", "background": true });
    let out = bash.call(input.clone(), bash_ctx(&bash, &input, ws.path(), &reg)).await.unwrap();
    let id = started_id(&out);
    assert_eq!(reg.get(id).unwrap().owner, Some(me.clone()));
    assert!(!reg.write_scopes().is_empty(), "an Opaque command's writes count as the agent's while it runs");
    let Some(TaskStatus::Done { output: Some(blob) }) = reg.wait(id).await else { panic!("{:?}", reg.get(id)) };
    let text = String::from_utf8(reg.blobs().get(&blob).await.unwrap()).unwrap();
    assert_eq!(text, "out\nerr\n[exit code 3]");
    assert!(ws.path().join("made").exists());
    assert!(reg.write_scopes().is_empty(), "finished");
    let mut c = ctx(ws.path(), vec![]);
    c.tasks = Some(reg.clone());
    assert_eq!(text_of(&TaskOutput.call(json!({ "id": id }), c.clone()).await.unwrap()), text);

    // Killed with task_kill: the command stops (well before it would end).
    let input = json!({ "command": "sleep 30; touch late", "background": true });
    let id = started_id(&bash.call(input.clone(), bash_ctx(&bash, &input, ws.path(), &reg)).await.unwrap());
    let t0 = std::time::Instant::now();
    TaskKill.call(json!({ "id": id }), c.clone()).await.unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(10), reg.wait(id)).await.unwrap(), Some(TaskStatus::Killed));
    assert!(t0.elapsed() < Duration::from_secs(10));

    // Its timeout ends it as timed out.
    let input = json!({ "command": "sleep 30", "background": true, "timeout_ms": 200 });
    let id = started_id(&bash.call(input.clone(), bash_ctx(&bash, &input, ws.path(), &reg)).await.unwrap());
    assert_eq!(tokio::time::timeout(Duration::from_secs(10), reg.wait(id)).await.unwrap(), Some(TaskStatus::TimedOut));
    assert!(!ws.path().join("late").exists());

    // A rewind stops the session's writing tasks.
    let input = json!({ "command": "sleep 30", "background": true });
    let id = started_id(&bash.call(input.clone(), bash_ctx(&bash, &input, ws.path(), &reg)).await.unwrap());
    assert_eq!(reg.kill_writers(&SessionId::new("other")), Vec::<u64>::new());
    assert_eq!(reg.kill_writers(&me), vec![id]);
    assert_eq!(tokio::time::timeout(Duration::from_secs(10), reg.wait(id)).await.unwrap(), Some(TaskStatus::Killed));

    // Without a registry: a tool failure.
    let input = json!({ "command": "true", "background": true });
    assert!(call_granted(&bash, input, ws.path()).await.is_err());
}

#[tokio::test]
async fn background_bash_is_never_isolated() {
    let ws = tempfile::tempdir().unwrap();
    let reg = Arc::new(TaskRegistry::new(Arc::new(MemBlobStore::new())));
    let bash = agent_tools::Bash::new(true).with_isolation(true);
    let fg = json!({ "command": "frobnicate --all" });
    let bg = json!({ "command": "frobnicate --all", "background": true });
    assert!(bash.isolated(&fg), "an Opaque command runs isolated, its diff reviewed after");
    assert!(!bash.isolated(&bg), "in the background it is approved before it runs instead");
    // An isolated call is never turned into a background task.
    let mut c = bash_ctx(&bash, &bg, ws.path(), &reg);
    c.isolated = true;
    let e = bash.call(bg, c).await.unwrap_err();
    assert!(e.to_string().contains("cannot run in the background"), "{e}");
    assert!(reg.list().is_empty());
}

#[tokio::test]
async fn background_bash_output_from_the_network_is_untrusted() {
    let ws = tempfile::tempdir().unwrap();
    let reg = Arc::new(TaskRegistry::new(Arc::new(MemBlobStore::new())));
    let bash = agent_tools::BashTool::default();
    let input = json!({ "command": "echo page", "background": true });
    let mut c = bash_ctx(&bash, &input, ws.path(), &reg);
    c.grants.push(Access::read(ResourceUri::net("example.com", 443)));
    let id = started_id(&bash.call(input.clone(), c).await.unwrap());
    reg.wait(id).await;
    let mut c = ctx(ws.path(), vec![]);
    c.tasks = Some(reg.clone());
    let out = TaskOutput.call(json!({ "id": id }), c).await.unwrap();
    assert!(out.trust.is_some_and(|t| t.is_untrusted()), "labelled like the network content it may carry");
    // Offline commands: no label.
    let input = json!({ "command": "echo local", "background": true });
    let id = started_id(&bash.call(input.clone(), bash_ctx(&bash, &input, ws.path(), &reg)).await.unwrap());
    reg.wait(id).await;
    assert_eq!(reg.trust(id), None);
}
