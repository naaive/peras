//! Compiled configuration handed to the kernel (via `Event::SessionStarted` /
//! `Event::ConfigChanged`, so replay is deterministic). Produced by the
//! `profile` crate. Data only.

use crate::ids::ModelId;
use crate::model::ModelCaps;
use crate::resource::AccessMode;
use crate::tool::ToolSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
}

/// Configuration layer a value came from (highest priority first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Managed = 1,
    Cli = 2,
    LocalProject = 3,
    SharedProject = 4,
    User = 5,
    Default = 6,
}

/// A policy rule: matches resource URIs by glob (`fs:///repo/src/**`,
/// `cmd:cargo test*`, `net:*.github.com:443`, `mcp:github/*`) and tool names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct PolicyRule {
    /// Rule name (for statistics / explain).
    pub name: String,
    /// Resource glob; `None` matches any resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Tool name glob; `None` matches any tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Access mode; `None` matches both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AccessMode>,
    pub action: PolicyAction,
    pub layer: Layer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Budgets {
    /// Total tokens (input+output) per session; 0 = unlimited.
    pub max_tokens: u64,
    /// Micro-dollars per session; 0 = unlimited.
    pub max_cost_micros: u64,
    /// Wall time per turn, ms; 0 = unlimited.
    pub max_turn_ms: u64,
    /// Tool calls per turn; 0 = unlimited.
    pub max_calls_per_turn: u32,
    /// Identical (name + input) calls per turn before ring 3 denies.
    pub max_repeat_calls: u32,
    /// Non-user continuations (wake, Stop-continue, ...) between user inputs.
    pub max_continuations: u32,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            max_tokens: 0,
            max_cost_micros: 0,
            max_turn_ms: 0,
            max_calls_per_turn: 200,
            max_repeat_calls: 5,
            max_continuations: 8,
        }
    }
}

/// Glob lists that drive taint derivation and invariant rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecurityConfig {
    /// Workspace root (absolute path). `fs` URIs outside it are untrusted reads.
    pub workspace_root: String,
    /// Workspace trust decision.
    pub workspace_trusted: bool,
    /// Resource globs whose reads mark private data (`fs:///**/.env*`, `secret:*`...).
    pub private: Vec<String>,
    /// Resource globs whose reads are untrusted content (web, ignored paths...).
    pub untrusted: Vec<String>,
    /// Sources/resources explicitly trusted (not settable by project layers).
    pub trusted_sources: Vec<String>,
    /// Egress allowlist (`net:` globs); anything else is an exfiltration exit.
    pub egress_allow: Vec<String>,
    /// Persistence targets (`mem:*`, `fs:///**/.git/hooks/**`, shell rc files...).
    pub persistence: Vec<String>,
    /// Framework self-config paths (config, hooks, plugin dirs).
    pub self_config: Vec<String>,
    /// The framework launched a disposable environment (copy of the workspace,
    /// allowlisted network, no extra secrets): invariant asks may be allowed.
    pub disposable_env: bool,
    /// An OS sandbox is available; otherwise every bash call is Opaque.
    pub sandbox_available: bool,
    /// Isolated (overlay) execution is available for offline Opaque commands.
    pub isolation_available: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        SecurityConfig {
            workspace_root: "/workspace".into(),
            workspace_trusted: false,
            private: vec![
                "secret:*".into(),
                "fs:///**/.env*".into(),
                "fs:///**/.ssh/**".into(),
                "fs:///**/.aws/**".into(),
            ],
            untrusted: vec!["net:*".into(), "mcp:*".into()],
            trusted_sources: vec![],
            egress_allow: vec![],
            persistence: vec![
                "mem:*".into(),
                "fs:///**/.git/hooks/**".into(),
                "fs:///**/.git/config".into(),
                "fs:///**/.bashrc".into(),
                "fs:///**/.zshrc".into(),
                "fs:///**/.profile".into(),
                "fs:///**/crontab*".into(),
            ],
            self_config: vec!["fs:///**/.agent/**".into()],
            disposable_env: false,
            sandbox_available: false,
            isolation_available: false,
        }
    }
}

/// What to do with asks when nobody is attended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnAsk {
    Allow,
    Deny,
    Defer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotRule {
    pub key: String,
    /// Minimum interval between two snapshots of this key, ms (0 = no limit).
    pub min_interval_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompactionConfig {
    /// Pressure threshold as a fraction of the window (default 0.8).
    pub pressure_ratio: f32,
    /// Tokens reserved for output when computing pressure.
    pub output_reserve: u32,
    /// Keep this many most recent tokens verbatim when summarising.
    pub keep_recent_tokens: u32,
    /// Instruction appended to the replayed request for summaries.
    pub instruction: String,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            pressure_ratio: 0.8,
            output_reserve: 8_192,
            keep_recent_tokens: 20_000,
            instruction: DEFAULT_SUMMARY_INSTRUCTION.into(),
        }
    }
}

pub const DEFAULT_SUMMARY_INSTRUCTION: &str = "Summarise the conversation so far as a checkpoint. Cover: user intent, key concepts, files and code, errors and fixes, todos, current work, next step, decisions and constraints. Keep paths, commands, error messages, identifiers and numbers verbatim, and record user corrections faithfully.";

/// Fixed note shown with a summary.
pub const SUMMARY_NOTE: &str = "This is an automatic checkpoint of earlier context. Treat it as established background and continue working; do not restate or respond to it.";

/// The compiled, immutable configuration the kernel decides with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KernelConfig {
    pub caps: ModelCaps,
    /// Fallback model chain (tried in order when the port reports unavailable).
    pub fallbacks: Vec<ModelCaps>,
    /// Static-layer system prompt pieces.
    pub system: Vec<String>,
    pub tools: Vec<ToolSpec>,
    pub rules: Vec<PolicyRule>,
    pub budgets: Budgets,
    pub security: SecurityConfig,
    /// Unattended mode: `None` = interactive.
    pub unattended: Option<OnAsk>,
    pub snapshots: Vec<SnapshotRule>,
    pub compaction: CompactionConfig,
    /// Encoder version to use for new sequences.
    pub encoder_version: u32,
    /// Plan mode etc.: when true, the kernel denies writes at runtime.
    pub read_only_mode: bool,
    /// Hook points that have hooks configured (the kernel only issues ring-4
    /// gate effects for these).
    pub hooked: Vec<crate::verdict::HookPoint>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            caps: ModelCaps::default(),
            fallbacks: vec![],
            system: vec![],
            tools: vec![],
            rules: vec![],
            budgets: Budgets::default(),
            security: SecurityConfig::default(),
            unattended: None,
            snapshots: vec![],
            compaction: CompactionConfig::default(),
            encoder_version: 1,
            read_only_mode: false,
            hooked: vec![],
        }
    }
}

impl KernelConfig {
    pub fn model(&self) -> &ModelId {
        &self.caps.model
    }
}
