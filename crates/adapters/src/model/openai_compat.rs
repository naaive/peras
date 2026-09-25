//! Minimal OpenAI-compatible chat-completions port (streaming, tool calls).
//! Demonstrates vendor neutrality: it maps to the same semantic deltas as the
//! Anthropic adapter.

use super::anthropic::{data_frame, reminder};
use super::sse::SseEvent;
use super::{delta_stream, is_overflow_message, map_status, retry_after_ms, StreamMapper};
use agent_proto::*;
use agent_runtime::{Delta, Encoder, ModelPort, Request};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Map, Value};

pub const VENDOR: &str = "openai";

/// OpenAI-compatible chat completions (`POST {base_url}/chat/completions`).
pub struct OpenAiCompat {
    model: String,
    api_key: Option<String>,
    base_url: String,
    caps: ModelCaps,
    encoder: OpenAiEncoderV1,
    http: reqwest::Client,
}

impl OpenAiCompat {
    /// `base_url` includes the version prefix, e.g. `https://api.openai.com/v1`.
    /// The key defaults to `OPENAI_API_KEY` (read at send time); an empty key
    /// sends no `Authorization` header (local servers).
    pub fn new(model: impl Into<String>, base_url: impl Into<String>) -> Self {
        let model = model.into();
        OpenAiCompat {
            caps: ModelCaps {
                model: ModelId::new(model.clone()),
                parallel_tools: true,
                thinking: false,
                images: false,
                structured_output: false,
                window: 128_000,
                max_output: 16_384,
                cache_breakpoints: 0,
                mid_sequence_updates: false,
                render: RenderProfile { name: "openai".into(), mid_sequence_system: false, ..RenderProfile::default() },
                bytes_per_token: 4,
            },
            model,
            api_key: None,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            encoder: OpenAiEncoderV1,
            http: reqwest::Client::new(),
        }
    }
    pub fn api_key(mut self, k: impl Into<String>) -> Self {
        self.api_key = Some(k.into());
        self
    }
    pub fn http_client(mut self, c: reqwest::Client) -> Self {
        self.http = c;
        self
    }
    pub fn caps_mut(&mut self) -> &mut ModelCaps {
        &mut self.caps
    }

    pub fn wire_body(&self, req: &Request) -> Value {
        let mut m = Map::new();
        m.insert("model".into(), json!(self.model));
        m.insert("max_tokens".into(), json!(req.max_tokens));
        m.insert("stream".into(), json!(true));
        m.insert("stream_options".into(), json!({"include_usage": true}));
        if let Value::Object(b) = &req.body {
            for (k, v) in b {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    async fn send(&self, req: Request) -> Result<reqwest::Response, ModelError> {
        let key = self.api_key.clone().or_else(|| std::env::var("OPENAI_API_KEY").ok()).unwrap_or_default();
        let mut rb = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        if !key.is_empty() {
            rb = rb.bearer_auth(key);
        }
        let resp = rb
            .body(serde_json::to_vec(&self.wire_body(&req)).expect("json"))
            .send()
            .await
            .map_err(|e| ModelError::Network { message: e.to_string() })?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let ra = retry_after_ms(resp.headers());
        let text = resp.text().await.unwrap_or_default();
        Err(map_api_error(status, ra, &text))
    }
}

impl ModelPort for OpenAiCompat {
    fn caps(&self) -> &ModelCaps {
        &self.caps
    }
    fn encoder(&self) -> &dyn Encoder {
        &self.encoder
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        futures::stream::once(self.send(req))
            .flat_map(|r| match r {
                Ok(resp) => delta_stream(resp, OpenAiStreamMapper::default()),
                Err(e) => futures::stream::iter([Err(e)]).boxed(),
            })
            .boxed()
    }
}

pub fn map_api_error(status: u16, retry_after: Option<u64>, body: &str) -> ModelError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = v.pointer("/error/message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| body.to_string());
    let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
    if code == "context_length_exceeded" || (status == 400 && is_overflow_message(&msg)) {
        return ModelError::Overflow;
    }
    if code == "model_not_found" {
        return ModelError::Unavailable { message: msg };
    }
    map_status(status, retry_after, msg)
}

fn map_finish(s: &str) -> StopReason {
    match s {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

/// OpenAI chat-completion chunks -> semantic deltas.
#[derive(Debug, Default)]
pub struct OpenAiStreamMapper {
    open_tool: Option<u64>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    finished: bool,
}

impl OpenAiStreamMapper {
    fn close_tool(&mut self, out: &mut Vec<Result<Delta, ModelError>>) {
        if self.open_tool.take().is_some() {
            out.push(Ok(Delta::ToolUseEnd));
        }
    }
    fn terminal(&mut self) -> Vec<Result<Delta, ModelError>> {
        let mut out = vec![];
        self.close_tool(&mut out);
        self.finished = true;
        out.push(Ok(Delta::Usage(self.usage.unwrap_or_default())));
        out.push(Ok(Delta::Stop(self.stop.unwrap_or(StopReason::EndTurn))));
        out
    }
}

impl StreamMapper for OpenAiStreamMapper {
    fn on_event(&mut self, ev: SseEvent) -> Vec<Result<Delta, ModelError>> {
        if self.finished {
            return vec![];
        }
        if ev.data.trim() == "[DONE]" {
            return self.terminal();
        }
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(e) => return vec![Err(ModelError::Network { message: format!("bad sse json: {e}") })],
        };
        let mut out = vec![];
        if let Some(err) = v.get("error") {
            self.finished = true;
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            out.push(Err(if is_overflow_message(&msg) { ModelError::Overflow } else { ModelError::Network { message: msg } }));
            return out;
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let g = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0) as u32;
            let cached = g("/prompt_tokens_details/cached_tokens");
            self.usage = Some(Usage {
                input_tokens: g("/prompt_tokens").saturating_sub(cached),
                output_tokens: g("/completion_tokens"),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cost_micros: 0,
            });
        }
        let Some(choice) = v.pointer("/choices/0") else { return out };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            if !t.is_empty() {
                self.close_tool(&mut out);
                out.push(Ok(Delta::Text(t.to_string())));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let index = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                if self.open_tool != Some(index) {
                    self.close_tool(&mut out);
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string();
                    out.push(Ok(Delta::ToolUseStart { id: CallId::new(id), name }));
                    self.open_tool = Some(index);
                }
                if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                    if !a.is_empty() {
                        out.push(Ok(Delta::ToolUseInput(a.to_string())));
                    }
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.close_tool(&mut out);
            self.stop = Some(map_finish(f));
        }
        out
    }

    fn finish(&mut self) -> Vec<Result<Delta, ModelError>> {
        if self.finished {
            return vec![];
        }
        if self.stop.is_some() {
            // Some servers omit `[DONE]`.
            return self.terminal();
        }
        self.finished = true;
        vec![Err(ModelError::Network { message: "stream ended before finish_reason".into() })]
    }
}

/// OpenAI chat encoder, version 1 (frozen, deterministic).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiEncoderV1;

fn flat_text(head: &SeqHead, b: &RBlock) -> Option<String> {
    Some(match b {
        RBlock::Text { text } => text.clone(),
        RBlock::Guidance { text } => reminder(text),
        RBlock::Data { source, text } => data_frame(&head.render.data_warning, source, text),
        RBlock::Image { blob } => format!("[image {} ({} bytes)]", blob.sha256, blob.size),
        RBlock::ToolResult { content, .. } => {
            content.iter().filter_map(|c| flat_text(head, c)).collect::<Vec<_>>().join("\n")
        }
        _ => return None,
    })
}

impl Encoder for OpenAiEncoderV1 {
    fn version(&self) -> u32 {
        1
    }

    fn encode(&self, head: &SeqHead, body: &[Rendered], max_tokens: u32) -> Request {
        let mut msgs: Vec<Value> = vec![];
        if !head.system.is_empty() {
            msgs.push(json!({"role": "system", "content": head.system.join("\n\n")}));
        }
        // Merge consecutive fragments of the same wire role first.
        let mut merged: Vec<(Role, Vec<RBlock>)> = vec![];
        for r in body {
            let (role, blocks): (Role, Vec<RBlock>) = if r.role == Role::System {
                let bs = r
                    .blocks
                    .iter()
                    .map(|b| match b {
                        RBlock::Text { text } => RBlock::Guidance { text: text.clone() },
                        o => o.clone(),
                    })
                    .collect();
                (Role::User, bs)
            } else {
                (r.role, r.blocks.clone())
            };
            match merged.last_mut() {
                Some((r0, bs)) if *r0 == role => bs.extend(blocks),
                _ => merged.push((role, blocks)),
            }
        }
        for (role, blocks) in merged {
            match role {
                Role::Assistant => {
                    let text: Vec<String> = blocks.iter().filter_map(|b| match b {
                        RBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    }).collect();
                    let calls: Vec<Value> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            RBlock::ToolUse { id, name, input } => Some(json!({
                                "id": id, "type": "function",
                                "function": {"name": name, "arguments": serde_json::to_string(input).unwrap_or_default()}
                            })),
                            _ => None,
                        })
                        .collect();
                    let mut m = Map::new();
                    m.insert("role".into(), json!("assistant"));
                    m.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text.join("")) });
                    if !calls.is_empty() {
                        m.insert("tool_calls".into(), Value::Array(calls));
                    }
                    msgs.push(Value::Object(m));
                }
                _ => {
                    // Tool results become `tool` messages and must come first.
                    let mut rest = vec![];
                    for b in &blocks {
                        match b {
                            RBlock::ToolResult { id, is_error, .. } => {
                                let t = flat_text(head, b).unwrap_or_default();
                                let t = if *is_error { format!("[error] {t}") } else { t };
                                msgs.push(json!({"role": "tool", "tool_call_id": id, "content": t}));
                            }
                            other => rest.extend(flat_text(head, other)),
                        }
                    }
                    if !rest.is_empty() {
                        msgs.push(json!({"role": "user", "content": rest.join("\n\n")}));
                    }
                }
            }
        }
        let mut out = Map::new();
        out.insert("messages".into(), Value::Array(msgs));
        if !head.tools.is_empty() {
            out.insert(
                "tools".into(),
                Value::Array(
                    head.tools
                        .iter()
                        .map(|t| json!({"type": "function", "function": {
                            "name": t.name, "description": t.description, "parameters": t.input_schema
                        }}))
                        .collect(),
                ),
            );
        }
        Request { encoder_version: 1, body: Value::Object(out), max_tokens }
    }
}
