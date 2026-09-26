//! `#[tool]`-generated spec / schema / class / access, and `#[agent_test]`.

use agent_proto::{Access, EffectClass, ResourceUri, ToolContent};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use agent_tools::prelude::*;
use agent_tools::testing::{call_granted, text_of};
use serde_json::json;

/// Counts lines.
///
/// Second paragraph.
#[tool]
async fn count_lines(file: Read<File>, min: Option<u32>) -> Result<usize> {
    let n = file.text().await?.lines().count();
    Ok(n.max(min.unwrap_or(0) as usize))
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Opts {
    verbose: bool,
}

#[derive(serde::Serialize)]
struct Report {
    ok: bool,
}

/// Copies a file.
#[tool(name = "copy_file")]
async fn copy(src: Read<File>, dst: Write<File>, opts: Opts) -> Result<Report> {
    let t = src.text().await?;
    dst.write_text(&t).await?;
    Ok(Report { ok: opts.verbose })
}

/// Posts.
#[tool]
async fn post(url: Net<Url>, token: Secret<Name>) -> Result<String> {
    let _ = (url, token);
    Ok("x".into())
}

/// Runs.
#[tool]
async fn run_it(cmd: Exec<Cmd>, note: Mem<Key>) -> Result<()> {
    let _ = (cmd, note);
    Ok(())
}

/// Pure, no caps, uses ctx; fails with a string error.
#[tool]
async fn whoami(ctx: &ToolCtx, fail: bool) -> Result<String, String> {
    if fail {
        return Err("boom".into());
    }
    Ok(ctx.session.to_string())
}

/// Optional capability.
#[tool]
async fn maybe(file: Option<Read<File>>) -> Result<ToolOutput> {
    Ok(ToolOutput::text(if file.is_some() {
        "some"
    } else {
        "none"
    }))
}

#[test]
fn spec_and_schema() {
    let s = count_lines.spec();
    assert_eq!(s.name, "count_lines");
    assert_eq!(s.description, "Counts lines.\n\nSecond paragraph.");
    assert_eq!(s.class, EffectClass::Pure);
    assert_eq!(s.input_schema["type"], "object");
    assert_eq!(s.input_schema["properties"]["file"]["type"], "string");
    assert_eq!(s.input_schema["properties"]["min"]["type"], "integer");
    assert_eq!(s.input_schema["required"], json!(["file"]));

    let s = copy.spec();
    assert_eq!(s.name, "copy_file");
    assert_eq!(s.class, EffectClass::LocalWrite);
    assert_eq!(s.input_schema["required"], json!(["src", "dst", "opts"]));
    assert_eq!(
        s.input_schema["properties"]["opts"]["properties"]["verbose"]["type"],
        "boolean"
    );

    assert_eq!(post.spec().class, EffectClass::Network);
    assert_eq!(run_it.spec().class, EffectClass::Opaque);
    assert_eq!(whoami.spec().class, EffectClass::Pure);
    assert_eq!(whoami.spec().input_schema["required"], json!(["fail"]));
    assert!(whoami.spec().input_schema["properties"]
        .get("ctx")
        .is_none());
    assert_eq!(maybe.spec().input_schema["required"], json!([]));
    assert_eq!(copy.class(&json!({})), EffectClass::LocalWrite);
    assert_eq!(copy::CLASS, EffectClass::LocalWrite);
}

#[test]
fn access_declarations() {
    let actx = AccessCtx {
        workspace: "/w".into(),
    };
    assert_eq!(
        copy.access(
            &json!({"src": "a", "dst": "b/c", "opts": {"verbose": true}}),
            &actx
        )
        .unwrap(),
        vec![
            Access::read(ResourceUri::fs("/w/a")),
            Access::write(ResourceUri::fs("/w/b/c"))
        ]
    );
    assert_eq!(
        post.access(
            &json!({"url": "https://api.example.com/x", "token": "GH"}),
            &actx
        )
        .unwrap(),
        vec![
            Access::write(ResourceUri::net("api.example.com", 443)),
            Access::read(ResourceUri::secret("GH"))
        ]
    );
    assert_eq!(
        run_it
            .access(
                &json!({"cmd": "make test", "note": "project/conventions"}),
                &actx
            )
            .unwrap(),
        vec![
            Access::write(ResourceUri::cmd("make test")),
            Access::write(ResourceUri::mem("project/conventions"))
        ]
    );
    assert_eq!(
        whoami.access(&json!({"fail": false}), &actx).unwrap(),
        vec![]
    );
    assert!(matches!(
        whoami.access(&json!({}), &actx),
        Err(ToolError::InvalidInput(_))
    ));
    assert_eq!(maybe.access(&json!({}), &actx).unwrap(), vec![]);
    assert_eq!(
        maybe.access(&json!({"file": "x"}), &actx).unwrap(),
        vec![Access::read(ResourceUri::fs("/w/x"))]
    );
    // Get<Url> is a read, with the scheme's default port.
    let g = agent_tools::web_fetch
        .access(&json!({"url": "http://example.org/a"}), &actx)
        .unwrap();
    assert_eq!(g, vec![Access::read(ResourceUri::net("example.org", 80))]);
    assert_eq!(agent_tools::web_fetch.spec().class, EffectClass::Pure);
    assert!(agent_tools::web_fetch
        .access(&json!({"url": "file:///etc/passwd"}), &actx)
        .is_err());
}

#[agent_tools::agent_test]
async fn generated_call_paths() -> Result<(), ToolError> {
    let w = agent_tools::testing::workspace();
    std::fs::write(w.join("a"), "1\n2\n3\n").unwrap();
    // Serialize -> Json.
    let out = call_granted(&count_lines, json!({"file": "a"}), &w).await?;
    assert_eq!(out.content, vec![ToolContent::Json { value: json!(3) }]);
    assert_eq!(out.observed.len(), 1);
    let out = call_granted(
        &copy,
        json!({"src": "a", "dst": "b", "opts": {"verbose": true}}),
        &w,
    )
    .await?;
    assert_eq!(
        out.content,
        vec![ToolContent::Json {
            value: json!({"ok": true})
        }]
    );
    assert_eq!(std::fs::read_to_string(w.join("b")).unwrap(), "1\n2\n3\n");
    // () -> "ok"
    let out = call_granted(&run_it, json!({"cmd": "true", "note": "k"}), &w).await?;
    assert_eq!(text_of(&out), "ok");
    // String errors -> Failed; ctx injection.
    let out = call_granted(&whoami, json!({"fail": false}), &w).await?;
    assert_eq!(text_of(&out), "test-session");
    let r = call_granted(&whoami, json!({"fail": true}), &w).await;
    assert!(matches!(r, Err(ToolError::Failed(m)) if m == "boom"));
    // Invalid input.
    let r = call_granted(&whoami, json!({"fail": 3}), &w).await;
    assert!(matches!(r, Err(ToolError::InvalidInput(_))));
    let out = call_granted(&maybe, json!({}), &w).await?;
    assert_eq!(text_of(&out), "none");
    Ok(())
}

#[agent_tools::agent_test]
async fn cancellation() {
    let w = agent_tools::testing::workspace();
    let c = agent_tools::testing::ctx(&w, vec![]);
    c.cancel.cancel();
    let r = whoami.call(json!({"fail": false}), c).await;
    assert!(matches!(r, Err(ToolError::Cancelled)));
}

#[test]
fn tools_are_values() {
    // Unit structs: usable as values in tool tuples.
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(count_lines),
        Box::new(agent_tools::edit),
        Box::new(Bash),
    ];
    assert_eq!(tools.len(), 3);
}
