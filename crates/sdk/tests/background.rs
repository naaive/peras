//! Background bash commands end to end: a task of the session (notified when
//! it ends, output read with `task_output`), its changes attributed to the
//! agent while it runs, stopped by a rewind.

use agent::prelude::*;
use agent::proto::{Event, Seq};
use agent::runtime::TaskStatus;
use agent::tools::testing::LocalSandbox;
use std::time::Duration;

/// Drive a run to its end, allowing every ask (embedding code counts as the user).
async fn drive(mut run: Run) -> Option<TurnOutcome> {
    let mut out = None;
    while let Some(u) = run.next().await {
        match u {
            Update::Ask(a) => a.allow(),
            Update::Done(o) => out = Some(o),
            _ => {}
        }
    }
    out
}

fn done(text: &str) -> Option<TurnOutcome> {
    Some(TurnOutcome::Done { text: text.into() })
}

#[agent::test]
async fn background_bash_is_a_task_of_the_session() {
    let ws = agent::tools::testing::workspace();
    let model = Script::new()
        .call(Bash, json!({ "command": "sleep 0.5; echo hi > bg.txt; echo built", "background": true }))
        .say("started")
        .call("task_output", json!({ "id": 1 }))
        .say("done");
    let agent = Agent::new(model).tools((Bash, agent::tools::TaskOutput)).sandbox(LocalSandbox::default());
    let chat = agent.session("bg");
    assert_eq!(drive(chat.stream("build it")).await, done("started"));
    let rt = agent.runtime().await.unwrap();
    let tasks = rt.tasks().list_for(chat.id());
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].name.starts_with("bash: sleep 0.5"), "{tasks:?}");
    let status = tokio::time::timeout(Duration::from_secs(10), rt.tasks().wait(tasks[0].id)).await.unwrap();
    assert!(matches!(status, Some(TaskStatus::Done { .. })), "{status:?}");
    assert_eq!(std::fs::read_to_string(ws.join("bg.txt")).unwrap(), "hi\n");

    assert_eq!(drive(chat.stream("what happened?")).await, done("done"));
    let events = chat.events().await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.body, Event::Injected { source, .. } if source == "task")),
        "the task's end was delivered to the session"
    );
    let output = events.iter().find_map(|e| match &e.body {
        Event::ToolResulted { call, result } if call.name == "task_output" => Some(format!("{:?}", result.content)),
        _ => None,
    });
    assert!(output.unwrap().contains("built"));
    // Written after the call returned, while the task ran: the agent's change.
    let (agent_changed, external): (Vec<String>, Vec<String>) = events
        .iter()
        .filter_map(|e| match &e.body {
            Event::CheckpointTaken { info } => Some((info.agent_changes.clone(), info.external_changes.clone())),
            _ => None,
        })
        .fold((vec![], vec![]), |(mut a, mut x), (ac, xc)| {
            a.extend(ac);
            x.extend(xc);
            (a, x)
        });
    assert!(agent_changed.iter().any(|p| p == "bg.txt"), "agent: {agent_changed:?} external: {external:?}");
    assert!(!external.iter().any(|p| p == "bg.txt"));
}

#[agent::test]
async fn rewind_stops_background_writers_first() {
    let ws = agent::tools::testing::workspace();
    let model = Script::new()
        .call(Bash, json!({ "command": "sleep 2; touch late", "background": true }))
        .say("started");
    let agent = Agent::new(model).tools((Bash,)).sandbox(LocalSandbox::default());
    let chat = agent.session("rewound");
    let before: Seq = chat.next_seq().await.unwrap();
    assert_eq!(drive(chat.stream("start the watcher")).await, done("started"));
    let rt = agent.runtime().await.unwrap();
    let id = rt.tasks().list_for(chat.id())[0].id;
    chat.rewind(before).await.unwrap();
    assert_eq!(rt.tasks().get(id).unwrap().status, TaskStatus::Killed, "stopped before the workspace was restored");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(!ws.join("late").exists());
}
