use agent_proto::{Access, ResourceUri};
use agent_runtime::{AccessCtx, Tool, ToolError};
use agent_tools::testing::{call_granted, ctx, text_of};
use agent_tools::{glob, grep, remember, LoadSkill, Recall};
use serde_json::json;

fn ws() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    (d, p)
}

#[tokio::test]
async fn glob_and_grep() {
    let (_d, w) = ws();
    std::fs::create_dir_all(w.join("src/a")).unwrap();
    std::fs::create_dir_all(w.join("target")).unwrap();
    std::fs::write(w.join("src/lib.rs"), "fn main() {}\n// TODO one\n").unwrap();
    std::fs::write(w.join("src/a/b.rs"), "// TODO two\n").unwrap();
    std::fs::write(w.join("README.md"), "TODO three\n").unwrap();
    std::fs::write(w.join("target/x.rs"), "TODO ignored\n").unwrap();
    std::fs::write(w.join(".gitignore"), "target/\n").unwrap();

    let acc = glob
        .access(
            &json!({"dir": ".", "pattern": "**/*.rs"}),
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        acc,
        vec![Access::read(ResourceUri::fs(&format!(
            "{}/**",
            w.display()
        )))]
    );
    let out = call_granted(&glob, json!({"dir": ".", "pattern": "**/*.rs"}), &w)
        .await
        .unwrap();
    assert_eq!(text_of(&out), "src/a/b.rs\nsrc/lib.rs");
    let out = call_granted(&glob, json!({"dir": "src", "pattern": "*.rs"}), &w)
        .await
        .unwrap();
    assert_eq!(text_of(&out), "lib.rs");

    let out = call_granted(&grep, json!({"dir": ".", "pattern": "TODO \\w+"}), &w)
        .await
        .unwrap();
    assert_eq!(
        text_of(&out),
        "README.md:1:TODO three\nsrc/a/b.rs:1:// TODO two\nsrc/lib.rs:2:// TODO one"
    );
    let out = call_granted(
        &grep,
        json!({"dir": ".", "pattern": "TODO", "glob": "**/*.rs"}),
        &w,
    )
    .await
    .unwrap();
    assert_eq!(
        text_of(&out),
        "src/a/b.rs:1:// TODO two\nsrc/lib.rs:2:// TODO one"
    );
    // Grant for src only does not cover the root.
    let r = grep
        .call(
            json!({"dir": ".", "pattern": "x"}),
            ctx(
                &w,
                vec![Access::read(ResourceUri::fs(&format!(
                    "{}/src/**",
                    w.display()
                )))],
            ),
        )
        .await;
    assert!(matches!(r, Err(ToolError::NotGranted(_))));
}

#[tokio::test]
async fn memory_tools() {
    let (_d, w) = ws();
    let acc = remember
        .access(
            &json!({"key": "conventions", "value": "tabs"}),
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        acc,
        vec![Access::write(ResourceUri::mem("project/conventions"))]
    );
    assert_eq!(remember.spec().class, agent_proto::EffectClass::LocalWrite);
    let mut grants = acc;
    grants.push(Access::read(ResourceUri::mem("project/*")));
    let c = ctx(&w, grants);
    remember
        .call(
            json!({"key": "conventions", "value": "use tabs"}),
            c.clone(),
        )
        .await
        .unwrap();
    let out = Recall
        .call(json!({"query": "TABS"}), c.clone())
        .await
        .unwrap();
    assert_eq!(text_of(&out), "project/conventions: use tabs");
    assert_eq!(
        Recall
            .access(
                &json!({"query": "x"}),
                &AccessCtx {
                    workspace: w.clone()
                }
            )
            .unwrap(),
        vec![Access::read(ResourceUri::mem("project/*"))]
    );
    let r = Recall.call(json!({"query": "x", "scope": "user"}), c).await;
    assert!(matches!(r, Err(ToolError::NotGranted(_))));
}

#[tokio::test]
async fn load_skill() {
    let (_d, w) = ws();
    std::fs::create_dir_all(w.join(".agent/skills/deploy")).unwrap();
    std::fs::write(w.join(".agent/skills/deploy/SKILL.md"), "# Deploy\nsteps").unwrap();
    let acc = LoadSkill
        .access(
            &json!({"name": "deploy"}),
            &AccessCtx {
                workspace: w.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        acc,
        vec![Access::read(ResourceUri::fs(&format!(
            "{}/.agent/skills/deploy/SKILL.md",
            w.display()
        )))]
    );
    let out = call_granted(&LoadSkill, json!({"name": "deploy"}), &w)
        .await
        .unwrap();
    assert_eq!(text_of(&out), "# Deploy\nsteps");
    assert!(LoadSkill
        .access(
            &json!({"name": "../x"}),
            &AccessCtx {
                workspace: w.clone()
            }
        )
        .is_err());
    assert!(call_granted(&LoadSkill, json!({"name": "nope"}), &w)
        .await
        .is_err());
}
