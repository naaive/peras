//! One writing session per workspace: a second process gets a clear error;
//! agents of the same process (e.g. sub-agents) share the lock.

use agent::prelude::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

const HOLDER_ENV: &str = "PERAS_LOCK_TEST_HOLD";
const HOME_ENV: &str = "PERAS_LOCK_TEST_HOME";

/// Helper run in a child process: hold the lock on `$PERAS_LOCK_TEST_HOLD`
/// until stdin closes. Both processes use the same (temporary) home, so the
/// same data directory, never the real `~/.agent`.
#[tokio::test]
#[ignore = "helper for writing_session_in_another_process_is_refused"]
async fn lock_holder_helper() {
    let (Some(ws), Some(home)) = (std::env::var_os(HOLDER_ENV), std::env::var_os(HOME_ENV)) else { return };
    let agent = Agent::new(Script::new().say("hi")).workspace(&ws).home(Some(&home)).tools((edit,));
    agent.check().await.unwrap();
    println!("locked");
    let _ = std::io::stdin().read_line(&mut String::new());
}

#[tokio::test]
async fn writing_session_in_another_process_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let h = tempfile::tempdir().unwrap();
    let home = Some(h.path());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_helper", "--ignored", "--nocapture", "--test-threads=1"])
        .env(HOLDER_ENV, d.path())
        .env(HOME_ENV, h.path())
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

    let writer = Agent::new(Script::new().say("hi")).workspace(d.path()).home(home).tools((edit,));
    let e = writer.check().await.unwrap_err();
    assert!(matches!(&e, Error::WorkspaceLocked(m) if m.contains("separate git worktrees")), "{e}");
    assert!(e.to_string().contains(&format!("pid {}", child.id())), "names the holder: {e}");
    // Running fails the same way; reading the profile (`agent doctor`) does not.
    let e = writer.run("hi").await.unwrap_err();
    assert!(e.to_string().contains("in use by another writing session"), "{e}");
    assert!(writer.profile().await.is_ok());
    // A read-only agent takes no lock.
    let reader = Agent::new(Script::new().say("read")).workspace(d.path()).home(home).tools((read,));
    assert_eq!(reader.run("look").await.unwrap(), "read");

    drop(child.stdin.take()); // the helper exits: its lease ends with it
    child.wait().unwrap();
    let writer = Agent::new(Script::new().say("hi")).workspace(d.path()).home(home).tools((edit,));
    assert_eq!(writer.run("now").await.unwrap(), "hi");
}

#[agent::test]
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
