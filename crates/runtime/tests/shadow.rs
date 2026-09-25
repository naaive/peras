use agent_proto::*;
use agent_runtime::assemble::Assembler;
use agent_runtime::dispatch::preview;
use agent_runtime::shadow::parse_gitignore;
use agent_runtime::*;
use std::fs;
use std::path::Path;

fn w(p: &Path) -> Access {
    Access::write(ResourceUri::fs(p.to_str().unwrap()))
}

fn scope(writes: Vec<Access>, safe_point: bool) -> CheckpointScope {
    CheckpointScope { declared_writes: writes, safe_point }
}

fn plan(ck: &CheckpointInfo) -> RestorePlan {
    RestorePlan { to: EventId::new("e"), checkpoint: Some(ck.id.clone()) }
}

#[tokio::test]
async fn shadow_attribution_restore_conflicts_and_idempotence() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join("a.txt"), "a0").unwrap();
    fs::write(ws.join("b.txt"), "b0").unwrap();
    fs::write(ws.join(".gitignore"), "# build\ntarget/\n*.log\n").unwrap();
    fs::create_dir_all(ws.join(".git")).unwrap();
    fs::write(ws.join(".git/config"), "x").unwrap();
    fs::create_dir_all(ws.join("target")).unwrap();
    fs::write(ws.join("target/out"), "o0").unwrap();

    // The store must be outside the workspace.
    assert!(ShadowCheckpointer::new(&ws, ws.join("shadow")).is_err());
    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();

    let c0 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(c0.agent_changes.is_empty() && c0.external_changes.is_empty());

    // Pre-batch: the agent declares writes to a.txt and target/**.
    let decl = vec![w(&ws.join("a.txt")), w(&ws.join("target/**"))];
    ck.save_originals(&decl).await.unwrap();
    let c1 = ck.checkpoint(&scope(decl, false)).await.unwrap();
    assert!(c1.agent_changes.is_empty() && c1.external_changes.is_empty());

    // The batch runs; meanwhile the user edits b.txt and .git changes.
    fs::write(ws.join("a.txt"), "a1 by agent").unwrap();
    // Inside an ignored directory: only the directory's own metadata is
    // tracked, so an added entry is noticed.
    fs::write(ws.join("target/new"), "o1 by agent").unwrap();
    fs::write(ws.join("b.txt"), "b1 by user").unwrap();
    fs::write(ws.join(".git/config"), "changed").unwrap();
    let c2 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(c2.agent_changes, vec!["a.txt".to_string(), "target/".to_string()]);
    assert_eq!(c2.external_changes, vec!["b.txt".to_string()]);

    // Nothing changed: an extra checkpoint reports nothing (only changed files hashed).
    let c2b = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(c2b.agent_changes.is_empty() && c2b.external_changes.is_empty());

    // Rewind to c1: only the agent's change is undone.
    let r = ck.restore(&plan(&c1)).await.unwrap();
    assert_eq!(r.restored, vec!["a.txt".to_string()]);
    assert!(r.conflicts.is_empty());
    assert_eq!(r.unrestored_ignored, vec!["target/".to_string()]);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "a0");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "b1 by user");
    assert_eq!(fs::read_to_string(ws.join("target/new")).unwrap(), "o1 by agent");

    // Idempotent: same plan again converges to the same report.
    let r2 = ck.restore(&plan(&c1)).await.unwrap();
    assert_eq!(r2, r);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "a0");
    // The restore's own writes are not "external changes".
    let c_after = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(c_after.external_changes.is_empty(), "{c_after:?}");

    // Conflict: agent writes, then the user edits the same file.
    let decl = vec![w(&ws.join("a.txt")), w(&ws.join("new.txt"))];
    ck.save_originals(&decl).await.unwrap();
    let c3 = ck.checkpoint(&scope(decl, false)).await.unwrap();
    fs::write(ws.join("a.txt"), "a2 by agent").unwrap();
    fs::write(ws.join("new.txt"), "created by agent").unwrap();
    let c4 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(c4.agent_changes, vec!["a.txt".to_string(), "new.txt".to_string()]);
    fs::write(ws.join("a.txt"), "user edit").unwrap();
    let r3 = ck.restore(&plan(&c3)).await.unwrap();
    assert_eq!(r3.conflicts, vec!["a.txt".to_string()]);
    assert_eq!(r3.restored, vec!["new.txt".to_string()]);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "user edit");
    assert!(!ws.join("new.txt").exists(), "agent-created file removed");

    // State persists across instances.
    drop(ck);
    let ck2 = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    let c5 = ck2.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(c5.agent_changes.is_empty() && c5.external_changes.is_empty(), "{c5:?}");
    // Rewinding all the way to c0 still refuses to touch the conflicting file.
    let r4 = ck2.restore(&plan(&c0)).await.unwrap();
    assert_eq!(r4.conflicts, vec!["a.txt".to_string()]);
}

#[tokio::test]
async fn shadow_restore_originals_for_crash_recovery() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join("f"), "orig").unwrap();
    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    ck.checkpoint(&scope(vec![], true)).await.unwrap();
    let decl = vec![w(&ws.join("f"))];
    ck.save_originals(&decl).await.unwrap();
    ck.checkpoint(&scope(decl.clone(), false)).await.unwrap();
    fs::write(ws.join("f"), "half-written").unwrap();
    // Crash; a new process restores the originals before re-running.
    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    ck.restore_originals(&decl).await.unwrap();
    assert_eq!(fs::read_to_string(ws.join("f")).unwrap(), "orig");
}

#[test]
fn gitignore_subset() {
    let p = parse_gitignore("# c\n\ntarget/\n*.log\n/build\n!keep.log\ndocs/gen\n");
    assert_eq!(
        p,
        vec!["**/target/**", "**/*.log", "**/*.log/**", "build", "build/**", "docs/gen", "docs/gen/**"]
    );
}

#[test]
fn preview_is_utf8_safe() {
    let s = "é".repeat(5000);
    let p = preview(&s, 101);
    assert!(p.contains("bytes omitted"));
    assert!(p.len() < 300);
    assert_eq!(preview("short", 10), "short");
}

#[test]
fn assembler_partial_drops_incomplete_tool_use() {
    let reg = ToolRegistry::default();
    let mut a = Assembler::new();
    a.push(Delta::Text("a".into()), &reg);
    a.push(Delta::ToolUseStart { id: CallId::new("1"), name: "x".into() }, &reg);
    a.push(Delta::ToolUseInput("{\"k\":".into()), &reg);
    let p = a.partial();
    assert_eq!(p.stop, StopReason::Interrupted);
    assert_eq!(p.content, vec![ContentBlock::Text { text: "a".into() }]);
    a.push(Delta::ToolUseInput("1}".into()), &reg);
    let pushed = a.push(Delta::ToolUseEnd, &reg);
    assert_eq!(pushed.call.unwrap().input, serde_json::json!({"k":1}));
    assert_eq!(a.finish().stop, StopReason::ToolUse);
}

#[tokio::test]
async fn direct_sandbox_runs_argv() {
    let sb = DirectSandbox;
    assert!(!sb.report().available);
    let out = sb
        .run(&["sh".into(), "-c".into(), "echo hi".into()], &SandboxSpec::default(), Default::default())
        .await
        .unwrap();
    assert_eq!(out.stdout, b"hi\n");
    assert_eq!(out.status, Some(0));
}

#[test]
fn ulid_gen_is_monotonic() {
    let g = UlidGen::new();
    let mut last = String::new();
    for i in 0..1000 {
        let id = g.event_id(Timestamp(1_000 + i / 100)).0;
        assert!(id > last, "{id} <= {last}");
        last = id;
    }
}

#[tokio::test]
async fn shadow_nested_gitignore_negation_and_ignored_dirs() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join(".gitignore"), "*.log\nnode_modules/\n").unwrap();
    fs::create_dir_all(ws.join("sub")).unwrap();
    fs::write(ws.join("sub/.gitignore"), "*.tmp\n!keep.tmp\n!important.log\n").unwrap();
    fs::create_dir_all(ws.join(".git/info")).unwrap();
    fs::write(ws.join(".git/info/exclude"), "secret.txt\n").unwrap();
    for f in ["sub/a.tmp", "sub/keep.tmp", "sub/important.log", "x.log", "secret.txt", "plain.txt"] {
        fs::write(ws.join(f), "v0").unwrap();
    }
    fs::create_dir_all(ws.join("node_modules/pkg/deep")).unwrap();
    fs::write(ws.join("node_modules/pkg/deep/f.js"), "v0").unwrap();

    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    ck.checkpoint(&scope(vec![], true)).await.unwrap();
    let decl = vec![w(&ws.join("**"))];
    ck.save_originals(&decl).await.unwrap();
    let c1 = ck.checkpoint(&scope(decl, false)).await.unwrap();

    for f in ["sub/a.tmp", "sub/keep.tmp", "sub/important.log", "x.log", "secret.txt", "plain.txt"] {
        fs::write(ws.join(f), "v1 by agent").unwrap();
    }
    // Deep inside an ignored directory: not descended into, so not seen.
    fs::write(ws.join("node_modules/pkg/deep/g.js"), "new").unwrap();
    let c2 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(!c2.agent_changes.iter().any(|p| p.starts_with("node_modules")), "{c2:?}");
    // A new top-level entry (made by the user) changes the ignored directory's own metadata.
    fs::write(ws.join("node_modules/new.js"), "new").unwrap();
    let c3 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(c3.external_changes, vec!["node_modules/".to_string()]);

    let r = ck.restore(&plan(&c1)).await.unwrap();
    // Negated (re-included) paths are restored; ignored ones only listed.
    assert_eq!(r.restored, vec!["plain.txt".to_string(), "sub/important.log".to_string(), "sub/keep.tmp".to_string()]);
    assert_eq!(
        r.unrestored_ignored,
        vec!["secret.txt".to_string(), "sub/a.tmp".to_string(), "x.log".to_string()]
    );
    assert_eq!(fs::read_to_string(ws.join("sub/keep.tmp")).unwrap(), "v0");
    assert_eq!(fs::read_to_string(ws.join("sub/a.tmp")).unwrap(), "v1 by agent");
    assert!(r.git_refs.is_empty());
}

#[tokio::test]
async fn shadow_lists_git_ref_changes_without_restoring_them() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    let git = ws.join(".git");
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::create_dir_all(git.join("refs/tags")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git.join("refs/heads/main"), "aaa\n").unwrap();
    fs::write(git.join("packed-refs"), "# pack-refs with: peeled\nbbb refs/heads/old\nccc refs/tags/v1\n^ddd\n").unwrap();
    fs::write(ws.join("f"), "x").unwrap();

    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    let c0 = ck.checkpoint(&scope(vec![], true)).await.unwrap();

    // `git checkout -b feature && git commit && git tag -d v1`, and main moves.
    fs::write(git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
    fs::write(git.join("refs/heads/feature"), "eee\n").unwrap();
    fs::write(git.join("refs/heads/main"), "fff\n").unwrap();
    fs::write(git.join("packed-refs"), "bbb refs/heads/old\n").unwrap();
    ck.checkpoint(&scope(vec![], true)).await.unwrap();

    let r = ck.restore(&plan(&c0)).await.unwrap();
    assert_eq!(
        r.git_refs,
        vec![
            "HEAD: ref: refs/heads/main -> ref: refs/heads/feature".to_string(),
            "refs/heads/feature: (none) -> eee".to_string(),
            "refs/heads/main: aaa -> fff".to_string(),
            "refs/tags/v1: ccc -> (none)".to_string(),
        ]
    );
    // Listed only, never restored.
    assert_eq!(fs::read_to_string(git.join("HEAD")).unwrap(), "ref: refs/heads/feature\n");
    // Loose refs override packed ones.
    let refs = agent_runtime::shadow::read_git_refs(&ws).unwrap();
    fs::write(git.join("packed-refs"), "bbb refs/heads/old\nzzz refs/heads/main\n").unwrap();
    assert_eq!(agent_runtime::shadow::read_git_refs(&ws).unwrap(), refs);
}
