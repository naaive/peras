//! `agent-code`: a Claude Code style coding agent built on the framework.
//!
//! What it adds on top of a discovered [`Agent`]:
//!
//! - a coding system prompt with the environment (working directory, git
//!   status, platform, date);
//! - coding tools besides the built-ins: `edit` with `replace_all`,
//!   `multi_edit`, `ls`, `todo_write`, `exit_plan_mode`;
//! - permission modes (`default`, `acceptEdits`, `plan`,
//!   `bypassPermissions`) switchable during a session, and session grants
//!   ("don't ask again for `cargo test` commands");
//! - built-in sub-agents (`explore`, `plan`, `general-purpose`) and prompt
//!   commands (`/init`, `/review`, `/security-review`, `/commit`), which
//!   files of the same name in `.agent/agents` / `.agent/commands` replace;
//! - workspace trust as a first-run decision, a conversation index for
//!   `--continue` / `--resume`;
//! - frontends: an interactive terminal REPL ([`repl`]) and a headless print
//!   mode ([`print`]).
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! let coding = agent_code::Options::new(".").build();
//! let text = coding.agent.run("Explain the build").await?;
//! # Ok(()) }
//! ```

pub mod builtin;
pub mod history;
pub mod mode;
pub mod print;
pub mod prompt;
pub mod render;
pub mod repl;
pub mod tools;
pub mod trust;

use agent::prelude::*;
use agent::runtime::ModelPort;
use mode::{Grant, ModeRule, PermissionMode, Permissions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tools::{ExitPlanMode, TodoStore, TodoWrite};

pub use mode::PermissionMode as Mode;

/// How to assemble the coding agent.
#[derive(Clone)]
pub struct Options {
    /// Workspace (the project directory).
    pub dir: PathBuf,
    /// Journal database; default `<dir>/.agent/runs.db`.
    pub db: Option<PathBuf>,
    /// Model id (`[model] id`); default from the configuration.
    pub model: Option<String>,
    /// Model port instead of the configured one (tests, other vendors).
    pub port: Option<Arc<dyn ModelPort>>,
    pub mode: PermissionMode,
    /// `edit`, `bash(git diff:*)`: never asked for (policy-level questions).
    pub allowed_tools: Vec<String>,
    /// Tools (or `bash(prefix:*)` commands) denied by policy.
    pub disallowed_tools: Vec<String>,
    /// Appended to the system prompt.
    pub append_system_prompt: Option<String>,
    /// Extra settings file (command-line layer).
    pub settings: Option<PathBuf>,
    /// The workspace is trusted (its instruction files, commands, hooks and
    /// MCP servers are active; its files are not untrusted content).
    pub trusted: bool,
    /// Unattended: what to do with questions nobody can answer.
    pub unattended: Option<OnAsk>,
    /// Watch the configuration files and apply changes between turns.
    pub hot_reload: bool,
    /// The user's home (user configuration layer); default `$HOME`.
    pub home: Option<Option<PathBuf>>,
}

impl Options {
    pub fn new(dir: impl Into<PathBuf>) -> Options {
        Options {
            dir: dir.into(),
            db: None,
            model: None,
            port: None,
            mode: PermissionMode::Default,
            allowed_tools: vec![],
            disallowed_tools: vec![],
            append_system_prompt: None,
            settings: None,
            trusted: false,
            unattended: None,
            hot_reload: false,
            home: None,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.db.clone().unwrap_or_else(|| self.dir.join(".agent").join("runs.db"))
    }

    /// The command-line settings layer for these options.
    pub fn settings_toml(&self) -> String {
        let mut t = toml::Table::new();
        if let Some(m) = &self.model {
            let mut model = toml::Table::new();
            model.insert("id".into(), m.clone().into());
            t.insert("model".into(), model.into());
        }
        if self.trusted {
            let mut sec = toml::Table::new();
            sec.insert("workspace_trusted".into(), true.into());
            t.insert("security".into(), sec.into());
        }
        if let Some(s) = &self.append_system_prompt {
            t.insert("system".into(), toml::Value::Array(vec![s.clone().into()]));
        }
        let mut perms = vec![];
        for spec in &self.disallowed_tools {
            let mut p = toml::Table::new();
            p.insert("name".into(), format!("cli:disallowed:{spec}").into());
            match parse_tool_spec(spec) {
                ToolSpecArg::Tool(name) => {
                    p.insert("tool".into(), name.into());
                }
                ToolSpecArg::Command(prefix) => {
                    p.insert("tool".into(), "bash".into());
                    p.insert("resource".into(), format!("cmd:{prefix}*").into());
                }
            }
            p.insert("action".into(), "deny".into());
            perms.push(toml::Value::Table(p));
        }
        if !perms.is_empty() {
            t.insert("permissions".into(), toml::Value::Array(perms));
        }
        t.to_string()
    }

    /// Assemble the agent. Never fails: configuration problems surface on
    /// the first run (or [`Agent::check`]).
    pub fn build(&self) -> Coding {
        let permissions = Permissions::new(self.mode, &self.dir);
        for spec in &self.allowed_tools {
            permissions.grant(match parse_tool_spec(spec) {
                ToolSpecArg::Tool(name) => Grant::Tool(name),
                ToolSpecArg::Command(prefix) => Grant::CommandPrefix(prefix),
            });
        }
        let todos = TodoStore::default();
        let db = self.db_path();
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut system = prompt::SYSTEM.to_string();
        system.push_str("\n\n");
        system.push_str(&prompt::environment(&self.dir));
        let gate_perms = permissions.clone();
        let mut agent = Agent::discover(&self.dir)
            .journal(Sqlite(db.clone()))
            .system_prompt(system)
            .settings(self.settings_toml())
            .sources(builtin::add_to)
            .extra_tools(tools_for(&permissions, &todos))
            .gate(move |p| gate_perms.gate(p))
            .auto_answer(ModeRule(permissions.clone()));
        if let Some(home) = &self.home {
            agent = agent.home(home.clone());
        }
        if let Some(port) = &self.port {
            agent = agent.model_port(port.clone());
        }
        if let Some(path) = &self.settings {
            agent = agent.policy(path);
        }
        if let Some(on_ask) = self.unattended {
            agent = agent.unattended(on_ask);
        }
        if self.hot_reload {
            agent = agent.hot_reload();
        }
        Coding { agent, permissions, todos, dir: self.dir.clone(), db, custom_model: self.port.is_some() }
    }
}

/// The coding tools added to the built-ins.
fn tools_for(permissions: &Permissions, todos: &TodoStore) -> Vec<Arc<dyn agent::runtime::Tool>> {
    vec![
        Arc::new(tools::edit),
        Arc::new(tools::multi_edit),
        Arc::new(tools::ls),
        Arc::new(TodoWrite(todos.clone())),
        Arc::new(ExitPlanMode(permissions.clone())),
    ]
}

enum ToolSpecArg {
    Tool(String),
    Command(String),
}

/// `edit` → a tool; `bash(git diff:*)` / `bash(npm test)` → a command prefix.
fn parse_tool_spec(spec: &str) -> ToolSpecArg {
    let s = spec.trim();
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower.starts_with("bash(").then(|| &s[5..]).and_then(|r| r.strip_suffix(')')) {
        let prefix = inner.trim_end_matches('*').trim_end_matches(':').trim();
        return ToolSpecArg::Command(prefix.to_string());
    }
    ToolSpecArg::Tool(s.to_string())
}

/// The assembled coding agent and the state its frontends share with it.
#[derive(Clone)]
pub struct Coding {
    pub agent: Agent,
    pub permissions: Permissions,
    pub todos: TodoStore,
    pub dir: PathBuf,
    pub db: PathBuf,
    /// Driven by a model port given in [`Options::port`].
    pub custom_model: bool,
}

impl Coding {
    /// The model id sessions start with.
    pub async fn model_name(&self) -> Result<String, agent::Error> {
        let id = self.agent.profile().await?.kernel.caps.model.to_string();
        Ok(if !self.custom_model && (id.is_empty() || id == "scripted") {
            agent::adapters::model::anthropic::DEFAULT_MODEL.to_string()
        } else {
            id
        })
    }

    /// The conversation index of this workspace.
    pub fn history(&self) -> history::History {
        history::History::new(&data_dir(&self.dir))
    }
}

/// `<dir>/.agent`.
pub fn data_dir(dir: &Path) -> PathBuf {
    dir.join(".agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_layer_from_options() {
        let mut o = Options::new(".");
        o.model = Some("claude-opus-5-5".into());
        o.trusted = true;
        o.append_system_prompt = Some("Answer in French.".into());
        o.disallowed_tools = vec!["web_fetch".into(), "bash(git push:*)".into()];
        let t: toml::Table = o.settings_toml().parse().unwrap();
        assert_eq!(t["model"]["id"].as_str(), Some("claude-opus-5-5"));
        assert_eq!(t["security"]["workspace_trusted"].as_bool(), Some(true));
        let perms = t["permissions"].as_array().unwrap();
        assert_eq!(perms[0]["tool"].as_str(), Some("web_fetch"));
        assert_eq!(perms[1]["resource"].as_str(), Some("cmd:git push*"));
        assert!(agent::profile::Settings::default() == agent::profile::Settings::default());
        let parsed: agent::profile::Settings = toml::from_str(&o.settings_toml()).expect("valid settings");
        assert_eq!(parsed.permissions.len(), 2);
    }

    #[test]
    fn tool_specs() {
        assert!(matches!(parse_tool_spec("bash(git diff:*)"), ToolSpecArg::Command(p) if p == "git diff"));
        assert!(matches!(parse_tool_spec("Bash(npm test)"), ToolSpecArg::Command(p) if p == "npm test"));
        assert!(matches!(parse_tool_spec("edit"), ToolSpecArg::Tool(t) if t == "edit"));
    }
}
