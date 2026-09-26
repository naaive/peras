//! The compiled, immutable [`Profile`].

use crate::instructions::InstructionFile;
use crate::settings::{AutoAnswer, HookExecutor};
use crate::sources::Scope;
use agent_proto::{HookPoint, KernelConfig, Layer, ToolSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDef {
    pub name: String,
    pub point: HookPoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub executor: HookExecutor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub layer: Layer,
}

/// Ring-5 auto-answer rule (evaluated by the runtime before asking a human;
/// never applies to invariant-level asks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoAnswerRule {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub answer: AutoAnswer,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub trusted: bool,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Path of the `SKILL.md` file (body loaded on demand).
    pub path: String,
    pub trusted: bool,
}

/// Slash command from `.agent/commands/<name>.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    /// Markdown template (frontmatter stripped); `$ARGUMENTS` is substituted.
    pub template: String,
    pub path: String,
    pub scope: Scope,
}

impl CommandDef {
    pub fn expand(&self, arguments: &str) -> String {
        self.template.replace("$ARGUMENTS", arguments)
    }
}

/// Sub-agent definition from `.agent/agents/<name>.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// Allowed tools; `None` = all of the parent's tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// System prompt of the child (the file body).
    pub prompt: String,
    pub path: String,
    pub scope: Scope,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPrefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<String>,
    pub require: bool,
}

/// A compile-time note: a setting that was ignored or adjusted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Warning {
    pub layer: Layer,
    pub key: String,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}: {}", self.layer, self.key, self.message)
    }
}

/// `agent config explain <key>`: final value and the layer it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explained {
    pub value: serde_json::Value,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub kernel: KernelConfig,
    pub hooks: Vec<HookDef>,
    pub auto_answer: Vec<AutoAnswerRule>,
    pub mcp: BTreeMap<String, McpServer>,
    /// Instruction files root -> cwd (after the byte budget).
    pub instructions: Vec<InstructionFile>,
    pub skills: Vec<Skill>,
    pub commands: Vec<CommandDef>,
    pub agents: Vec<AgentDef>,
    pub sandbox: SandboxPrefs,
    /// Tool-name allowlist (set for sub-agent profiles); applied by
    /// [`Profile::with_tools`] too, so narrowing survives late tool assembly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
    pub warnings: Vec<Warning>,
    pub explain: BTreeMap<String, Explained>,
    /// sha256 (hex) of the canonical serialization of everything above.
    pub hash: String,
}

impl Profile {
    /// Where the final value of `key` (e.g. `model.id`, `budgets.max_tokens`,
    /// `security.egress_allow`, `unattended.on_ask`) came from.
    pub fn explain(&self, key: &str) -> Option<&Explained> {
        self.explain.get(key)
    }

    /// Deterministic hash of the profile content (the `hash` field excluded).
    pub fn compute_hash(&self) -> String {
        let mut c = self.clone();
        c.hash = String::new();
        let bytes = serde_json::to_vec(&c).expect("profile serializes");
        hex::encode(Sha256::digest(&bytes))
    }

    pub(crate) fn rehash(mut self) -> Self {
        self.hash = self.compute_hash();
        self
    }

    /// Install the tool list (known only once the SDK has assembled tools).
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.kernel.tools = tools;
        self.apply_allowlist();
        self.rehash()
    }

    fn apply_allowlist(&mut self) {
        if let Some(allowed) = &self.tool_allowlist {
            self.kernel.tools.retain(|t| allowed.iter().any(|a| a == &t.name));
        }
    }

    pub fn command(&self, name: &str) -> Option<&CommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }

    pub fn agent(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Child profile for a sub-agent. It can only be narrower than `self`:
    /// tools are the intersection of the parent's and the definition's list,
    /// everything else (rules, budgets, security, unattended mode, hooks) is
    /// inherited. The definition's body is appended to the system prompt and
    /// its `model` (if any) replaces the model id.
    pub fn child(&self, agent: &str) -> Option<Profile> {
        let def = self.agent(agent)?.clone();
        let mut p = self.clone();
        if let Some(allowed) = &def.tools {
            p.tool_allowlist = Some(match &self.tool_allowlist {
                Some(parent) => allowed.iter().filter(|a| parent.contains(a)).cloned().collect(),
                None => allowed.clone(),
            });
        }
        p.apply_allowlist();
        if let Some(m) = &def.model {
            p.kernel.caps.model = agent_proto::ModelId::new(m.clone());
        }
        if !def.prompt.trim().is_empty() {
            p.kernel.system.push(def.prompt.clone());
        }
        // Children do not inherit the sub-agent catalog (no recursive spawning
        // unless the embedding code adds it back).
        p.agents.clear();
        p.kernel.tools.retain(|t| !t.subagent);
        Some(p.rehash())
    }
}
