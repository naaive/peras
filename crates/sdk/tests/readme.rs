//! The README's test example, verbatim, and what it relies on: agents built
//! inside `#[agent::test]` use the temporary workspace (never the real
//! current directory) and the virtual clock, and an awaited run allows
//! policy-level asks there.

use agent::prelude::*;

// ---- README.md, "Tests use a scripted model and a temporary workspace" ----
#[agent::test]
async fn edits_readme() -> anyhow::Result<()> {
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "bar" }))
        .say("Done");
    let out = Agent::new(model).tools((edit,)).run("Edit README").await?;
    assert_eq!(out, "Done");
    Ok(())
}
// ---- end of the README example ----

/// The same example with a README in the temporary workspace: the edit lands
/// there, and the real current directory is untouched.
#[agent::test]
async fn readme_example_edits_the_temporary_workspace() -> anyhow::Result<()> {
    let ws = agent::tools::testing::workspace();
    assert_ne!(ws, std::env::current_dir()?);
    std::fs::write(ws.join("README.md"), "hello foo\n")?;
    let cwd_readme = std::env::current_dir()?.join("README.md");
    let before = std::fs::read(&cwd_readme).ok();
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "bar" }))
        .say("Done");
    let agent = Agent::new(model).tools((edit,));
    let run = agent.run("Edit README");
    let session = run.session_id().clone();
    // Awaited exactly like the README: its policy-level ask is allowed.
    assert_eq!(run.await?, "Done");
    assert_eq!(std::fs::read_to_string(ws.join("README.md"))?, "hello bar\n");
    let rt = agent.runtime().await?;
    let events = rt.env().journal.load(&session, 0).await?;
    let answered_by_code = events.iter().any(|e| {
        matches!(&e.body, agent::proto::Event::QuestionAnswered { responder: agent::proto::Responder::Code, .. })
    });
    assert!(answered_by_code, "recorded as answered by the embedding code");
    assert_eq!(std::fs::read(&cwd_readme).ok(), before, "the real cwd is untouched");

    // Events carry the virtual time, which moves only when the test says so.
    let events = rt.env().journal.load(&session, 0).await?;
    assert!(events.iter().all(|e| e.at.0 == agent::tools::testing::TEST_EPOCH_MS), "virtual clock");
    agent::tools::testing::clock().advance(60_000);
    let model = Script::new().say("later");
    let agent = Agent::new(model);
    let mut run = agent.run("again");
    let session = run.session_id().clone();
    while run.next().await.is_some() {}
    let events = agent.runtime().await?.env().journal.load(&session, 0).await?;
    assert!(events.iter().all(|e| e.at.0 == agent::tools::testing::TEST_EPOCH_MS + 60_000));
    Ok(())
}

/// Awaiting a run in a test allows policy-level asks on the temporary
/// workspace, but never invariant-level ones (a real human only).
#[agent::test]
async fn awaited_test_runs_allow_policy_asks_only() -> anyhow::Result<()> {
    let ws = agent::tools::testing::workspace();
    std::fs::write(ws.join("README.md"), "hello foo\n")?;
    let model = Script::new()
        .call(write, json!({ "file": "notes.md", "content": "n" }))
        .call(write, json!({ "file": ".agent/settings.toml", "content": "x = 1" }))
        .say("Done");
    let out = Agent::new(model).tools((write,)).run("write").await?;
    assert_eq!(out, "Done");
    assert_eq!(std::fs::read_to_string(ws.join("notes.md"))?, "n");
    assert!(!ws.join(".agent/settings.toml").exists(), "self-modification needs a human");

    // An agent pointed elsewhere gets no automatic approval.
    let other = tempfile::tempdir()?;
    let model = Script::new().call(write, json!({ "file": "x.md", "content": "x" })).say("Done");
    Agent::new(model).workspace(other.path()).tools((write,)).run("write").await?;
    assert!(!other.path().join("x.md").exists());
    Ok(())
}
