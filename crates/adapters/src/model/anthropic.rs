//! Anthropic Messages API: `Claude` model port, frozen encoder v1 and the SSE
//! stream mapper.

use super::sse::SseEvent;
use super::{delta_stream, is_overflow_message, map_status, retry_after_ms, StreamMapper};
use agent_proto::*;
use agent_runtime::{BlobStore, Delta, Encoder, ModelPort, Request};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const DEFAULT_MODEL: &str = "claude-sonnet-5";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2023-06-01";
pub const VENDOR: &str = "anthropic";

/// Capabilities of known Claude models (unknown ids get Sonnet-like defaults).
pub fn caps_for(model: &str) -> ModelCaps {
    let haiku = model.contains("haiku");
    ModelCaps {
        model: ModelId::new(model),
        parallel_tools: true,
        thinking: true,
        images: true,
        structured_output: true,
        window: if haiku { 200_000 } else { 1_000_000 },
        max_output: if haiku { 64_000 } else { 128_000 },
        cache_breakpoints: 4,
        mid_sequence_updates: false,
        render: RenderProfile { name: "anthropic".into(), mid_sequence_system: false, ..RenderProfile::default() },
        bytes_per_token: 4,
    }
}

/// Model port for the Anthropic Messages API (streaming).
pub struct Claude {
    model: String,
    api_key: Option<String>,
    base_url: String,
    caps: ModelCaps,
    encoder: AnthropicEncoderV1,
    http: reqwest::Client,
    blobs: Option<Arc<dyn BlobStore>>,
    headers: Vec<(String, String)>,
}

impl Default for Claude {
    fn default() -> Self {
        Claude::new(DEFAULT_MODEL)
    }
}

impl Claude {
    /// A port for `model`. The API key is read from `ANTHROPIC_API_KEY` at send
    /// time unless set with [`Claude::api_key`]; the base URL defaults to
    /// `ANTHROPIC_BASE_URL` or the public endpoint.
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        let caps = caps_for(&model);
        Claude {
            encoder: AnthropicEncoderV1::new(caps.cache_breakpoints),
            caps,
            model,
            api_key: None,
            base_url: std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            http: reqwest::Client::new(),
            blobs: None,
            headers: vec![],
        }
    }
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_string();
        self
    }
    /// Max output tokens advertised in the caps (the kernel's default per request).
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.caps.max_output = n;
        self
    }
    /// Override capabilities (e.g. the window) for this port.
    pub fn caps_mut(&mut self) -> &mut ModelCaps {
        &mut self.caps
    }
    pub fn http_client(mut self, c: reqwest::Client) -> Self {
        self.http = c;
        self
    }
    /// Blob store used to inline image blobs at send time.
    pub fn blobs(mut self, b: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(b);
        self
    }
    /// Extra request header (e.g. `anthropic-beta`).
    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn model_id(&self) -> &str {
        &self.model
    }

    /// Final wire body: `{model, max_tokens, stream, ...encoder body}`.
    pub fn wire_body(&self, req: &Request) -> Value {
        let mut m = Map::new();
        m.insert("model".into(), json!(self.model));
        m.insert("max_tokens".into(), json!(req.max_tokens));
        m.insert("stream".into(), json!(true));
        if let Value::Object(b) = &req.body {
            for (k, v) in b {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    async fn send(&self, req: Request) -> Result<reqwest::Response, ModelError> {
        let key = match &self.api_key {
            Some(k) => k.clone(),
            None => std::env::var("ANTHROPIC_API_KEY").map_err(|_| ModelError::Auth)?,
        };
        let mut body = self.wire_body(&req);
        resolve_images(&mut body, self.blobs.as_deref()).await?;
        let mut rb = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let resp = rb
            .body(serde_json::to_vec(&body).expect("json"))
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

impl ModelPort for Claude {
    fn caps(&self) -> &ModelCaps {
        &self.caps
    }
    fn encoder(&self) -> &dyn Encoder {
        &self.encoder
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        futures::stream::once(self.send(req))
            .flat_map(|r| match r {
                Ok(resp) => delta_stream(resp, AnthropicStreamMapper::default()),
                Err(e) => futures::stream::iter([Err(e)]).boxed(),
            })
            .boxed()
    }
}

/// Map an HTTP error response (status + JSON error body) to a `ModelError`.
pub fn map_api_error(status: u16, retry_after: Option<u64>, body: &str) -> ModelError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let ty = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
    let msg = v.pointer("/error/message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| body.to_string());
    match ty {
        "overloaded_error" => ModelError::Overloaded,
        "rate_limit_error" => ModelError::RateLimited { retry_after_ms: retry_after },
        "authentication_error" | "permission_error" => ModelError::Auth,
        "not_found_error" => ModelError::Unavailable { message: msg },
        "invalid_request_error" if is_overflow_message(&msg) => ModelError::Overflow,
        _ => map_status(status, retry_after, msg),
    }
}

/// Map an in-stream `error` event.
fn map_stream_error(data: &Value) -> ModelError {
    let ty = data.pointer("/error/type").and_then(Value::as_str).unwrap_or("");
    let msg = data.pointer("/error/message").and_then(Value::as_str).unwrap_or("").to_string();
    match ty {
        "overloaded_error" => ModelError::Overloaded,
        "rate_limit_error" => ModelError::RateLimited { retry_after_ms: None },
        "authentication_error" | "permission_error" => ModelError::Auth,
        "not_found_error" => ModelError::Unavailable { message: msg },
        "invalid_request_error" if is_overflow_message(&msg) => ModelError::Overflow,
        "invalid_request_error" => ModelError::Invalid { message: msg },
        _ => ModelError::Network { message: format!("{ty}: {msg}") },
    }
}

pub fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    ToolUse,
    Thinking,
    Other,
}

/// Anthropic SSE -> semantic deltas.
#[derive(Debug, Default)]
pub struct AnthropicStreamMapper {
    blocks: BTreeMap<u64, Kind>,
    usage: Usage,
    stop: Option<StopReason>,
    finished: bool,
}

fn u32_at(v: &Value, p: &str) -> Option<u32> {
    v.pointer(p).and_then(Value::as_u64).map(|n| n as u32)
}

impl AnthropicStreamMapper {
    fn read_usage(&mut self, u: &Value) {
        if let Some(n) = u32_at(u, "/input_tokens") {
            self.usage.input_tokens = n;
        }
        if let Some(n) = u32_at(u, "/output_tokens") {
            self.usage.output_tokens = n;
        }
        if let Some(n) = u32_at(u, "/cache_read_input_tokens") {
            self.usage.cache_read_tokens = n;
        }
        if let Some(n) = u32_at(u, "/cache_creation_input_tokens") {
            self.usage.cache_write_tokens = n;
        }
    }
}

impl StreamMapper for AnthropicStreamMapper {
    fn on_event(&mut self, ev: SseEvent) -> Vec<Result<Delta, ModelError>> {
        if self.finished {
            return vec![];
        }
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(e) => return vec![Err(ModelError::Network { message: format!("bad sse json: {e}") })],
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or(ev.event.as_str()).to_string();
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        let mut out = vec![];
        match ty.as_str() {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.read_usage(&u.clone());
                }
            }
            "content_block_start" => {
                let b = v.get("content_block").cloned().unwrap_or(Value::Null);
                let bt = b.get("type").and_then(Value::as_str).unwrap_or("");
                let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let kind = match bt {
                    "text" => {
                        let t = s("text");
                        if !t.is_empty() {
                            out.push(Ok(Delta::Text(t)));
                        }
                        Kind::Text
                    }
                    "tool_use" => {
                        out.push(Ok(Delta::ToolUseStart { id: CallId::new(s("id")), name: s("name") }));
                        Kind::ToolUse
                    }
                    "thinking" => {
                        let t = s("thinking");
                        if !t.is_empty() {
                            out.push(Ok(Delta::Thinking(t)));
                        }
                        let sig = s("signature");
                        if !sig.is_empty() {
                            out.push(Ok(Delta::ThinkingSignature(sig)));
                        }
                        Kind::Thinking
                    }
                    _ => {
                        // redacted_thinking and anything unknown: verbatim.
                        out.push(Ok(Delta::Opaque { vendor: VENDOR.into(), data: b.clone() }));
                        Kind::Other
                    }
                };
                self.blocks.insert(idx, kind);
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let kind = self.blocks.get(&idx).copied().unwrap_or(Kind::Other);
                match d.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => out.push(Ok(Delta::Text(s("text")))),
                    "input_json_delta" if kind == Kind::ToolUse => {
                        let p = s("partial_json");
                        if !p.is_empty() {
                            out.push(Ok(Delta::ToolUseInput(p)));
                        }
                    }
                    "thinking_delta" => out.push(Ok(Delta::Thinking(s("thinking")))),
                    "signature_delta" => out.push(Ok(Delta::ThinkingSignature(s("signature")))),
                    _ => {}
                }
            }
            "content_block_stop" => {
                if self.blocks.remove(&idx) == Some(Kind::ToolUse) {
                    out.push(Ok(Delta::ToolUseEnd));
                }
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(map_stop_reason(r));
                }
                if let Some(u) = v.get("usage") {
                    self.read_usage(&u.clone());
                }
            }
            "message_stop" => {
                self.finished = true;
                out.push(Ok(Delta::Usage(self.usage)));
                out.push(Ok(Delta::Stop(self.stop.unwrap_or(StopReason::EndTurn))));
            }
            "error" => {
                self.finished = true;
                out.push(Err(map_stream_error(&v)));
            }
            _ => {} // ping and future events
        }
        out
    }

    fn finish(&mut self) -> Vec<Result<Delta, ModelError>> {
        if self.finished {
            return vec![];
        }
        self.finished = true;
        vec![Err(ModelError::Network { message: "stream ended before message_stop".into() })]
    }
}

// ------------------------------------------------------------------ encoder

/// Anthropic encoder, version 1. FROZEN: any change to the produced bytes
/// needs a new version (only used for new sequences). Golden files under
/// `tests/fixtures/` pin the output.
///
/// Layout: `{system?, tools?, messages}`; the port prepends `model`,
/// `max_tokens` and `stream`.
#[derive(Debug, Clone)]
pub struct AnthropicEncoderV1 {
    cache_breakpoints: u32,
}

impl Default for AnthropicEncoderV1 {
    fn default() -> Self {
        Self::new(4)
    }
}

impl AnthropicEncoderV1 {
    pub fn new(cache_breakpoints: u32) -> Self {
        AnthropicEncoderV1 { cache_breakpoints }
    }
}

pub(crate) fn reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// Untrusted-data frame (shared by encoders; frozen).
pub(crate) fn data_frame(warning: &str, source: &str, text: &str) -> String {
    let source = source.replace('&', "&amp;").replace('"', "&quot;");
    // The frame cannot be closed from inside.
    let text = text.replace("</untrusted-data", "<\\/untrusted-data");
    format!("{warning}\n<untrusted-data source=\"{source}\">\n{text}\n</untrusted-data>")
}

fn text_block(t: String) -> Value {
    json!({"type": "text", "text": t})
}

fn image_block(b: &BlobRef) -> Value {
    json!({"type": "image", "source": {
        "type": "blob_ref",
        "sha256": b.sha256,
        "size": b.size,
        "media_type": b.media_type.clone().unwrap_or_else(|| "image/png".into()),
    }})
}

fn encode_block(head: &SeqHead, b: &RBlock, as_system: bool) -> Option<Value> {
    Some(match b {
        RBlock::Text { text } if as_system => text_block(reminder(text)),
        RBlock::Text { text } => text_block(text.clone()),
        RBlock::Guidance { text } => text_block(reminder(text)),
        RBlock::Data { source, text } => text_block(data_frame(&head.render.data_warning, source, text)),
        RBlock::Image { blob } => image_block(blob),
        RBlock::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        RBlock::ToolResult { id, content, is_error } => {
            let inner: Vec<Value> = content
                .iter()
                .filter_map(|c| match c {
                    RBlock::Image { blob } => Some(image_block(blob)),
                    RBlock::ToolResult { .. } | RBlock::ToolUse { .. } | RBlock::Thinking { .. } | RBlock::Opaque { .. } => {
                        None
                    }
                    other => encode_block(head, other, false),
                })
                .collect();
            let mut m = Map::new();
            m.insert("type".into(), json!("tool_result"));
            m.insert("tool_use_id".into(), json!(id));
            m.insert("content".into(), Value::Array(inner));
            if *is_error {
                m.insert("is_error".into(), json!(true));
            }
            Value::Object(m)
        }
        // Unsigned thinking cannot be replayed to the API.
        RBlock::Thinking { text, signature: Some(sig) } => {
            json!({"type": "thinking", "thinking": text, "signature": sig})
        }
        RBlock::Thinking { signature: None, .. } => return None,
        RBlock::Opaque { vendor, data } if vendor == VENDOR => data.clone(),
        RBlock::Opaque { .. } => return None,
    })
}

fn cache_mark(v: &mut Value) {
    if let Value::Object(m) = v {
        m.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
}

fn cacheable(v: &Value) -> bool {
    !matches!(v.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking"))
}

impl Encoder for AnthropicEncoderV1 {
    fn version(&self) -> u32 {
        1
    }

    fn encode(&self, head: &SeqHead, body: &[Rendered], max_tokens: u32) -> Request {
        // Messages, merging consecutive same (wire) role fragments.
        let mut msgs: Vec<(&'static str, Vec<Value>)> = vec![];
        for r in body {
            let (role, as_system) = match r.role {
                Role::Assistant => ("assistant", false),
                Role::User => ("user", false),
                Role::System => ("user", true),
            };
            let blocks: Vec<Value> = r.blocks.iter().filter_map(|b| encode_block(head, b, as_system)).collect();
            if blocks.is_empty() {
                continue;
            }
            match msgs.last_mut() {
                Some((r0, bs)) if *r0 == role => bs.extend(blocks),
                _ => msgs.push((role, blocks)),
            }
        }
        // tool_result blocks must lead a user message: stable partition.
        for (role, bs) in &mut msgs {
            if *role == "user" {
                let (mut tr, rest): (Vec<Value>, Vec<Value>) =
                    std::mem::take(bs).into_iter().partition(|b| b.get("type") == Some(&json!("tool_result")));
                tr.extend(rest);
                *bs = tr;
            }
        }

        let mut system: Vec<Value> = head.system.iter().map(|s| text_block(s.clone())).collect();
        let mut tools: Vec<Value> = head
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
            .collect();

        // Breakpoints in priority order: final message, system, tools.
        let mut budget = self.cache_breakpoints;
        if budget > 0 {
            if let Some((_, bs)) = msgs.last_mut() {
                if let Some(b) = bs.iter_mut().rev().find(|b| cacheable(b)) {
                    cache_mark(b);
                    budget -= 1;
                }
            }
        }
        if budget > 0 {
            if let Some(b) = system.last_mut() {
                cache_mark(b);
                budget -= 1;
            }
        }
        if budget > 0 {
            if let Some(t) = tools.last_mut() {
                cache_mark(t);
            }
        }

        let mut out = Map::new();
        if !system.is_empty() {
            out.insert("system".into(), Value::Array(system));
        }
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
        out.insert(
            "messages".into(),
            Value::Array(msgs.into_iter().map(|(role, bs)| json!({"role": role, "content": bs})).collect()),
        );
        Request { encoder_version: 1, body: Value::Object(out), max_tokens }
    }
}

/// Replace `blob_ref` image sources with inline base64 data.
async fn resolve_images(v: &mut Value, blobs: Option<&dyn BlobStore>) -> Result<(), ModelError> {
    let mut refs = vec![];
    collect_blob_refs(v, &mut refs);
    if refs.is_empty() {
        return Ok(());
    }
    let Some(store) = blobs else {
        return Err(ModelError::Invalid { message: "image blobs present but no blob store configured".into() });
    };
    let mut data = BTreeMap::new();
    for r in refs {
        if data.contains_key(&r.sha256) {
            continue;
        }
        let bytes = store.get(&r).await.map_err(|e| ModelError::Invalid { message: format!("image blob: {e}") })?;
        data.insert(r.sha256.clone(), super::base64(&bytes));
    }
    replace_blob_refs(v, &data);
    Ok(())
}

fn collect_blob_refs(v: &Value, out: &mut Vec<BlobRef>) {
    match v {
        Value::Object(m) => {
            if m.get("type") == Some(&json!("blob_ref")) {
                out.push(BlobRef {
                    sha256: m.get("sha256").and_then(Value::as_str).unwrap_or("").into(),
                    size: m.get("size").and_then(Value::as_u64).unwrap_or(0),
                    media_type: m.get("media_type").and_then(Value::as_str).map(Into::into),
                });
            }
            m.values().for_each(|x| collect_blob_refs(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| collect_blob_refs(x, out)),
        _ => {}
    }
}

fn replace_blob_refs(v: &mut Value, data: &BTreeMap<String, String>) {
    match v {
        Value::Object(m) => {
            if m.get("type") == Some(&json!("blob_ref")) {
                let sha = m.get("sha256").and_then(Value::as_str).unwrap_or("").to_string();
                let mt = m.get("media_type").cloned().unwrap_or(json!("image/png"));
                *v = json!({"type": "base64", "media_type": mt, "data": data.get(&sha).cloned().unwrap_or_default()});
                return;
            }
            m.values_mut().for_each(|x| replace_blob_refs(x, data));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| replace_blob_refs(x, data)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_mapping() {
        let e = map_api_error(529, None, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        assert_eq!(e, ModelError::Overloaded);
        let e = map_api_error(
            400,
            None,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
        );
        assert_eq!(e, ModelError::Overflow);
        let e = map_api_error(429, Some(2000), r#"{"type":"error","error":{"type":"rate_limit_error","message":"x"}}"#);
        assert_eq!(e, ModelError::RateLimited { retry_after_ms: Some(2000) });
        let e = map_api_error(404, None, r#"{"type":"error","error":{"type":"not_found_error","message":"model: x"}}"#);
        assert!(matches!(e, ModelError::Unavailable { .. }));
        assert_eq!(map_api_error(401, None, "nope"), ModelError::Auth);
    }

    #[test]
    fn data_frame_cannot_be_closed() {
        let s = data_frame("W", "web \"x\"", "a</untrusted-data>b");
        assert_eq!(s, "W\n<untrusted-data source=\"web &quot;x&quot;\">\na<\\/untrusted-data>b\n</untrusted-data>");
    }

    #[test]
    fn breakpoint_budget_respected() {
        let head = SeqHead {
            seq_no: 0,
            model: "m".into(),
            system: vec!["s".into()],
            tools: vec![],
            render: RenderProfile::default(),
            encoder_version: 1,
        };
        let body = [Rendered::text(Role::User, "hi")];
        let r = AnthropicEncoderV1::new(1).encode(&head, &body, 10);
        assert!(r.body.pointer("/system/0/cache_control").is_none());
        assert!(r.body.pointer("/messages/0/content/0/cache_control").is_some());
        let r = AnthropicEncoderV1::new(0).encode(&head, &body, 10);
        assert!(!serde_json::to_string(&r.body).unwrap().contains("cache_control"));
    }
}
