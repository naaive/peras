//! `load_skill` beyond the project directory (user skills, a compiled
//! catalog) and the background task tools.

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
