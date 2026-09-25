use agent_proto::{Access, EffectClass, ResourceUri, Trust};
use agent_runtime::{AccessCtx, Tool, ToolError};
use agent_tools::testing::{ctx, text_of};
use agent_tools::McpClient;
use serde_json::json;

fn python() -> Option<String> {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "python3".into())
}

#[tokio::test]
async fn fake_server_roundtrip() {
    let Some(py) = python() else {
        eprintln!("python3 not available; skipping");
        return;
    };
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mcp.py");
    let client = McpClient::spawn("fake", &py, &[script.to_string()], &[])
        .await
        .unwrap();
    assert_eq!(client.server_info()["serverInfo"]["name"], "fake");
    let tools = client.tools(false).await.unwrap();
    assert_eq!(tools.len(), 2, "pagination followed");
    let echo = &tools[0];
    let spec = echo.spec();
    assert_eq!(spec.name, "mcp__fake__echo");
    assert_eq!(spec.class, EffectClass::Network);
    assert_eq!(spec.input_schema["required"], json!(["text"]));
    let actx = AccessCtx {
        workspace: "/w".into(),
    };
    let acc = echo.access(&json!({"text": "hi"}), &actx).unwrap();
    assert_eq!(acc, vec![Access::write(ResourceUri::mcp("fake", "echo"))]);

    assert!(matches!(
        echo.call(json!({"text": "hi"}), ctx("/w", vec![])).await,
        Err(ToolError::NotGranted(_))
    ));
    let out = echo
        .call(json!({"text": "hi"}), ctx("/w", acc))
        .await
        .unwrap();
    assert_eq!(text_of(&out), "echo: hi");
    assert_eq!(
        out.trust,
        Some(Trust::Untrusted {
            source: "mcp:fake".into()
        })
    );

    let fail = &tools[1];
    let r = fail
        .call(
            json!({}),
            ctx("/w", fail.access(&json!({}), &actx).unwrap()),
        )
        .await;
    assert!(matches!(r, Err(ToolError::Failed(m)) if m == "it failed"));

    let trusted = client.tools(true).await.unwrap();
    let out = trusted[0]
        .call(json!({"text": "x"}), ctx("/w", acc_for(&trusted[0])))
        .await
        .unwrap();
    assert_eq!(out.trust, None);
}

fn acc_for(t: &agent_tools::McpTool) -> Vec<Access> {
    vec![Access::write(t.resource())]
}
