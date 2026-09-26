//! Tool specs, calls and results.

use crate::envelope::Trust;
use crate::ids::{BlobRef, CallId};
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
}

impl ToolResult {
    pub fn text(call_id: CallId, text: impl Into<String>, is_error: bool) -> Self {
        ToolResult {
            call_id,
            content: vec![ToolContent::Text { text: text.into() }],
            is_error,
            trust: Trust::Internal,
            observed: vec![],
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
