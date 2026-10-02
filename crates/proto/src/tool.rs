//! Tool specs, calls and results.

use crate::envelope::Trust;
use crate::effect::TurnOutcome;
use crate::ids::{BlobRef, CallId, SessionId};
use crate::resource::{Access, EffectClass};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What the model sees of a tool (goes into the Static layer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input object.
    pub input_schema: serde_json::Value,
    /// Default class when the call cannot be analysed further.
    pub class: EffectClass,
    /// Sub-agent tools are marked so the kernel can apply narrowing rules.
    #[serde(default)]
    pub subagent: bool,
}

/// A tool call proposed by the model, enriched by the runtime with the tool's
/// access declaration and side-effect class before it reaches the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub input: serde_json::Value,
    /// Declared accesses (from the parameter types or `Tool::access`).
    #[serde(default)]
    pub access: Vec<Access>,
    pub class: EffectClass,
    /// The call runs isolated (on a copy of the workspace, offline): its
    /// changes are staged and only merged once their change list is approved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub isolated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text { text: String },
    /// Large output spilled to a blob; the model sees `preview` and the reference.
    Blob { blob: BlobRef, preview: String },
    Image { blob: BlobRef },
    Json { value: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub call_id: CallId,
    pub content: Vec<ToolContent>,
    /// Tool failures (and denials) are results handed back to the model.
    pub is_error: bool,
    /// Trust of the content (e.g. untrusted for web fetches / MCP results).
    pub trust: Trust,
    /// Content hashes observed by reads (used for stale-write detection).
    #[serde(default)]
    pub observed: Vec<Access>,
    /// Workspace-relative paths an isolated call changed: staged, not yet
    /// in the workspace. The kernel reviews them before the result is written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged: Vec<String>,
    /// Sub-agent calls: the child's outcome and what it consumed (charged to
    /// the calling session's budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<Box<SubagentReport>>,
    /// Instruction files found along the directories this call accessed
    /// (attached by the runtime). The kernel journals new or changed ones as
    /// pending instructions and strips them from the recorded result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<FoundInstructions>,
}

/// What a sub-agent call reports back to its parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentReport {
    pub child: SessionId,
    pub outcome: TurnOutcome,
    /// Tokens (input + output + cache) the child consumed.
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub cost_micros: u64,
}

/// An instruction file (`AGENTS.md` and the like) in a subdirectory a tool
/// accessed: injected into the context at the next step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundInstructions {
    /// Absolute path of the file.
    pub path: String,
    pub text: String,
}

impl ToolResult {
    pub fn text(call_id: CallId, text: impl Into<String>, is_error: bool) -> Self {
        ToolResult {
            call_id,
            content: vec![ToolContent::Text { text: text.into() }],
            is_error,
            trust: Trust::Internal,
            observed: vec![],
            staged: vec![],
            subagent: None,
            instructions: vec![],
        }
    }
    /// The result synthesised for a denied call: the reason is returned to the model.
    pub fn denied(call_id: CallId, reason: &str) -> Self {
        Self::text(call_id, format!("Denied: {reason}"), true)
    }
    /// The result synthesised for calls whose result is missing after a hard interrupt.
    pub fn cancelled(call_id: CallId) -> Self {
        Self::text(call_id, "Cancelled before execution.", true)
    }
}
