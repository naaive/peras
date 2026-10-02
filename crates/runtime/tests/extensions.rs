//! Configuration watcher, task completion notices, subdirectory instruction
//! scanning and the question board's open notifications.

use agent_kernel::Kernel;
use agent_proto::*;
use agent_runtime::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn config_watcher_reports_relevant_changes_after_they_settle() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join(".agent");
    std::fs::create_dir_all(&dir).unwrap();
    let later = d.path().join(".agent/commands");
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let specs = vec![WatchSpec::file(dir.join("settings.toml")), WatchSpec::tree(later.clone())];
    let _w = ConfigWatcher::spawn(
        specs,
        |p| p.extension().is_some_and(|e| e == "toml" || e == "md"),
        Duration::from_millis(100),
        move |paths| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(paths);
            })
        },
    )
    .unwrap();
    std::fs::write(dir.join("runs.db"), "noise").unwrap();
    std::fs::write(dir.join("settings.toml"), "system = [\"x\"]").unwrap();
    std::fs::write(dir.join("settings.toml"), "system = [\"y\"]").unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("change seen").unwrap();
    assert_eq!(got, vec![dir.join("settings.toml")], "debounced into one batch, irrelevant files ignored");

    // A directory created after the watcher started is picked up.
    std::fs::create_dir_all(&later).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    std::fs::write(later.join("review.md"), "Review $ARGUMENTS").unwrap();
    let mut seen = vec![];
    while let Ok(Some(batch)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
        seen.extend(batch);
        if seen.contains(&later.join("review.md")) {
            break;
        }
    }
    assert!(seen.contains(&later.join("review.md")), "{seen:?}");
}

#[tokio::test]
async fn finished_background_task_notifies_its_session() {
    let rt = Runtime::<Kernel>::builder().build();
    let id = SessionId::new("s1");
    let h = rt.create_session(id.clone(), agent_kernel::start_session(id.clone(), "h".into(), KernelConfig::default())).await.unwrap();
    let t = rt.tasks().spawn_for(Some(id.clone()), "long job", None, |_| async { Ok(b"result".to_vec()) });
    assert_eq!(rt.tasks().list_for(&id).len(), 1);
    let mut events = h.subscribe(0);
    let notice = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = futures::StreamExt::next(&mut events).await {
            if let Event::Plugin { kind, data, .. } = &e.body {
                if kind == agent_kernel::PENDING_SIGNAL_KIND && data["key"] == format!("task:{t}") {
                    return data.clone();
                }
            }
        }
        panic!("session closed");
    })
    .await
    .expect("notification delivered");
    assert_eq!(notice["kind"], "notify");
    assert!(notice["text"].as_str().unwrap().contains("long job"));
    // Unowned tasks notify nobody.
    let _ = rt.tasks().spawn("anon", None, |_| async { Ok(vec![]) });
}

#[tokio::test]
async fn instruction_files_along_accessed_directories() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
    std::fs::write(root.join("a/AGENTS.md"), "a rules").unwrap();
    std::fs::write(root.join("a/b/CLAUDE.md"), "b rules").unwrap();
    std::fs::write(root.join("a/b/x.rs"), "").unwrap();
    let rt = Runtime::<Kernel>::builder()
        .options(RuntimeOptions {
            workspace: root.clone(),
            instructions: Some(InstructionScan {
                names: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
                loaded: vec![root.join("AGENTS.md")],
            }),
            ..RuntimeOptions::default()
        })
        .build();
    let fs = |p: PathBuf| ResourceUri::fs(&p.display().to_string());
    let found = agent_runtime::dispatch::scan_instructions(rt.env(), &[Access::read(fs(root.join("a/b/x.rs")))]);
    let paths: Vec<String> = found.iter().map(|f| f.path.clone()).collect();
    assert_eq!(paths, vec![root.join("a/AGENTS.md").display().to_string(), root.join("a/b/CLAUDE.md").display().to_string()]);
    assert_eq!(found[1].text, "b rules");
    // A glob is cut at its first wildcard; outside the workspace nothing.
    let found = agent_runtime::dispatch::scan_instructions(rt.env(), &[Access::write(ResourceUri(format!("fs://{}/a/**", root.display())))]);
    assert_eq!(found.len(), 1);
    assert!(agent_runtime::dispatch::scan_instructions(rt.env(), &[Access::read(ResourceUri::fs("/etc/passwd"))]).is_empty());
    // Off by default.
    let off = Runtime::<Kernel>::builder().options(RuntimeOptions { workspace: root.clone(), ..RuntimeOptions::default() }).build();
    assert!(agent_runtime::dispatch::scan_instructions(off.env(), &[Access::read(fs(root.join("a/b/x.rs")))]).is_empty());
}

#[tokio::test]
async fn ask_board_announces_opened_questions() {
    let board = Arc::new(AskBoard::new());
    let mut rx = board.subscribe();
    let q = Question {
        id: QuestionId("subagent:s1/c1:q1".into()),
        prompt: "allow?".into(),
        level: ApprovalLevel::Policy,
        ring: Ring::Human,
        rules: vec![],
        remember_destination: None,
    };
    board.open(&q);
    board.open(&q);
    assert_eq!(rx.recv().await.unwrap(), q);
    assert!(rx.try_recv().is_err(), "re-opening is not announced again");
}
