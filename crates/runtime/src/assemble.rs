//! Stream assembly: model [`Delta`]s -> [`AssistantMessage`], with early
//! enrichment of completed tool-use blocks.

use crate::ports::Delta;
use crate::registry::ToolRegistry;
use agent_proto::*;

#[derive(Debug, Clone)]
enum Part {
    Done(ContentBlock),
    /// A tool-use block still receiving input JSON.
    Tool { id: CallId, name: String, buf: String },
}

/// What pushing one delta produced.
#[derive(Debug, Default)]
pub struct Pushed {
    /// A tool-use block completed (already enriched).
    pub call: Option<ToolCall>,
    /// Pulse to publish, if any.
    pub pulse_text: Option<String>,
    pub pulse_thinking: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Assembler {
    parts: Vec<Part>,
    usage: Usage,
    stop: Option<StopReason>,
}

fn merge_usage(into: &mut Usage, u: Usage) {
    if u.input_tokens != 0 {
        into.input_tokens = u.input_tokens;
    }
    if u.output_tokens != 0 {
        into.output_tokens = u.output_tokens;
    }
    if u.cache_read_tokens != 0 {
        into.cache_read_tokens = u.cache_read_tokens;
    }
    if u.cache_write_tokens != 0 {
        into.cache_write_tokens = u.cache_write_tokens;
    }
    if u.cost_micros != 0 {
        into.cost_micros = u.cost_micros;
    }
}

/// Parse accumulated tool input JSON. Empty -> `{}`; invalid JSON is passed
/// through as a string (the tool rejects it as invalid input).
pub fn parse_tool_input(buf: &str) -> serde_json::Value {
    if buf.trim().is_empty() {
        return serde_json::Value::Object(Default::default());
    }
    serde_json::from_str(buf).unwrap_or_else(|_| serde_json::Value::String(buf.to_string()))
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    fn close_tool(&mut self, tools: &ToolRegistry) -> Option<ToolCall> {
        let open = self.parts.iter_mut().rev().find(|p| matches!(p, Part::Tool { .. }))?;
        let Part::Tool { id, name, buf } = open else { return None };
        let call = tools.enrich(id.clone(), name, parse_tool_input(buf));
        *open = Part::Done(ContentBlock::ToolUse(call.clone()));
        Some(call)
    }

    pub fn push(&mut self, d: Delta, tools: &ToolRegistry) -> Pushed {
        let mut out = Pushed::default();
        match d {
            Delta::Text(t) => {
                match self.parts.last_mut() {
                    Some(Part::Done(ContentBlock::Text { text })) => text.push_str(&t),
                    _ => self.parts.push(Part::Done(ContentBlock::Text { text: t.clone() })),
                }
                out.pulse_text = Some(t);
            }
            Delta::Thinking(t) => {
                match self.parts.last_mut() {
                    Some(Part::Done(ContentBlock::Thinking { text, signature: None })) => text.push_str(&t),
                    _ => self.parts.push(Part::Done(ContentBlock::Thinking { text: t.clone(), signature: None })),
                }
                out.pulse_thinking = Some(t);
            }
            Delta::ThinkingSignature(sig) => {
                // Attach to the most recent thinking block.
                let found = self.parts.iter_mut().rev().find_map(|p| match p {
                    Part::Done(ContentBlock::Thinking { signature, .. }) => Some(signature),
                    _ => None,
                });
                match found {
                    Some(signature) => signature.get_or_insert_with(String::new).push_str(&sig),
                    None => self.parts.push(Part::Done(ContentBlock::Thinking {
                        text: String::new(),
                        signature: Some(sig),
                    })),
                }
            }
            Delta::ToolUseStart { id, name } => {
                // An unterminated previous block is implicitly complete.
                out.call = self.close_tool(tools);
                self.parts.push(Part::Tool { id, name, buf: String::new() });
            }
            Delta::ToolUseInput(s) => {
                if let Some(Part::Tool { buf, .. }) =
                    self.parts.iter_mut().rev().find(|p| matches!(p, Part::Tool { .. }))
                {
                    buf.push_str(&s);
                } else {
                    tracing::warn!("tool input delta without an open tool-use block");
                }
            }
            Delta::ToolUseEnd => out.call = self.close_tool(tools),
            Delta::Opaque { vendor, data } => {
                out.call = self.close_tool(tools);
                self.parts.push(Part::Done(ContentBlock::Opaque { vendor, data }));
            }
            Delta::Usage(u) => merge_usage(&mut self.usage, u),
            Delta::Stop(r) => self.stop = Some(r),
        }
        out
    }

    fn blocks(&self) -> Vec<ContentBlock> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Done(b) => Some(b.clone()),
                Part::Tool { .. } => None,
            })
            .collect()
    }

    fn has_tool_use(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, Part::Done(ContentBlock::ToolUse(_))))
    }

    /// The message shown so far, cut by a hard interrupt: incomplete tool-use
    /// blocks are dropped, stop = `Interrupted`.
    pub fn partial(&self) -> AssistantMessage {
        AssistantMessage { content: self.blocks(), stop: StopReason::Interrupted, usage: self.usage }
    }

    /// The final message at end of stream. An unterminated tool-use block at
    /// the end is dropped (incomplete).
    pub fn finish(&self) -> AssistantMessage {
        if self.parts.iter().any(|p| matches!(p, Part::Tool { .. })) {
            tracing::warn!("stream ended inside a tool-use block; dropping it");
        }
        let stop = self.stop.unwrap_or(if self.has_tool_use() { StopReason::ToolUse } else { StopReason::EndTurn });
        AssistantMessage { content: self.blocks(), stop, usage: self.usage }
    }
}
