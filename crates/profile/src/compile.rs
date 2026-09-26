//! `compile(&Sources) -> Profile`: the pure layered merge.

use crate::frontmatter;
use crate::instructions::{apply_budget, InstructionFile, DEFAULT_INSTRUCTION_BUDGET};
use crate::profile::*;
use crate::settings::*;
use crate::sources::{Scope, SourceFile, Sources};
use agent_proto::{
    Budgets, CompactionConfig, KernelConfig, Layer, ModelCaps, ModelId, OnAsk, PolicyAction, PolicyRule,
    SecurityConfig, SnapshotRule,
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{layer:?} settings: {message}")]
    Parse { layer: Layer, message: String },
    #[error("{layer:?} settings: invalid {key}: {message}")]
    Invalid { layer: Layer, key: String, message: String },
}

/// Layers that may set sensitive fields freely.
pub fn accepts_sensitive(layer: Layer) -> bool {
    matches!(layer, Layer::Managed | Layer::Cli | Layer::User)
}

pub fn is_project(layer: Layer) -> bool {
    matches!(layer, Layer::LocalProject | Layer::SharedProject)
}

/// Strictness of an unattended fallback (higher = stricter).
pub fn on_ask_strictness(a: OnAsk) -> u8 {
    match a {
        OnAsk::Allow => 0,
        OnAsk::Defer => 1,
        OnAsk::Deny => 2,
    }
}

/// Compile the sources into a profile. Pure: no IO, no clock, no env.
pub fn compile(sources: &Sources) -> Result<Profile, ConfigError> {
    Compiler::new(sources)?.run(sources)
}

struct Compiler {
    /// Priority order: Managed, Cli, LocalProject, SharedProject, User.
    layers: Vec<(Layer, Settings)>,
    warnings: Vec<Warning>,
    explain: BTreeMap<String, Explained>,
}

fn warn(ws: &mut Vec<Warning>, layer: Layer, key: &str, message: impl Into<String>) {
    ws.push(Warning { layer, key: key.to_string(), message: message.into() });
}

/// Remove a dotted path from a TOML table; true if something was removed.
fn remove_path(t: &mut toml::Table, path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = match parts.pop() {
        Some(l) => l,
        None => return false,
    };
    let mut cur = t;
    for p in parts {
        match cur.get_mut(p) {
            Some(toml::Value::Table(inner)) => cur = inner,
            _ => return false,
        }
    }
    cur.remove(last).is_some()
}

fn check_glob(layer: Layer, key: &str, g: &Option<String>) -> Result<(), ConfigError> {
    if let Some(g) = g {
        globset::Glob::new(g).map_err(|e| ConfigError::Invalid {
            layer,
            key: key.to_string(),
            message: e.to_string(),
        })?;
    }
    Ok(())
}

fn push_unique(v: &mut Vec<String>, items: &[String]) {
    for i in items {
        if !v.contains(i) {
            v.push(i.clone());
        }
    }
}

fn json<T: Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

impl Compiler {
    fn new(sources: &Sources) -> Result<Self, ConfigError> {
        let raw = [
            (Layer::Managed, &sources.managed),
            (Layer::Cli, &sources.cli),
            (Layer::LocalProject, &sources.local_project),
            (Layer::SharedProject, &sources.shared_project),
            (Layer::User, &sources.user),
        ];
        let mut tables = vec![];
        for (layer, text) in raw {
            let t: toml::Table = match text {
                Some(s) => toml::from_str(s).map_err(|e| ConfigError::Parse { layer, message: e.to_string() })?,
                None => toml::Table::new(),
            };
            tables.push((layer, t));
        }
        let mut warnings = vec![];
        // Managed locks: strip locked keys from every other layer.
        let locked: Vec<String> = tables[0]
            .1
            .get("locked")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        for (layer, t) in tables.iter_mut().skip(1) {
            if t.remove("locked").is_some() {
                warn(&mut warnings, *layer, "locked", "only the managed layer can lock fields; ignored");
            }
            for key in &locked {
                if remove_path(t, key) {
                    warn(&mut warnings, *layer, key, "locked by managed policy; ignored");
                }
            }
        }
        let mut layers = vec![];
        for (layer, t) in tables {
            let s: Settings = toml::Value::Table(t)
                .try_into()
                .map_err(|e: toml::de::Error| ConfigError::Parse { layer, message: e.to_string() })?;
            layers.push((layer, s));
        }
        Ok(Compiler { layers, warnings, explain: BTreeMap::new() })
    }

    fn warn(&mut self, layer: Layer, key: &str, message: impl Into<String>) {
        warn(&mut self.warnings, layer, key, message);
    }

    fn record<T: Serialize>(&mut self, key: &str, value: &T, layer: Layer) {
        self.explain.insert(key.to_string(), Explained { value: json(value), layer });
    }

    /// Plain scalar: the highest-priority layer that sets it wins.
    fn pick<T: Clone + Serialize>(&mut self, key: &str, default: T, get: impl Fn(&Settings) -> Option<T>) -> T {
        let found = self.layers.iter().find_map(|(l, s)| get(s).map(|v| (*l, v)));
        let (layer, v) = found.unwrap_or((Layer::Default, default));
        self.record(key, &v, layer);
        v
    }

    /// Sensitive boolean where `true` is the looser value: only trusted layers
    /// may set it; project layers may set it to `false` only.
    fn pick_sensitive_flag(&mut self, key: &str, get: impl Fn(&Settings) -> Option<bool>) -> bool {
        let base = self.layers.iter().find(|(l, s)| accepts_sensitive(*l) && get(s).is_some());
        let (mut layer, mut v) = base.map(|(l, s)| (*l, get(s).unwrap())).unwrap_or((Layer::Default, false));
        let project: Vec<(Layer, bool)> =
            self.layers.iter().filter(|(l, _)| is_project(*l)).filter_map(|(l, s)| get(s).map(|b| (*l, b))).collect();
        for (pl, pv) in project {
            if !pv && v {
                v = false;
                layer = pl;
            } else if pv && !v {
                self.warn(pl, key, "sensitive field: project layers cannot loosen it; ignored");
            }
        }
        self.record(key, &v, layer);
        v
    }

    /// Sensitive allowlist: set by the highest trusted layer; project layers
    /// may only remove entries (their list is intersected).
    fn pick_sensitive_list(&mut self, key: &str, get: impl Fn(&Settings) -> Option<Vec<String>>) -> Vec<String> {
        let base = self.layers.iter().find_map(|(l, s)| if accepts_sensitive(*l) { get(s).map(|v| (*l, v)) } else { None });
        let (mut layer, mut v) = base.unwrap_or((Layer::Default, vec![]));
        let project: Vec<(Layer, Vec<String>)> =
            self.layers.iter().filter(|(l, _)| is_project(*l)).filter_map(|(l, s)| get(s).map(|b| (*l, b))).collect();
        for (pl, pv) in project {
            let extra: Vec<&String> = pv.iter().filter(|x| !v.contains(x)).collect();
            if !extra.is_empty() {
                self.warn(
                    pl,
                    key,
                    format!("sensitive field: project layers can only remove entries; ignored {extra:?}"),
                );
            }
            let before = v.len();
            v.retain(|x| pv.contains(x));
            if v.len() != before {
                layer = pl;
            }
        }
        self.record(key, &v, layer);
        v
    }

    fn run(mut self, src: &Sources) -> Result<Profile, ConfigError> {
        // ---- workspace trust (sensitive: decides what project config may do)
        let trusted = self.pick_sensitive_flag("security.workspace_trusted", |s| {
            s.security.as_ref().and_then(|x| x.workspace_trusted)
        });

        // ---- model
        let def_caps = ModelCaps::default();
        let mut caps = def_caps.clone();
        caps.model = ModelId::new(self.pick("model.id", def_caps.model.0.clone(), |s| {
            s.model.as_ref().and_then(|m| m.id.clone())
        }));
        caps.window = self.pick("model.window", def_caps.window, |s| s.model.as_ref().and_then(|m| m.window));
        caps.max_output =
            self.pick("model.max_output", def_caps.max_output, |s| s.model.as_ref().and_then(|m| m.max_output));
        caps.thinking = self.pick("model.thinking", def_caps.thinking, |s| s.model.as_ref().and_then(|m| m.thinking));
        caps.parallel_tools = self.pick("model.parallel_tools", def_caps.parallel_tools, |s| {
            s.model.as_ref().and_then(|m| m.parallel_tools)
        });
        caps.images = self.pick("model.images", def_caps.images, |s| s.model.as_ref().and_then(|m| m.images));
        let fallbacks: Vec<String> =
            self.pick("model.fallbacks", vec![], |s| s.model.as_ref().and_then(|m| m.fallbacks.clone()));
        let fallbacks = fallbacks
            .into_iter()
            .map(|id| ModelCaps { model: ModelId::new(id), ..caps.clone() })
            .collect();
        let encoder_version =
            self.pick("model.encoder_version", 1u32, |s| s.model.as_ref().and_then(|m| m.encoder_version));

        // ---- system prompt additions (lowest priority first, managed last)
        let mut system = vec![];
        let mut sys_layer = Layer::Default;
        let mut drop_notes = vec![];
        for (l, s) in self.layers.iter().rev() {
            if let Some(add) = &s.system {
                if is_project(*l) && !trusted {
                    drop_notes.push((*l, "system"));
                    continue;
                }
                system.extend(add.iter().cloned());
                sys_layer = *l;
            }
        }
        self.record("system", &system, sys_layer);

        // ---- permissions: merged across layers, deny first
        let mut rules = vec![];
        for (l, s) in &self.layers {
            for p in &s.permissions {
                check_glob(*l, &format!("permissions.{}.resource", p.name), &p.resource)?;
                check_glob(*l, &format!("permissions.{}.tool", p.name), &p.tool)?;
                if is_project(*l) && !trusted && p.action != PolicyAction::Deny {
                    drop_notes.push((*l, "permissions"));
                    continue;
                }
                rules.push(PolicyRule {
                    name: p.name.clone(),
                    resource: p.resource.clone(),
                    tool: p.tool.clone(),
                    mode: p.mode,
                    action: p.action,
                    layer: *l,
                });
            }
        }
        // Stable: Deny, then Ask, then Allow; within an action by layer priority.
        rules.sort_by_key(|r| (std::cmp::Reverse(r.action), r.layer));

        // ---- budgets
        let db = Budgets::default();
        let budgets = Budgets {
            max_tokens: self.pick("budgets.max_tokens", db.max_tokens, |s| s.budgets.as_ref().and_then(|b| b.max_tokens)),
            max_cost_micros: self.pick("budgets.max_cost_micros", db.max_cost_micros, |s| {
                s.budgets.as_ref().and_then(|b| b.max_cost_micros)
            }),
            max_turn_ms: self
                .pick("budgets.max_turn_ms", db.max_turn_ms, |s| s.budgets.as_ref().and_then(|b| b.max_turn_ms)),
            max_calls_per_turn: self.pick("budgets.max_calls_per_turn", db.max_calls_per_turn, |s| {
                s.budgets.as_ref().and_then(|b| b.max_calls_per_turn)
            }),
            max_repeat_calls: self.pick("budgets.max_repeat_calls", db.max_repeat_calls, |s| {
                s.budgets.as_ref().and_then(|b| b.max_repeat_calls)
            }),
            max_continuations: self.pick("budgets.max_continuations", db.max_continuations, |s| {
                s.budgets.as_ref().and_then(|b| b.max_continuations)
            }),
        };

        // ---- security
        let ds = SecurityConfig::default();
        let mut security = SecurityConfig {
            workspace_root: src.project_root.clone().unwrap_or(ds.workspace_root.clone()),
            workspace_trusted: trusted,
            ..ds.clone()
        };
        security.egress_allow =
            self.pick_sensitive_list("security.egress_allow", |s| s.security.as_ref().and_then(|x| x.egress_allow.clone()));
        security.trusted_sources = self.pick_sensitive_list("security.trusted_sources", |s| {
            s.security.as_ref().and_then(|x| x.trusted_sources.clone())
        });
        security.disposable_env =
            self.pick_sensitive_flag("security.disposable_env", |s| s.security.as_ref().and_then(|x| x.disposable_env));
        // Additive lists: every layer may add (adding is a tightening).
        for (key, sel) in [
            ("security.private", 0usize),
            ("security.untrusted", 1),
            ("security.persistence", 2),
        ] {
            let mut layer = Layer::Default;
            for (l, s) in self.layers.iter().rev() {
                let Some(x) = &s.security else { continue };
                let items = match sel {
                    0 => &x.private,
                    1 => &x.untrusted,
                    _ => &x.persistence,
                };
                if let Some(items) = items {
                    let target = match sel {
                        0 => &mut security.private,
                        1 => &mut security.untrusted,
                        _ => &mut security.persistence,
                    };
                    push_unique(target, items);
                    layer = *l;
                }
            }
            let v = match sel {
                0 => &security.private,
                1 => &security.untrusted,
                _ => &security.persistence,
            }
            .clone();
            self.record(key, &v, layer);
        }

        // ---- unattended (sensitive: project layers only tighten)
        let base = self
            .layers
            .iter()
            .find_map(|(l, s)| if accepts_sensitive(*l) { s.unattended.as_ref().and_then(|u| u.on_ask).map(|a| (*l, a)) } else { None });
        let (mut ua_layer, mut unattended) = match base {
            Some((l, a)) => (l, Some(a)),
            None => (Layer::Default, None),
        };
        let project: Vec<(Layer, OnAsk)> = self
            .layers
            .iter()
            .filter(|(l, _)| is_project(*l))
            .filter_map(|(l, s)| s.unattended.as_ref().and_then(|u| u.on_ask).map(|a| (*l, a)))
            .collect();
        for (pl, pa) in project {
            match unattended {
                None => self.warn(pl, "unattended.on_ask", "sensitive field: project layers cannot enable unattended mode; ignored"),
                Some(cur) if on_ask_strictness(pa) > on_ask_strictness(cur) => {
                    unattended = Some(pa);
                    ua_layer = pl;
                }
                Some(cur) if on_ask_strictness(pa) < on_ask_strictness(cur) => {
                    self.warn(pl, "unattended.on_ask", "sensitive field: project layers can only make it stricter; ignored")
                }
                Some(_) => {}
            }
        }
        self.record("unattended.on_ask", &unattended, ua_layer);

        // ---- auto-answer rules (sensitive: project layers may add deny only)
        let mut auto_answer = vec![];
        for (l, s) in &self.layers {
            for r in &s.auto_answer {
                check_glob(*l, &format!("auto_answer.{}.resource", r.rule), &r.resource)?;
                check_glob(*l, &format!("auto_answer.{}.tool", r.rule), &r.tool)?;
                if is_project(*l) && r.answer == AutoAnswer::Allow {
                    let key = format!("auto_answer.{}", r.rule);
                    warn(
                        &mut self.warnings,
                        *l,
                        &key,
                        "sensitive field: project layers cannot add allow auto-answers; ignored",
                    );
                    continue;
                }
                auto_answer.push(AutoAnswerRule {
                    rule: r.rule.clone(),
                    resource: r.resource.clone(),
                    tool: r.tool.clone(),
                    answer: r.answer,
                    layer: *l,
                });
            }
        }
        // Deny rules first so any deny wins over an allow for the same ask.
        auto_answer.sort_by_key(|r| (std::cmp::Reverse(r.answer), r.layer));

        // ---- hooks
        let mut hooks = vec![];
        for (l, s) in &self.layers {
            for (i, h) in s.hooks.iter().enumerate() {
                check_glob(*l, &format!("hooks[{i}].matcher"), &h.matcher)?;
                if is_project(*l) && !trusted {
                    drop_notes.push((*l, "hooks"));
                    continue;
                }
                hooks.push(HookDef {
                    name: h.name.clone().unwrap_or_else(|| format!("{:?}#{i}", l).to_lowercase()),
                    point: h.point,
                    matcher: h.matcher.clone(),
                    executor: h.executor.clone(),
                    timeout_ms: h.timeout_ms,
                    layer: *l,
                });
            }
        }
        let mut hooked: Vec<_> = hooks.iter().map(|h| h.point).collect();
        hooked.sort();
        hooked.dedup();

        // ---- MCP servers (by name; higher layers replace the whole entry)
        let mut mcp = BTreeMap::new();
        let mut mcp_warn = vec![];
        for (l, s) in self.layers.iter().rev() {
            for (name, m) in &s.mcp {
                if is_project(*l) && !trusted {
                    drop_notes.push((*l, "mcp"));
                    continue;
                }
                if m.command.is_none() && m.url.is_none() {
                    return Err(ConfigError::Invalid {
                        layer: *l,
                        key: format!("mcp.{name}"),
                        message: "needs `command` or `url`".into(),
                    });
                }
                let mut trusted_srv = m.trusted;
                if is_project(*l) && m.trusted {
                    trusted_srv = false;
                    mcp_warn.push((*l, format!("mcp.{name}.trusted")));
                }
                mcp.insert(
                    name.clone(),
                    McpServer {
                        command: m.command.clone(),
                        args: m.args.clone(),
                        env: m.env.clone(),
                        url: m.url.clone(),
                        trusted: trusted_srv,
                        layer: *l,
                    },
                );
            }
        }
        for (l, k) in mcp_warn {
            self.warn(l, &k, "sensitive field: project layers cannot mark servers trusted; ignored");
        }

        // ---- snapshots (by key, higher layer wins)
        let mut snaps: BTreeMap<String, SnapshotRule> = BTreeMap::new();
        for (_, s) in self.layers.iter().rev() {
            for r in &s.snapshots {
                snaps.insert(r.key.clone(), r.clone());
            }
        }

        // ---- compaction
        let dc = CompactionConfig::default();
        let compaction = CompactionConfig {
            pressure_ratio: self.pick("compaction.pressure_ratio", dc.pressure_ratio, |s| {
                s.compaction.as_ref().and_then(|c| c.pressure_ratio)
            }),
            output_reserve: self.pick("compaction.output_reserve", dc.output_reserve, |s| {
                s.compaction.as_ref().and_then(|c| c.output_reserve)
            }),
            keep_recent_tokens: self.pick("compaction.keep_recent_tokens", dc.keep_recent_tokens, |s| {
                s.compaction.as_ref().and_then(|c| c.keep_recent_tokens)
            }),
            instruction: self.pick("compaction.instruction", dc.instruction.clone(), |s| {
                s.compaction.as_ref().and_then(|c| c.instruction.clone())
            }),
        };
        if !(compaction.pressure_ratio > 0.0 && compaction.pressure_ratio <= 1.0) {
            let layer = self.explain["compaction.pressure_ratio"].layer;
            return Err(ConfigError::Invalid {
                layer,
                key: "compaction.pressure_ratio".into(),
                message: "must be in (0, 1]".into(),
            });
        }

        // ---- sandbox / plan / instructions budget
        let sandbox = SandboxPrefs {
            prefer: self.pick("sandbox.prefer", None, |s| s.sandbox.as_ref().map(|x| x.prefer.clone()).filter(Option::is_some)),
            require: self.pick("sandbox.require", false, |s| s.sandbox.as_ref().and_then(|x| x.require)),
        };
        let read_only = self.pick("plan.read_only", false, |s| s.plan.as_ref().and_then(|p| p.read_only));
        let max_bytes = self.pick("instructions.max_bytes", DEFAULT_INSTRUCTION_BUDGET, |s| {
            s.instructions.as_ref().and_then(|i| i.max_bytes)
        });

        // Report untrusted-workspace drops once per (layer, key).
        drop_notes.sort();
        drop_notes.dedup();
        for (l, k) in drop_notes {
            self.warn(l, k, "workspace is not trusted: project configuration does not take effect");
        }

        // ---- instructions
        let files: Vec<InstructionFile> = src
            .instructions
            .iter()
            .map(|f| InstructionFile {
                path: f.path.clone(),
                text: f.text.clone(),
                trusted: f.scope == Scope::User || trusted,
                truncated: false,
            })
            .collect();
        let (instructions, notes) = apply_budget(files, max_bytes);
        for n in notes {
            self.warn(Layer::Default, "instructions", n);
        }

        // ---- skills / commands / agents
        let skills = compile_skills(&src.skills, trusted);
        let mut commands = vec![];
        let mut agents = vec![];
        for f in &src.commands {
            if f.scope == Scope::Project && !trusted {
                self.warn(Layer::SharedProject, "commands", format!("workspace is not trusted: {} ignored", f.path));
                continue;
            }
            commands.push(compile_command(f));
        }
        for f in &src.agents {
            if f.scope == Scope::Project && !trusted {
                self.warn(Layer::SharedProject, "agents", format!("workspace is not trusted: {} ignored", f.path));
                continue;
            }
            agents.push(compile_agent(f));
        }
        // Project definitions override user ones of the same name.
        let commands = dedup_by_name(commands, |c| (c.name.clone(), c.scope));
        let agents = dedup_by_name(agents, |a| (a.name.clone(), a.scope));

        // Trusted instructions and the skill catalog go into the Static layer.
        for f in instructions.iter().filter(|f| f.trusted) {
            system.push(format!("Contents of {}:\n\n{}", f.path, f.text));
        }
        if !skills.is_empty() {
            let mut s = String::from("Available skills (load the SKILL.md file before using one):\n");
            for k in &skills {
                s.push_str(&format!("- {}: {} ({})\n", k.name, k.description, k.path));
            }
            system.push(s);
        }

        let kernel = KernelConfig {
            caps,
            fallbacks,
            system,
            tools: src.tools.clone(),
            rules,
            budgets,
            security,
            unattended,
            snapshots: snaps.into_values().collect(),
            compaction,
            encoder_version,
            read_only_mode: read_only,
            hooked,
        };
        let profile = Profile {
            kernel,
            hooks,
            auto_answer,
            mcp,
            instructions,
            skills,
            commands,
            agents,
            sandbox,
            tool_allowlist: None,
            warnings: self.warnings,
            explain: self.explain,
            hash: String::new(),
        };
        Ok(profile.rehash())
    }
}

fn dedup_by_name<T>(items: Vec<T>, key: impl Fn(&T) -> (String, Scope)) -> Vec<T> {
    let mut m: BTreeMap<String, (Scope, T)> = BTreeMap::new();
    for it in items {
        let (name, scope) = key(&it);
        match m.get(&name) {
            Some((s, _)) if *s > scope => {}
            _ => {
                m.insert(name, (scope, it));
            }
        }
    }
    m.into_values().map(|(_, t)| t).collect()
}

/// Name of the directory containing a file (`.../skills/<name>/SKILL.md`), or
/// the file stem.
fn stem(path: &str) -> String {
    let p = std::path::Path::new(path);
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn parent_name(path: &str) -> String {
    let p = std::path::Path::new(path);
    p.parent()
        .and_then(|d| d.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn compile_skills(files: &[SourceFile], trusted: bool) -> Vec<Skill> {
    let mut out: BTreeMap<String, Skill> = BTreeMap::new();
    for f in files {
        let (fm, body) = frontmatter::parse(&f.text);
        let name = fm.get("name").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| parent_name(&f.path));
        let description = fm
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string());
        // Project skills override user skills of the same name (files arrive user first).
        out.insert(
            name.clone(),
            Skill { name, description, path: f.path.clone(), trusted: f.scope == Scope::User || trusted },
        );
    }
    out.into_values().collect()
}

fn compile_command(f: &SourceFile) -> CommandDef {
    let (fm, body) = frontmatter::parse(&f.text);
    CommandDef {
        name: fm.get("name").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| stem(&f.path)),
        description: fm.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        template: body,
        path: f.path.clone(),
        scope: f.scope,
    }
}

fn compile_agent(f: &SourceFile) -> AgentDef {
    let (fm, body) = frontmatter::parse(&f.text);
    AgentDef {
        name: fm.get("name").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| stem(&f.path)),
        description: fm.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        tools: fm.get("tools").map(|v| v.as_list()),
        model: fm.get("model").and_then(|v| v.as_str()).map(String::from),
        prompt: body,
        path: f.path.clone(),
        scope: f.scope,
    }
}

/// Convenience used by tests / the CLI: which on_ask value is stricter.
pub fn stricter_on_ask(a: OnAsk, b: OnAsk) -> OnAsk {
    if on_ask_strictness(a) >= on_ask_strictness(b) {
        a
    } else {
        b
    }
}
