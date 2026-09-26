//! Model-facing data: capabilities, replies, usage, errors, request-sequence heads.

use crate::ids::ModelId;
use crate::render::RenderProfile;
use crate::tool::{ToolCall, ToolSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a model port can do. The kernel only queries capabilities and never
/// branches on vendor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCaps {
    pub model: ModelId,
    pub parallel_tools: bool,
    pub thinking: bool,
    pub images: bool,
    pub structured_output: bool,
    /// Context window in tokens.
    pub window: u32,
    /// Max output tokens per request.
    pub max_output: u32,
    /// Max number of cache breakpoints the encoder may place.
    pub cache_breakpoints: u32,
    /// System prompt / tools can be updated mid-sequence without a new sequence.
    pub mid_sequence_updates: bool,
    pub render: RenderProfile,
    /// Bytes per token used by the estimator.
    pub bytes_per_token: u32,
}

impl Default for ModelCaps {
    fn default() -> Self {
        ModelCaps {
            model: ModelId::new("scripted"),
            parallel_tools: true,
            thinking: false,
            images: false,
            structured_output: false,
            window: 200_000,
            max_output: 8_192,
            cache_breakpoints: 4,
            mid_sequence_updates: false,
            render: RenderProfile::default(),
            bytes_per_token: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// Cost in micro-dollars, if the meter knows prices.
    #[serde(default)]
    pub cost_micros: u64,
}

impl Usage {
    pub fn total_context(&self) -> u32 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    /// Partially shown reply cut by a hard interrupt.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse(ToolCall),
    /// Vendor-private content, stored and sent back verbatim; dropped when the
    /// model changes (with the new sequence).
    Opaque {
        vendor: String,
        data: serde_json::Value,
    },
}

/// A complete model reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub stop: StopReason,
    pub usage: Usage,
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse(c) => Some(c),
            _ => None,
        })
    }
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Unified model errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelError {
    /// Request exceeded the context window: handed to pressure relief.
    #[error("context overflow")]
    Overflow,
    #[error("rate limited")]
    RateLimited { retry_after_ms: Option<u64> },
    #[error("overloaded")]
    Overloaded,
    /// The model is unavailable: the kernel may switch to a fallback.
    #[error("model unavailable: {message}")]
    Unavailable { message: String },
    #[error("authentication failed")]
    Auth,
    #[error("invalid request: {message}")]
    Invalid { message: String },
    #[error("network error: {message}")]
    Network { message: String },
}

impl ModelError {
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ModelError::RateLimited { .. } | ModelError::Overloaded | ModelError::Network { .. }
        )
    }
}

/// Fixed head of a request sequence. Written to the journal when a sequence
/// opens; changing any of it means a new sequence (cache invalidation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SeqHead {
    /// Sequence number within the session (0, 1, 2...).
    pub seq_no: u32,
    pub model: ModelId,
    /// System prompt pieces (Static layer), in order.
    pub system: Vec<String>,
    /// Tool definitions (Static layer).
    pub tools: Vec<ToolSpec>,
    pub render: RenderProfile,
    /// Encoder version; frozen after release.
    pub encoder_version: u32,
}
