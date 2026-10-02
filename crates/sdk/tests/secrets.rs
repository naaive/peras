//! Secret values handed to tools never reach the journal, the model context
//! or blobs: they are redacted.

use agent::prelude::*;

const NAME: &str = "PERAS_REDACTION_TEST_TOKEN";
const VALUE: &str = "tok-7f3c9e1a-redaction-test";

/// A misbehaving tool that returns the secret's value.
#[tool]
async fn leak(token: Secret<Name>, big: bool) -> Result<String> {
    let v = token.expose()?;
    let filler = if big { "x".repeat(40_000) } else { String::new() };
    Ok(format!("token is {v}\n{filler}"))
}

#[tokio::test]
async fn secrets_are_redacted_from_journal_context_and_blobs() {
    std::env::set_var(NAME, VALUE);
    let d = tempfile::tempdir().unwrap();
    let model = Script::new()
        .call(leak, json!({ "token": NAME, "big": false }))
        .call(leak, json!({ "token": NAME, "big": true }))
        .say(format!("the model repeats {VALUE}"));
    let agent = Agent::new(model).workspace(d.path()).tools((leak,));
    let mut run = agent.run("use the token");
    let session = run.session_id().clone();
    let mut results = vec![];
    while let Some(u) = run.next().await {
        if let Update::Tool { result, .. } = u {
            results.push(result);
        }
    }
    assert_eq!(results.len(), 2);
    let rt = agent.runtime().await.unwrap();
    let journal = serde_json::to_string(&rt.env().journal.load(&session, 0).await.unwrap()).unwrap();
    assert!(!journal.contains(VALUE), "secret value in the journal");
    assert!(journal.contains(&format!("token is [REDACTED:{NAME}]")), "{journal}");
    // Even what the model says back is redacted before it is journaled.
    assert!(journal.contains(&format!("the model repeats [REDACTED:{NAME}]")));
    // The long output was spilled to a blob: redacted too.
    let blob = results[1]
        .content
        .iter()
        .find_map(|c| match c {
            agent::proto::ToolContent::Blob { blob, .. } => Some(blob.clone()),
            _ => None,
        })
        .expect("spilled to a blob");
    let bytes = rt.env().blobs.get(&blob).await.unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains(VALUE));
    assert!(text.starts_with(&format!("token is [REDACTED:{NAME}]")));
}
