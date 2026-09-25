use agent_proto::{Access, AccessMode, ResourceUri};
use agent_runtime::{AccessCtx, Tool, ToolError};
use agent_tools::caps::sha256_hex;
use agent_tools::testing::{call_granted, ctx, text_of};
use agent_tools::{edit, read, write, Capability, File, Observations, Read, Write};
use serde_json::json;
use std::path::Path;

fn ws() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    (d, p)
}

fn fs(p: &Path) -> ResourceUri {
    ResourceUri::fs(p.to_str().unwrap())
}

#[tokio::test]
async fn access_resolves_relative_paths() {
    let (_d, w) = ws();
    let a = read
        .access(
            &json!({"file": "src/../a.txt"}),
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    assert_eq!(a, vec![Access::read(fs(&w.join("a.txt")))]);
    let a = edit
        .access(
            &json!({"file": "/etc/x", "old": "a", "new": "b"}),
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    assert_eq!(a, vec![Access::write(ResourceUri::fs("/etc/x"))]);
    assert!(matches!(
        read.access(&json!({}), &AccessCtx { workspace: w }),
        Err(ToolError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn grants_are_enforced() {
    let (_d, w) = ws();
    std::fs::write(w.join("a.txt"), "hello\nworld\n").unwrap();
    // No grants.
    let r = read.call(json!({"file": "a.txt"}), ctx(&w, vec![])).await;
    assert!(matches!(r, Err(ToolError::NotGranted(_))), "{r:?}");
    // A read grant does not allow writes.
    let r = write
        .call(
            json!({"file": "a.txt", "content": "x"}),
            ctx(&w, vec![Access::read(fs(&w.join("a.txt")))]),
        )
        .await;
    assert!(matches!(r, Err(ToolError::NotGranted(_))), "{r:?}");
    // Grant for another file.
    let r = read
        .call(
            json!({"file": "a.txt"}),
            ctx(&w, vec![Access::read(fs(&w.join("b.txt")))]),
        )
        .await;
    assert!(matches!(r, Err(ToolError::NotGranted(_))));
    // A glob grant covers it.
    let out = read
        .call(
            json!({"file": "a.txt"}),
            ctx(
                &w,
                vec![Access::read(ResourceUri::fs(&format!(
                    "{}/**",
                    w.display()
                )))],
            ),
        )
        .await
        .unwrap();
    assert_eq!(text_of(&out), "     1\thello\n     2\tworld\n");
    // Observed hash recorded.
    assert_eq!(
        out.observed,
        vec![Access {
            resource: fs(&w.join("a.txt")),
            mode: AccessMode::Read,
            content_hash: Some(sha256_hex(b"hello\nworld\n"))
        }]
    );
}

#[tokio::test]
async fn read_offset_limit() {
    let (_d, w) = ws();
    let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    std::fs::write(w.join("f"), body).unwrap();
    let out = call_granted(&read, json!({"file": "f", "offset": 3, "limit": 2}), &w)
        .await
        .unwrap();
    assert_eq!(
        text_of(&out),
        "     3\tl3\n     4\tl4\n(showing lines 3-4 of 10)\n"
    );
}

#[tokio::test]
async fn write_creates_dirs_and_edit_requires_uniqueness() {
    let (_d, w) = ws();
    call_granted(
        &write,
        json!({"file": "sub/dir/f.txt", "content": "foo bar foo"}),
        &w,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(w.join("sub/dir/f.txt")).unwrap(),
        "foo bar foo"
    );
    let r = call_granted(
        &edit,
        json!({"file": "sub/dir/f.txt", "old": "foo", "new": "x"}),
        &w,
    )
    .await;
    assert!(
        matches!(&r, Err(ToolError::Failed(m)) if m.contains("2 times")),
        "{r:?}"
    );
    let r = call_granted(
        &edit,
        json!({"file": "sub/dir/f.txt", "old": "baz", "new": "x"}),
        &w,
    )
    .await;
    assert!(
        matches!(&r, Err(ToolError::Failed(m)) if m.contains("not found")),
        "{r:?}"
    );
    let out = call_granted(
        &edit,
        json!({"file": "sub/dir/f.txt", "old": "bar", "new": "BAR"}),
        &w,
    )
    .await
    .unwrap();
    assert_eq!(text_of(&out), "ok");
    assert_eq!(
        std::fs::read_to_string(w.join("sub/dir/f.txt")).unwrap(),
        "foo BAR foo"
    );
    // The new content hash is recorded.
    assert_eq!(
        out.observed[0].content_hash.as_deref(),
        Some(sha256_hex(b"foo BAR foo").as_str())
    );
}

#[tokio::test]
async fn stale_writes_are_rejected() {
    let (_d, w) = ws();
    std::fs::write(w.join("f"), "one").unwrap();
    let grant = |h: &str| {
        vec![Access {
            resource: fs(&w.join("f")),
            mode: AccessMode::Write,
            content_hash: Some(h.to_string()),
        }]
    };
    let r = edit
        .call(
            json!({"file": "f", "old": "one", "new": "two"}),
            ctx(&w, grant(&sha256_hex(b"older"))),
        )
        .await;
    assert!(matches!(r, Err(ToolError::Stale(_))), "{r:?}");
    edit.call(
        json!({"file": "f", "old": "one", "new": "two"}),
        ctx(&w, grant(&sha256_hex(b"one"))),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(w.join("f")).unwrap(), "two");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_is_blocked() {
    let (_d, w) = ws();
    let (_o, outside) = ws();
    std::fs::write(outside.join("secret"), "s3cret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), w.join("link")).unwrap();
    std::os::unix::fs::symlink(&outside, w.join("dirlink")).unwrap();
    // Symlinked file pointing outside.
    let r = call_granted(&read, json!({"file": "link"}), &w).await;
    assert!(r.is_err(), "{r:?}");
    // Through a symlinked directory.
    let r = call_granted(&read, json!({"file": "dirlink/secret"}), &w).await;
    assert!(r.is_err(), "{r:?}");
    let r = call_granted(&write, json!({"file": "dirlink/new", "content": "x"}), &w).await;
    assert!(r.is_err(), "{r:?}");
    assert!(!outside.join("new").exists());
    let r = call_granted(&write, json!({"file": "link", "content": "x"}), &w).await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(outside.join("secret")).unwrap(),
        "s3cret"
    );
    // A symlink that stays inside the workspace is fine for reads.
    std::fs::write(w.join("real"), "inside").unwrap();
    std::os::unix::fs::symlink(w.join("real"), w.join("inlink")).unwrap();
    let out = call_granted(&read, json!({"file": "inlink"}), &w).await;
    // openat2 follows it; the O_NOFOLLOW fallback refuses it. Either is safe.
    if let Ok(out) = out {
        assert!(text_of(&out).contains("inside"));
    }
}

#[tokio::test]
async fn handles_bind_and_check() {
    let (_d, w) = ws();
    std::fs::write(w.join("x"), "data").unwrap();
    let mut h: Read<File> = serde_json::from_value(json!("x")).unwrap();
    let obs = Observations::default();
    assert!(h.bind(&ctx(&w, vec![]), &obs).is_err());
    h.bind(&ctx(&w, vec![Access::read(fs(&w.join("x")))]), &obs)
        .unwrap();
    assert_eq!(h.text().await.unwrap(), "data");
    // Unbound handles cannot be used.
    let u: Write<File> = Write::new("x");
    assert!(matches!(u.write_text("y").await, Err(ToolError::Infra(_))));
    assert_eq!(<Read<File> as Capability>::schema()["type"], "string");
}
