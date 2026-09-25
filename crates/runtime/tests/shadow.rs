use agent_proto::*;
use agent_runtime::assemble::Assembler;
use agent_runtime::dispatch::preview;
use agent_runtime::shadow::{parse_gitignore, ChangeDetection, ShadowOptions};
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

// ---------------------------------------------------------------- change detection

fn changes(c: &CheckpointInfo) -> (Vec<String>, Vec<String>) {
    let mut a = c.agent_changes.clone();
    let mut e = c.external_changes.clone();
    a.sort();
    e.sort();
    (a, e)
}

#[tokio::test]
async fn shadow_watcher_detects_create_modify_delete_rename() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::create_dir_all(ws.join("src/deep")).unwrap();
    fs::create_dir_all(ws.join("target/debug")).unwrap();
    fs::write(ws.join("src/a.rs"), "a").unwrap();
    fs::write(ws.join("src/b.rs"), "b").unwrap();
    fs::write(ws.join("src/deep/c.rs"), "c").unwrap();
    fs::write(ws.join("gone.txt"), "g").unwrap();

    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    let d = ck.detection();
    assert_eq!(d.mode, ChangeDetection::Watcher, "{d:?}");
    ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(ck.detection().last_full_scan.as_deref(), Some("first checkpoint"));

    // Create, modify, delete, rename a file; move a directory; a new nested
    // directory; entries inside an ignored directory.
    fs::write(ws.join("new.txt"), "n").unwrap();
    fs::write(ws.join("src/a.rs"), "a changed").unwrap();
    fs::remove_file(ws.join("gone.txt")).unwrap();
    fs::rename(ws.join("src/b.rs"), ws.join("src/b2.rs")).unwrap();
    fs::rename(ws.join("src/deep"), ws.join("moved")).unwrap();
    fs::create_dir_all(ws.join("fresh/x/y")).unwrap();
    fs::write(ws.join("fresh/x/y/z.txt"), "z").unwrap();
    fs::write(ws.join("target/new.o"), "o").unwrap();
    fs::write(ws.join("target/debug/deep.o"), "o").unwrap();
    fs::write(ws.join("x.log"), "l").unwrap();
    let c1 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    let d = ck.detection();
    assert_eq!(d.last_full_scan, None, "incremental expected: {d:?}");
    assert!(d.reliable);
    assert_eq!(
        changes(&c1).1,
        vec![
            "fresh/x/y/z.txt",
            "gone.txt",
            "moved/c.rs",
            "new.txt",
            "src/a.rs",
            "src/b.rs",
            "src/b2.rs",
            "src/deep/c.rs",
            "target/",
            "x.log"
        ]
    );

    // Changes inside the moved and the new directory are seen afterwards.
    fs::write(ws.join("moved/c.rs"), "c changed").unwrap();
    fs::write(ws.join("fresh/x/w.txt"), "w").unwrap();
    // A .gitignore change re-walks its directory.
    fs::write(ws.join("src/.gitignore"), "*.rs\n").unwrap();
    let c2 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(ck.detection().last_full_scan, None);
    assert_eq!(
        changes(&c2).1,
        vec!["fresh/x/w.txt", "moved/c.rs", "src/.gitignore"],
        "now-ignored but unchanged files are not changes"
    );

    // Delete a directory tree; nothing else.
    fs::remove_dir_all(ws.join("fresh")).unwrap();
    let c3 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(changes(&c3).1, vec!["fresh/x/w.txt", "fresh/x/y/z.txt"]);
    let c4 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(c4.agent_changes.is_empty() && c4.external_changes.is_empty(), "{c4:?}");
    assert_eq!(ck.detection().counts, (1, 4));
}

#[tokio::test]
async fn shadow_watcher_overflow_falls_back_to_full_scan() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join("a"), "0").unwrap();
    let ck = ShadowCheckpointer::new(&ws, store_dir.path()).unwrap();
    ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert!(ck.detection().reliable);

    // Events are lost: the change is still found, by a full scan.
    fs::write(ws.join("a"), "changed").unwrap();
    fs::write(ws.join("b"), "new").unwrap();
    ck.inject_watch_overflow();
    let c1 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(changes(&c1).1, vec!["a", "b"]);
    let d = ck.detection();
    assert!(d.last_full_scan.as_deref().unwrap().contains("injected overflow"), "{d:?}");
    assert_eq!(d.mode, ChangeDetection::Watcher);
    assert!(d.reliable, "reliable again after the full scan");

    // Back to incremental.
    fs::remove_file(ws.join("b")).unwrap();
    let c2 = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(changes(&c2).1, vec!["b"]);
    assert_eq!(ck.detection().last_full_scan, None);
}

#[tokio::test]
async fn shadow_scan_mode_option() {
    let ws_dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    fs::write(ws.join("a"), "0").unwrap();
    let ck = ShadowCheckpointer::with_options(&ws, store_dir.path(), ShadowOptions { watch: false }).unwrap();
    let d = ck.detection();
    assert_eq!(d.mode, ChangeDetection::Scan);
    assert_eq!(d.watcher_off.as_deref(), Some("disabled by options"));
    ck.checkpoint(&scope(vec![], true)).await.unwrap();
    fs::write(ws.join("a"), "1").unwrap();
    let c = ck.checkpoint(&scope(vec![], true)).await.unwrap();
    assert_eq!(changes(&c).1, vec!["a"]);
    let d = ck.detection();
    assert!(!d.reliable && d.last_full_scan.is_some() && d.counts == (2, 0), "{d:?}");
}

/// xorshift64*
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        (!v.is_empty()).then(|| v[self.below(v.len())].clone())
    }
}

/// (files, dirs) under `root`, relative, not following symlinks, skipping `.git`.
fn tree(root: &Path) -> (Vec<String>, Vec<String>) {
    let (mut files, mut dirs) = (vec![], vec![]);
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            let t = e.file_type().unwrap();
            if t.is_dir() && e.file_name() != ".git" {
                dirs.push(rel);
                stack.push(p);
            } else if t.is_file() {
                files.push(rel);
            }
        }
    }
    files.sort();
    dirs.sort();
    (files, dirs)
}

/// Random edit sequences: the watcher-driven checkpointer reports exactly
/// what the scanning one does, checkpoint by checkpoint.
#[tokio::test]
async fn shadow_watcher_matches_scan_on_random_edits() {
    for seed in 1..=6u64 {
        let ws_dir = tempfile::tempdir().unwrap();
        let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ws = fs::canonicalize(ws_dir.path()).unwrap();
        fs::create_dir_all(ws.join(".git/info")).unwrap();
        fs::write(ws.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::create_dir_all(ws.join("target")).unwrap();
        for i in 0..30 {
            let d = ws.join(format!("d{}/e{}", i % 4, i % 3));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(format!("f{i}.txt")), format!("v{i}")).unwrap();
        }
        let watched = ShadowCheckpointer::new(&ws, s1.path()).unwrap();
        let scanned = ShadowCheckpointer::with_options(&ws, s2.path(), ShadowOptions { watch: false }).unwrap();
        assert_eq!(watched.detection().mode, ChangeDetection::Watcher);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let mut counter = 0u32;
        for step in 0..60 {
            let n_ops = 1 + rng.below(6);
            let mut declared = vec![];
            for _ in 0..n_ops {
                counter += 1;
                let (files, dirs) = tree(&ws);
                let dir = rng.pick(&dirs).map(|d| ws.join(d)).unwrap_or_else(|| ws.clone());
                match rng.below(14) {
                    0 | 1 => {
                        let _ = fs::write(dir.join(format!("n{counter}.txt")), "new");
                    }
                    2..=4 => {
                        if let Some(f) = rng.pick(&files) {
                            // Size-changing rewrite (same-size rewrites within one
                            // mtime tick are invisible to both modes alike).
                            let old = fs::read(ws.join(&f)).unwrap_or_default();
                            let _ = fs::write(ws.join(&f), format!("{}{counter}", String::from_utf8_lossy(&old)));
                        }
                    }
                    5 => {
                        if let Some(f) = rng.pick(&files) {
                            let _ = fs::remove_file(ws.join(f));
                        }
                    }
                    6 => {
                        if let Some(f) = rng.pick(&files) {
                            let _ = fs::rename(ws.join(&f), dir.join(format!("r{counter}")));
                        }
                    }
                    7 => {
                        if let Some(d) = rng.pick(&dirs) {
                            let to = ws.join(format!("m{counter}"));
                            let _ = fs::rename(ws.join(d), to);
                        }
                    }
                    8 => {
                        let nd = dir.join(format!("nd{counter}/sub"));
                        let _ = fs::create_dir_all(&nd);
                        let _ = fs::write(nd.join("x"), "x");
                    }
                    9 => {
                        if let Some(d) = rng.pick(&dirs) {
                            if d != "target" {
                                let _ = fs::remove_dir_all(ws.join(d));
                            }
                        }
                    }
                    10 => {
                        let t = ws.join("target");
                        let _ = fs::create_dir_all(t.join("deep"));
                        let _ = fs::write(t.join(if rng.below(2) == 0 { "deep/o" } else { "o" }), format!("{counter}"));
                        let _ = fs::write(ws.join(format!("l{counter}.log")), "log");
                    }
                    11 => {
                        // Toggle an ignore rule in a nested .gitignore.
                        let gi = dir.join(".gitignore");
                        if gi.exists() {
                            let _ = fs::remove_file(gi);
                        } else {
                            let _ = fs::write(gi, "*.txt\n!n*.txt\n");
                        }
                    }
                    12 => {
                        // A file turns into a directory, or a symlink appears.
                        if let Some(f) = rng.pick(&files) {
                            let p = ws.join(&f);
                            if fs::remove_file(&p).is_ok() {
                                if rng.below(2) == 0 {
                                    let _ = fs::create_dir_all(&p);
                                    let _ = fs::write(p.join("inner"), "i");
                                } else {
                                    #[cfg(unix)]
                                    let _ = std::os::unix::fs::symlink(&ws, &p);
                                }
                            }
                        }
                    }
                    _ => {
                        if rng.below(3) == 0 {
                            let _ = fs::write(ws.join(".git/info/exclude"), format!("*.e{counter}\n"));
                        } else if let Some(f) = rng.pick(&files) {
                            declared.push(ws.join(&f));
                        }
                    }
                }
            }
            let sp = step % 3 != 0;
            // Declared writes name files (reading originals of a directory fails in both modes).
            let declared: Vec<Access> = declared.iter().filter(|p| p.is_file()).map(|p| w(p)).collect();
            if !declared.is_empty() {
                watched.save_originals(&declared).await.unwrap();
                scanned.save_originals(&declared).await.unwrap();
            }
            let a = watched.checkpoint(&scope(declared.clone(), sp)).await.unwrap();
            let b = scanned.checkpoint(&scope(declared, sp)).await.unwrap();
            assert_eq!(a.id, b.id);
            assert_eq!(changes(&a), changes(&b), "seed {seed} step {step}: {:?}", watched.detection());
        }
        let d = watched.detection();
        assert!(d.counts.1 > 20, "mostly incremental checkpoints: {d:?}");
        // Rewinding gives the same outcome in both modes.
        // (Both may fail alike, e.g. a file to restore is now a directory.)
        let all = RestorePlan { to: EventId::new("e"), checkpoint: None };
        let r = watched.restore(&all).await.map(|r| (r.conflicts, r.unrestored_ignored));
        let r2 = scanned.restore(&all).await.map(|r| (r.conflicts, r.unrestored_ignored));
        assert_eq!(r, r2, "seed {seed}");
    }
}

/// Timing harness (not a budget check): incremental safe-point checkpoint of
/// a synthetic workspace shaped like `agent-bench`'s (300-byte files, 200 per
/// directory, an ignored `target/`), 3 files rewritten per round, in scan and
/// watcher mode. `AGENT_BENCH_FILES` (default 20000), `AGENT_BENCH_ROUNDS`
/// (default 50). Run in release:
/// `cargo test -p agent-runtime --release --test shadow -- --ignored --nocapture shadow_timing`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn shadow_timing() {
    use std::time::{Duration, Instant};
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (files, rounds) = (env("AGENT_BENCH_FILES", 20_000), env("AGENT_BENCH_ROUNDS", 50));
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    let path = |i: usize| ws.join(format!("pkg{:03}/mod{:03}/f{i:06}.rs", i / 2_000, (i / 200) % 10));
    let text = |i: usize, round: usize| {
        let mut s = format!("// round {round}\n");
        let mut k = 0;
        while s.len() < 300 + round % 50 {
            s.push_str(&format!("fn item_{i}_{k}(x: u32) -> u32 {{ x.wrapping_mul({k}) }}\n"));
            k += 1;
        }
        s
    };
    fs::write(ws.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::create_dir_all(ws.join("target")).unwrap();
    for i in 0..20 {
        fs::write(ws.join(format!("target/obj{i}.o")), [0u8; 512]).unwrap();
    }
    for i in 0..files {
        if i % 200 == 0 {
            fs::create_dir_all(path(i).parent().unwrap()).unwrap();
        }
        fs::write(path(i), text(i, 0)).unwrap();
    }
    let stats = |v: &mut Vec<Duration>| {
        v.sort();
        let pct = |p: f64| v[(((v.len() - 1) as f64) * p).round() as usize];
        let mean = v.iter().sum::<Duration>() / v.len() as u32;
        format!("mean {mean:.1?}, p50 {:.1?}, p99 {:.1?}, max {:.1?}", pct(0.5), pct(0.99), v[v.len() - 1])
    };
    let mut round = 0;
    for watch in [false, true] {
        let store = tempfile::tempdir().unwrap();
        let ck = ShadowCheckpointer::with_options(&ws, store.path(), ShadowOptions { watch }).unwrap();
        let t = Instant::now();
        ck.checkpoint(&scope(vec![], true)).await.unwrap();
        let full = t.elapsed();
        let mut v = vec![];
        for _ in 0..rounds {
            round += 1;
            for j in 0..3 {
                let i = (round * 7_919 + j * 104_729) % files;
                fs::write(path(i), text(i + round, round)).unwrap();
            }
            let t = Instant::now();
            let c = ck.checkpoint(&scope(vec![], true)).await.unwrap();
            v.push(t.elapsed());
            assert_eq!(c.external_changes.len(), 3);
        }
        let d = ck.detection();
        eprintln!("[timing] {files} files, {:?}: first full scan {full:.1?}; incremental: {} ({d:?})", d.mode, stats(&mut v));
    }
}
