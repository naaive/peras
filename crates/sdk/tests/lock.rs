//! One writing session per workspace: a second process gets a clear error;
//! agents of the same process (e.g. sub-agents) share the lock.

use agent::prelude::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

const HOLDER_ENV: &str = "PERAS_LOCK_TEST_HOLD";

/// Helper run in a child process: hold the lock on `$PERAS_LOCK_TEST_HOLD`
/// until stdin closes.
#[tokio::test]
#[ignore = "helper for writing_session_in_another_process_is_refused"]
async fn lock_holder_helper() {
    let Some(ws) = std::env::var_os(HOLDER_ENV) else { return };
    let agent = Agent::new(Script::new().say("hi")).workspace(&ws).tools((edit,));
    agent.check().await.unwrap();
    println!("locked");
    let _ = std::io::stdin().read_line(&mut String::new());
}

#[tokio::test]
async fn writing_session_in_another_process_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_helper", "--ignored", "--nocapture", "--test-threads=1"])
        .env(HOLDER_ENV, d.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while out.read_line(&mut line).unwrap() > 0 && !line.contains("locked") {
        line.clear();
    }
    assert!(line.contains("locked"), "helper did not take the lock");

    let writer = Agent::new(Script::new().say("hi")).workspace(d.path()).tools((edit,));
    let e = writer.check().await.unwrap_err();
    assert!(matches!(&e, Error::WorkspaceLocked(m) if m.contains("separate git worktrees")), "{e}");
    assert!(e.to_string().contains(&format!("pid {}", child.id())), "names the holder: {e}");
    // A read-only agent takes no lock.
    let reader = Agent::new(Script::new().say("read")).workspace(d.path()).tools((read,));
    assert_eq!(reader.run("look").await.unwrap(), "read");

    drop(child.stdin.take()); // the helper exits: its lease ends with it
    child.wait().unwrap();
    let writer = Agent::new(Script::new().say("hi")).workspace(d.path()).tools((edit,));
    assert_eq!(writer.run("now").await.unwrap(), "hi");
}

#[tokio::test]
async fn agents_of_one_process_share_the_lock() {
    let d = tempfile::tempdir().unwrap();
    let reviewer = Agent::new(Script::new().say("lgtm")).workspace(d.path()).tools((edit,)).named("reviewer");
    let lead = Agent::new(Script::new().call("reviewer", json!({ "task": "review" })).say("done"))
        .workspace(d.path())
        .tools((edit, reviewer))
        .allow("**");
    assert_eq!(lead.run("go").await.unwrap(), "done");
    let other = Agent::new(Script::new().say("also")).workspace(d.path()).tools((edit,));
    assert_eq!(other.run("go").await.unwrap(), "also");
}
