//! The TOML settings schema (one file per layer).
//!
//! Every field is optional: a layer only states what it wants to change. Unknown
//! keys are errors (typos in security settings must not be silently ignored).
//!
//! ```toml
//! # managed.toml only: fields no other layer may override
//! locked = ["security.egress_allow", "model.id"]
//!
//! system = ["Always answer in English."]      # system prompt additions
//!
//! [model]
//! id = "claude-sonnet-5"
//! fallbacks = ["claude-haiku-5"]
//! window = 200000
//! max_output = 8192
//!
//! [[permissions]]
//! name = "no-secrets"
//! resource = "fs:///**/.env*"
//! tool = "*"
//! mode = "read"            # read | write (omit = both)
//! action = "deny"          # allow | ask | deny
//!
//! [budgets]
//! max_tokens = 1000000
//! max_calls_per_turn = 100
//!
//! [security]
//! workspace_trusted = true         # sensitive
//! egress_allow = ["net:crates.io:443"]   # sensitive
//! trusted_sources = ["net:docs.internal:443"]  # sensitive
//! private = ["fs:///**/secrets/**"]  # appended to defaults
//! untrusted = ["fs:///**/vendor/**"]  # appended
//! persistence = ["fs:///**/.npmrc"]   # appended
//! disposable_env = false           # sensitive
//!
//! [unattended]
//! on_ask = "defer"                 # sensitive: allow | deny | defer
//!
//! [[auto_answer]]                  # sensitive (project layers: deny only)
//! rule = "tests"
//! resource = "cmd:cargo test*"
//! answer = "allow"
//!
//! [[hooks]]
//! point = "pre_tool"
//! matcher = "bash"
//! executor = { command = "./check.sh", args = ["--strict"] }   # or { http = ".." } / { mcp = "server/tool" }
//!
//! [mcp.github]
//! command = "github-mcp"
//! args = ["--stdio"]
//! env = { GITHUB_ORG = "acme" }
//! trusted = false
//!
//! [[snapshots]]
//! key = "git_status"
//! min_interval_ms = 5000
//!
//! [compaction]
//! pressure_ratio = 0.7
//!
//! [sandbox]
//! prefer = "bubblewrap"
//! require = true
//!
//! [plan]
//! read_only = true
//!
//! [instructions]
//! max_bytes = 65536
//! ```

use agent_proto::{AccessMode, HookPoint, OnAsk, PolicyAction, SnapshotRule};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Managed layer only: dotted keys (`model.id`, `security.egress_allow`,
    /// `permissions`, ...) that no other layer may set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSettings>,
    /// System prompt additions (accumulated across layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<PermissionSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<BudgetSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecuritySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattended: Option<UnattendedSettings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_answer: Vec<AutoAnswerSetting>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookSetting>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpSetting>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<SnapshotRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<InstructionSettings>,
}

impl Settings {
    /// Render as TOML (e.g. to pass CLI arguments as the `Cli` layer).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionSetting {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AccessMode>,
    pub action: PolicyAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turn_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls_per_turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repeat_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_continuations: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecuritySettings {
    /// Sensitive: project layers may only set `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_trusted: Option<bool>,
    /// Sensitive: project layers may only remove entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_allow: Option<Vec<String>>,
    /// Sensitive: project layers may only remove entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_sources: Option<Vec<String>>,
    /// Appended to the defaults (adding is always a tightening).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<Vec<String>>,
    /// Sensitive: project layers may only set `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposable_env: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnattendedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ask: Option<OnAsk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoAnswer {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoAnswerSetting {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub answer: AutoAnswer,
}

/// A hook executor: `{ command = "..", args = [..] }`, `{ http = ".." }` or
/// `{ mcp = "server/tool" }`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookExecutor {
    Command(CommandExecutor),
    Http(HttpExecutor),
    Mcp(McpExecutor),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandExecutor {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpExecutor {
    pub http: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpExecutor {
    pub mcp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSetting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub point: HookPoint,
    /// Tool-name glob (PreTool / PostTool / Permission); `None` = everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub executor: HookExecutor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSetting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Remote server URL (alternative to `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Sensitive: results from trusted servers do not taint. Ignored in project layers.
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reserve: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSettings {
    /// Preferred implementation (`bubblewrap`, `landlock`, `seatbelt`, `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<String>,
    /// Refuse to run commands when no sandbox is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSettings {
    /// Byte budget for instruction files (default 64 KiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}
