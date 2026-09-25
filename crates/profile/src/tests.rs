use crate::*;
use agent_proto::{HookPoint, Layer, OnAsk, PolicyAction};
use proptest::prelude::*;

fn src() -> Sources {
    Sources { project_root: Some("/repo".into()), ..Default::default() }
}

fn trusted_user() -> Option<String> {
    Some("[security]\nworkspace_trusted = true\n".into())
}

fn has_warning(p: &Profile, key: &str) -> bool {
    p.warnings.iter().any(|w| w.key == key)
}

#[test]
fn defaults_compile() {
    let p = compile(&src()).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "scripted");
    assert_eq!(p.kernel.security.workspace_root, "/repo");
    assert!(!p.kernel.security.workspace_trusted);
    assert_eq!(p.explain("model.id").unwrap().layer, Layer::Default);
    assert_eq!(p.hash.len(), 64);
    assert_eq!(p.hash, p.compute_hash());
}

#[test]
fn scalar_priority() {
    let mut s = src();
    s.user = Some("[model]\nid = \"u\"\nwindow = 1000\n".into());
    s.shared_project = Some("[model]\nid = \"shared\"\n".into());
    s.cli = Some("[model]\nid = \"cli\"\n".into());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "cli");
    assert_eq!(p.explain("model.id").unwrap().layer, Layer::Cli);
    assert_eq!(p.kernel.caps.window, 1000);
    assert_eq!(p.explain("model.window").unwrap().layer, Layer::User);
    assert_eq!(p.explain("model.id").unwrap().value, serde_json::json!("cli"));
}

#[test]
fn fallbacks_and_budgets() {
    let mut s = src();
    s.user = Some("[model]\nid = \"a\"\nfallbacks = [\"b\", \"c\"]\n[budgets]\nmax_tokens = 5\n".into());
    let p = compile(&s).unwrap();
    let f: Vec<_> = p.kernel.fallbacks.iter().map(|c| c.model.as_str().to_string()).collect();
    assert_eq!(f, vec!["b", "c"]);
    assert_eq!(p.kernel.budgets.max_tokens, 5);
    assert_eq!(p.kernel.budgets.max_calls_per_turn, 200);
}

#[test]
fn parse_errors_name_the_layer() {
    let mut s = src();
    s.shared_project = Some("[model\n".into());
    assert!(matches!(compile(&s), Err(ConfigError::Parse { layer: Layer::SharedProject, .. })));
    let mut s = src();
    s.user = Some("unknown_key = 1\n".into());
    assert!(matches!(compile(&s), Err(ConfigError::Parse { layer: Layer::User, .. })));
    let mut s = src();
    s.user = Some("[[permissions]]\nname = \"x\"\nresource = \"fs:///[\"\naction = \"deny\"\n".into());
    assert!(matches!(compile(&s), Err(ConfigError::Invalid { .. })));
}

#[test]
fn permissions_merge_deny_first() {
    let mut s = src();
    s.user = Some(
        "[security]\nworkspace_trusted = true\n[[permissions]]\nname = \"allow-src\"\nresource = \"fs:///repo/**\"\naction = \"allow\"\n"
            .into(),
    );
    s.shared_project =
        Some("[[permissions]]\nname = \"no-env\"\nresource = \"fs:///**/.env*\"\nmode = \"read\"\naction = \"deny\"\n".into());
    s.cli = Some("[[permissions]]\nname = \"ask-net\"\nresource = \"net:*\"\naction = \"ask\"\n".into());
    let p = compile(&s).unwrap();
    let names: Vec<_> = p.kernel.rules.iter().map(|r| (r.name.as_str(), r.layer)).collect();
    assert_eq!(
        names,
        vec![("no-env", Layer::SharedProject), ("ask-net", Layer::Cli), ("allow-src", Layer::User)]
    );
}

#[test]
fn untrusted_workspace_drops_project_extensions() {
    let mut s = src();
    s.shared_project = Some(
        r#"
system = ["project prompt"]
[[permissions]]
name = "allow-all"
action = "allow"
[[permissions]]
name = "deny-rm"
resource = "cmd:rm *"
action = "deny"
[[hooks]]
point = "pre_tool"
executor = { command = "./evil.sh" }
[mcp.evil]
command = "evil"
"#
        .into(),
    );
    s.instructions = vec![
        SourceFile::new("/home/u/.agent/AGENTS.md", Scope::User, "user rules"),
        SourceFile::new("/repo/AGENTS.md", Scope::Project, "repo rules"),
    ];
    s.commands = vec![SourceFile::new("/repo/.agent/commands/x.md", Scope::Project, "do x")];
    s.agents = vec![SourceFile::new("/repo/.agent/agents/r.md", Scope::Project, "---\nname: r\n---\nhi")];
    s.skills = vec![SourceFile::new("/repo/.agent/skills/s/SKILL.md", Scope::Project, "---\ndescription: d\n---\n")];
    let p = compile(&s).unwrap();
    assert!(p.hooks.is_empty());
    assert!(p.kernel.hooked.is_empty());
    assert!(p.mcp.is_empty());
    assert!(p.commands.is_empty());
    assert!(p.agents.is_empty());
    assert_eq!(p.kernel.rules.len(), 1);
    assert_eq!(p.kernel.rules[0].name, "deny-rm");
    assert!(!p.kernel.system.iter().any(|x| x.contains("project prompt")));
    assert!(p.instructions[0].trusted);
    assert!(!p.instructions[1].trusted);
    assert!(!p.skills[0].trusted);
    // trusted user instructions go to the static layer, untrusted ones don't
    assert!(p.kernel.system.iter().any(|x| x.contains("user rules")));
    assert!(!p.kernel.system.iter().any(|x| x.contains("repo rules")));
    for k in ["hooks", "mcp", "system", "permissions", "commands", "agents"] {
        assert!(has_warning(&p, k), "{k}: {:?}", p.warnings);
    }
}

#[test]
fn trusted_workspace_enables_project_extensions() {
    let mut s = src();
    s.user = trusted_user();
    s.shared_project = Some(
        r#"
[[hooks]]
name = "fmt"
point = "post_tool"
matcher = "edit*"
executor = { command = "fmt", args = ["-w"] }
[[hooks]]
point = "pre_tool"
executor = { http = "http://localhost:1/hook" }
[mcp.gh]
command = "gh-mcp"
trusted = true
"#
        .into(),
    );
    s.user = Some(
        "[security]\nworkspace_trusted = true\n[mcp.gh]\ncommand = \"user-gh\"\ntrusted = true\n[[hooks]]\npoint = \"stop\"\nexecutor = { mcp = \"srv/tool\" }\n"
            .into(),
    );
    let p = compile(&s).unwrap();
    assert_eq!(p.hooks.len(), 3);
    assert_eq!(p.kernel.hooked, vec![HookPoint::PreTool, HookPoint::PostTool, HookPoint::Stop]);
    assert!(matches!(&p.hooks[0].executor, HookExecutor::Command(c) if c.args == vec!["-w"]));
    assert!(matches!(&p.hooks[1].executor, HookExecutor::Http(_)));
    assert!(matches!(&p.hooks[2].executor, HookExecutor::Mcp(_)));
    // project overrides the user's entry but cannot mark it trusted
    let gh = &p.mcp["gh"];
    assert_eq!(gh.command.as_deref(), Some("gh-mcp"));
    assert!(!gh.trusted);
    assert!(has_warning(&p, "mcp.gh.trusted"));
}

#[test]
fn project_cannot_trust_workspace() {
    let mut s = src();
    s.shared_project = Some("[security]\nworkspace_trusted = true\n".into());
    let p = compile(&s).unwrap();
    assert!(!p.kernel.security.workspace_trusted);
    assert!(has_warning(&p, "security.workspace_trusted"));
    // but can distrust it
    let mut s = src();
    s.user = trusted_user();
    s.local_project = Some("[security]\nworkspace_trusted = false\n".into());
    let p = compile(&s).unwrap();
    assert!(!p.kernel.security.workspace_trusted);
    assert_eq!(p.explain("security.workspace_trusted").unwrap().layer, Layer::LocalProject);
}

#[test]
fn egress_only_narrowed_by_project() {
    let mut s = src();
    s.user = Some("[security]\negress_allow = [\"net:a:443\", \"net:b:443\"]\ntrusted_sources = [\"net:docs:443\"]\n".into());
    s.shared_project = Some(
        "[security]\negress_allow = [\"net:a:443\", \"net:evil:443\"]\ntrusted_sources = [\"net:docs:443\", \"net:evil:443\"]\n"
            .into(),
    );
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.security.egress_allow, vec!["net:a:443"]);
    assert_eq!(p.kernel.security.trusted_sources, vec!["net:docs:443"]);
    assert!(has_warning(&p, "security.egress_allow"));
    assert!(has_warning(&p, "security.trusted_sources"));
    assert_eq!(p.explain("security.egress_allow").unwrap().layer, Layer::SharedProject);
}

#[test]
fn additive_security_lists() {
    let mut s = src();
    s.shared_project = Some("[security]\nprivate = [\"fs:///repo/secrets/**\"]\n".into());
    let p = compile(&s).unwrap();
    assert!(p.kernel.security.private.contains(&"fs:///repo/secrets/**".to_string()));
    assert!(p.kernel.security.private.contains(&"secret:*".to_string()));
}

#[test]
fn unattended_tighten_only() {
    let mut s = src();
    s.shared_project = Some("[unattended]\non_ask = \"allow\"\n".into());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.unattended, None);
    assert!(has_warning(&p, "unattended.on_ask"));

    let mut s = src();
    s.user = Some("[unattended]\non_ask = \"allow\"\n".into());
    s.shared_project = Some("[unattended]\non_ask = \"defer\"\n".into());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.unattended, Some(OnAsk::Defer));

    let mut s = src();
    s.cli = Some("[unattended]\non_ask = \"deny\"\n".into());
    s.local_project = Some("[unattended]\non_ask = \"allow\"\n".into());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.unattended, Some(OnAsk::Deny));
    assert!(has_warning(&p, "unattended.on_ask"));
}

#[test]
fn auto_answer_project_deny_only() {
    let mut s = src();
    s.user = Some("[[auto_answer]]\nrule = \"tests\"\nresource = \"cmd:cargo test*\"\nanswer = \"allow\"\n".into());
    s.shared_project = Some(
        "[[auto_answer]]\nrule = \"all\"\nanswer = \"allow\"\n[[auto_answer]]\nrule = \"no-push\"\nresource = \"cmd:git push*\"\nanswer = \"deny\"\n"
            .into(),
    );
    let p = compile(&s).unwrap();
    let r: Vec<_> = p.auto_answer.iter().map(|r| (r.rule.as_str(), r.answer)).collect();
    assert_eq!(r, vec![("no-push", AutoAnswer::Deny), ("tests", AutoAnswer::Allow)]);
    assert!(has_warning(&p, "auto_answer.all"));
}

#[test]
fn managed_locks() {
    let mut s = src();
    s.managed = Some("locked = [\"model.id\", \"security.egress_allow\", \"permissions\"]\n[model]\nid = \"corp\"\n".into());
    s.cli = Some("[model]\nid = \"mine\"\n[security]\negress_allow = [\"net:x:443\"]\n".into());
    s.user = Some("locked = [\"budgets\"]\n[[permissions]]\nname = \"allow\"\naction = \"allow\"\n".into());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "corp");
    assert_eq!(p.explain("model.id").unwrap().layer, Layer::Managed);
    assert!(p.kernel.security.egress_allow.is_empty());
    assert!(p.kernel.rules.is_empty());
    assert!(has_warning(&p, "model.id"));
    assert!(has_warning(&p, "security.egress_allow"));
    assert!(has_warning(&p, "permissions"));
    assert!(has_warning(&p, "locked"));
}

#[test]
fn cli_settings_struct_roundtrip() {
    let cli = Settings {
        model: Some(ModelSettings { id: Some("x".into()), ..Default::default() }),
        unattended: Some(UnattendedSettings { on_ask: Some(OnAsk::Defer) }),
        plan: Some(PlanSettings { read_only: Some(true) }),
        ..Default::default()
    };
    let mut s = src();
    s.cli = Some(cli.to_toml());
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "x");
    assert_eq!(p.kernel.unattended, Some(OnAsk::Defer));
    assert!(p.kernel.read_only_mode);
}

#[test]
fn skills_commands_agents_and_child() {
    let mut s = src();
    s.user = trusted_user();
    s.skills = vec![
        SourceFile::new("/h/.agent/skills/pdf/SKILL.md", Scope::User, "---\nname: pdf\ndescription: user pdf\n---\nbody"),
        SourceFile::new("/repo/.agent/skills/pdf/SKILL.md", Scope::Project, "---\ndescription: project pdf\n---\nbody"),
    ];
    s.commands = vec![SourceFile::new(
        "/repo/.agent/commands/review.md",
        Scope::Project,
        "---\ndescription: Review\n---\nReview $ARGUMENTS carefully",
    )];
    s.agents = vec![SourceFile::new(
        "/repo/.agent/agents/reviewer.md",
        Scope::Project,
        "---\nname: reviewer\ndescription: reviews diffs\ntools: [read, grep, missing]\nmodel: small\n---\nYou review code.",
    )];
    let p = compile(&s).unwrap();
    assert_eq!(p.skills.len(), 1);
    assert_eq!(p.skills[0].description, "project pdf");
    assert!(p.kernel.system.iter().any(|x| x.contains("pdf: project pdf")));
    assert_eq!(p.command("review").unwrap().expand("a.rs"), "Review a.rs carefully");

    let tool = |n: &str| agent_proto::ToolSpec {
        name: n.into(),
        description: String::new(),
        input_schema: serde_json::json!({}),
        class: agent_proto::EffectClass::Pure,
        subagent: false,
    };
    let p = p.with_tools(vec![tool("read"), tool("edit"), tool("grep")]);
    let c = p.child("reviewer").unwrap();
    let names: Vec<_> = c.kernel.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["read", "grep"]);
    assert_eq!(c.kernel.caps.model.as_str(), "small");
    assert_eq!(c.kernel.system.last().unwrap(), "You review code.");
    assert_eq!(c.kernel.rules, p.kernel.rules);
    assert_ne!(c.hash, p.hash);
    // narrowing survives re-assembly of tools
    let c2 = c.clone().with_tools(vec![tool("read"), tool("edit")]);
    assert_eq!(c2.kernel.tools.len(), 1);
    assert!(p.child("nobody").is_none());
}

#[test]
fn discovery_reads_layers_and_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    let sub = root.join("crates/a");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(root.join(".agent/commands")).unwrap();
    std::fs::create_dir_all(root.join(".agent/skills/lint")).unwrap();
    std::fs::create_dir_all(home.join(".agent/agents")).unwrap();
    std::fs::write(root.join(".agent/settings.toml"), "[model]\nid = \"shared\"\n").unwrap();
    std::fs::write(root.join(".agent/settings.local.toml"), "[budgets]\nmax_tokens = 9\n").unwrap();
    std::fs::write(root.join(".agent/commands/fix.md"), "fix it").unwrap();
    std::fs::write(root.join(".agent/skills/lint/SKILL.md"), "---\nname: lint\ndescription: Lints\n---\n").unwrap();
    std::fs::write(home.join(".agent/settings.toml"), "[security]\nworkspace_trusted = true\n").unwrap();
    std::fs::write(home.join(".agent/agents/helper.md"), "---\ntools: read\n---\nhelp").unwrap();
    std::fs::write(root.join("AGENTS.md"), "root").unwrap();
    std::fs::write(root.join("crates/CLAUDE.md"), "crates").unwrap();
    std::fs::write(sub.join("AGENTS.md"), "leaf").unwrap();
    let managed = tmp.path().join("managed.toml");
    std::fs::write(&managed, "[plan]\nread_only = true\n").unwrap();

    let opts = DiscoverOptions {
        cwd: sub.clone(),
        project_root: None,
        home: Some(home),
        managed_path: Some(managed),
        cli: Some("[model]\nwindow = 10\n".into()),
        tools: vec![],
    };
    let s = discover(&opts).unwrap();
    let texts: Vec<_> = s.instructions.iter().map(|f| f.text.as_str()).collect();
    assert_eq!(texts, vec!["root", "crates", "leaf"]);
    assert!(s.project_root.as_deref().unwrap().ends_with("repo"));
    let p = load(&opts).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "shared");
    assert_eq!(p.kernel.budgets.max_tokens, 9);
    assert_eq!(p.kernel.caps.window, 10);
    assert!(p.kernel.read_only_mode);
    assert!(p.kernel.security.workspace_trusted);
    assert_eq!(p.commands[0].name, "fix");
    assert_eq!(p.skills[0].name, "lint");
    assert_eq!(p.agents[0].name, "helper");
    assert_eq!(p.agents[0].tools.as_deref(), Some(&["read".to_string()][..]));
    assert!(p.instructions.iter().all(|f| f.trusted));
    // discovery is repeatable
    assert_eq!(load(&opts).unwrap().hash, p.hash);
}

#[test]
fn instruction_budget_from_settings() {
    let mut s = src();
    s.user = Some("[instructions]\nmax_bytes = 5\n".into());
    s.instructions = vec![
        SourceFile::new("/repo/AGENTS.md", Scope::Project, "aaaa"),
        SourceFile::new("/repo/sub/AGENTS.md", Scope::Project, "bbbbbbb"),
    ];
    let p = compile(&s).unwrap();
    assert_eq!(p.instructions.len(), 1);
    assert_eq!(p.instructions[0].text, "bbbbb");
    assert!(p.instructions[0].truncated);
    assert!(has_warning(&p, "instructions"));
}

// ------------------------------------------------------------------ properties

const LAYERS: [Layer; 5] = [Layer::Managed, Layer::Cli, Layer::LocalProject, Layer::SharedProject, Layer::User];

fn set_layer(s: &mut Sources, l: Layer, text: String) {
    match l {
        Layer::Managed => s.managed = Some(text),
        Layer::Cli => s.cli = Some(text),
        Layer::LocalProject => s.local_project = Some(text),
        Layer::SharedProject => s.shared_project = Some(text),
        Layer::User => s.user = Some(text),
        Layer::Default => {}
    }
}

fn action_str(a: PolicyAction) -> &'static str {
    match a {
        PolicyAction::Allow => "allow",
        PolicyAction::Ask => "ask",
        PolicyAction::Deny => "deny",
    }
}

fn on_ask_str(a: OnAsk) -> &'static str {
    match a {
        OnAsk::Allow => "allow",
        OnAsk::Defer => "defer",
        OnAsk::Deny => "deny",
    }
}

#[derive(Debug, Clone)]
struct LayerGen {
    model: Option<u8>,
    max_tokens: Option<u64>,
    trusted: Option<bool>,
    egress: Option<Vec<u8>>,
    trusted_sources: Option<Vec<u8>>,
    on_ask: Option<OnAsk>,
    rules: Vec<(u8, PolicyAction)>,
    auto: Vec<(u8, bool)>,
    hook: bool,
}

fn on_ask_gen() -> impl Strategy<Value = OnAsk> {
    prop_oneof![Just(OnAsk::Allow), Just(OnAsk::Defer), Just(OnAsk::Deny)]
}
fn action_gen() -> impl Strategy<Value = PolicyAction> {
    prop_oneof![Just(PolicyAction::Allow), Just(PolicyAction::Ask), Just(PolicyAction::Deny)]
}

fn layer_gen() -> impl Strategy<Value = LayerGen> {
    (
        proptest::option::of(0u8..4),
        proptest::option::of(0u64..100),
        proptest::option::of(any::<bool>()),
        proptest::option::of(proptest::collection::vec(0u8..5, 0..4)),
        proptest::option::of(proptest::collection::vec(0u8..5, 0..4)),
        proptest::option::of(on_ask_gen()),
        proptest::collection::vec((0u8..3, action_gen()), 0..3),
        proptest::collection::vec((0u8..3, any::<bool>()), 0..3),
        any::<bool>(),
    )
        .prop_map(|(model, max_tokens, trusted, egress, trusted_sources, on_ask, rules, auto, hook)| LayerGen {
            model,
            max_tokens,
            trusted,
            egress,
            trusted_sources,
            on_ask,
            rules,
            auto,
            hook,
        })
}

fn render(g: &LayerGen) -> String {
    let list = |v: &Vec<u8>| v.iter().map(|x| format!("\"net:h{x}:443\"")).collect::<Vec<_>>().join(", ");
    let mut t = String::new();
    if let Some(m) = g.model {
        t += &format!("[model]\nid = \"m{m}\"\n");
    }
    if let Some(b) = g.max_tokens {
        t += &format!("[budgets]\nmax_tokens = {b}\n");
    }
    t += "[security]\n";
    if let Some(b) = g.trusted {
        t += &format!("workspace_trusted = {b}\n");
    }
    if let Some(e) = &g.egress {
        t += &format!("egress_allow = [{}]\n", list(e));
    }
    if let Some(e) = &g.trusted_sources {
        t += &format!("trusted_sources = [{}]\n", list(e));
    }
    if let Some(a) = g.on_ask {
        t += &format!("[unattended]\non_ask = \"{}\"\n", on_ask_str(a));
    }
    for (i, (r, a)) in g.rules.iter().enumerate() {
        t += &format!("[[permissions]]\nname = \"r{i}\"\nresource = \"fs:///res{r}/**\"\naction = \"{}\"\n", action_str(*a));
    }
    for (i, (r, allow)) in g.auto.iter().enumerate() {
        t += &format!(
            "[[auto_answer]]\nrule = \"a{i}\"\nresource = \"cmd:c{r}*\"\nanswer = \"{}\"\n",
            if *allow { "allow" } else { "deny" }
        );
    }
    if g.hook {
        t += "[[hooks]]\npoint = \"pre_tool\"\nexecutor = { command = \"h\" }\n";
    }
    t
}

fn sources_of(gens: &[Option<LayerGen>; 5]) -> Sources {
    let mut s = src();
    for (l, g) in LAYERS.iter().zip(gens.iter()) {
        if let Some(g) = g {
            set_layer(&mut s, *l, render(g));
        }
    }
    s
}

fn layers_gen() -> impl Strategy<Value = [Option<LayerGen>; 5]> {
    [
        proptest::option::of(layer_gen()),
        proptest::option::of(layer_gen()),
        proptest::option::of(layer_gen()),
        proptest::option::of(layer_gen()),
        proptest::option::of(layer_gen()),
    ]
}

/// The sensitive projection of a profile computed with project layers removed.
fn without_project(gens: &[Option<LayerGen>; 5]) -> Profile {
    let mut g = gens.clone();
    g[2] = None;
    g[3] = None;
    compile(&sources_of(&g)).unwrap()
}

fn subset(a: &[String], b: &[String]) -> bool {
    a.iter().all(|x| b.contains(x))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_scalar_override_priority(gens in layers_gen()) {
        let p = compile(&sources_of(&gens)).unwrap();
        let expected = LAYERS.iter().zip(gens.iter())
            .find_map(|(l, g)| g.as_ref().and_then(|g| g.model).map(|m| (*l, format!("m{m}"))));
        match expected {
            Some((l, m)) => {
                prop_assert_eq!(p.kernel.caps.model.as_str(), m.as_str());
                prop_assert_eq!(p.explain("model.id").unwrap().layer, l);
            }
            None => prop_assert_eq!(p.explain("model.id").unwrap().layer, Layer::Default),
        }
        let exp_tokens = gens.iter().find_map(|g| g.as_ref().and_then(|g| g.max_tokens)).unwrap_or(0);
        prop_assert_eq!(p.kernel.budgets.max_tokens, exp_tokens);
    }

    #[test]
    fn prop_every_deny_survives_and_comes_first(gens in layers_gen()) {
        let p = compile(&sources_of(&gens)).unwrap();
        let n_deny = gens.iter().flatten().map(|g| g.rules.iter().filter(|(_, a)| *a == PolicyAction::Deny).count()).sum::<usize>();
        let denies = p.kernel.rules.iter().filter(|r| r.action == PolicyAction::Deny).count();
        prop_assert_eq!(denies, n_deny);
        // deny rules are ordered before every non-deny rule
        let first_non_deny = p.kernel.rules.iter().position(|r| r.action != PolicyAction::Deny).unwrap_or(p.kernel.rules.len());
        prop_assert!(p.kernel.rules[first_non_deny..].iter().all(|r| r.action != PolicyAction::Deny));
        // every compiled rule carries the layer it came from
        for r in &p.kernel.rules { prop_assert!(r.layer != Layer::Default); }
    }

    #[test]
    fn prop_project_layers_never_loosen_sensitive(gens in layers_gen()) {
        let with = compile(&sources_of(&gens)).unwrap();
        let base = without_project(&gens);
        let (ws, bs) = (&with.kernel.security, &base.kernel.security);
        prop_assert!(subset(&ws.egress_allow, &bs.egress_allow));
        prop_assert!(subset(&ws.trusted_sources, &bs.trusted_sources));
        prop_assert!(!ws.workspace_trusted || bs.workspace_trusted);
        match (with.kernel.unattended, base.kernel.unattended) {
            (None, None) => {}
            (Some(w), Some(b)) => prop_assert!(on_ask_strictness(w) >= on_ask_strictness(b)),
            (w, b) => prop_assert!(false, "unattended changed from {:?} to {:?}", b, w),
        }
        // allow auto-answers only come from trusted layers
        for r in &with.auto_answer {
            if r.answer == AutoAnswer::Allow { prop_assert!(accepts_sensitive(r.layer)); }
        }
        let base_allows = base.auto_answer.iter().filter(|r| r.answer == AutoAnswer::Allow).count();
        let with_allows = with.auto_answer.iter().filter(|r| r.answer == AutoAnswer::Allow).count();
        prop_assert_eq!(base_allows, with_allows);
        // untrusted workspace: no project hooks, no non-deny project rules
        if !ws.workspace_trusted {
            prop_assert!(with.hooks.iter().all(|h| !is_project(h.layer)));
            prop_assert!(with.kernel.rules.iter().all(|r| !is_project(r.layer) || r.action == PolicyAction::Deny));
        }
    }

    #[test]
    fn prop_deterministic(gens in layers_gen()) {
        let s = sources_of(&gens);
        let a = compile(&s).unwrap();
        let b = compile(&s.clone()).unwrap();
        prop_assert_eq!(&a.hash, &b.hash);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(a.compute_hash(), a.hash.clone());
    }
}

#[test]
fn documented_example_compiles() {
    let src_text = include_str!("settings.rs");
    let mut in_block = false;
    let mut toml_text = String::new();
    for l in src_text.lines() {
        let l = l.strip_prefix("//!").unwrap_or("");
        let l = l.strip_prefix(' ').unwrap_or(l);
        if l.starts_with("```toml") {
            in_block = true;
        } else if l.starts_with("```") {
            in_block = false;
        } else if in_block {
            toml_text.push_str(l);
            toml_text.push('\n');
        }
    }
    let mut s = src();
    s.managed = Some(toml_text);
    let p = compile(&s).unwrap();
    assert_eq!(p.kernel.caps.model.as_str(), "claude-sonnet-5");
    assert_eq!(p.kernel.unattended, Some(OnAsk::Defer));
    assert!(p.kernel.read_only_mode);
    assert_eq!(p.hooks.len(), 1);
    assert!(p.mcp.contains_key("github"));
}
