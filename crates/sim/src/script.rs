//! A scripted model port and a trivial deterministic encoder.

use agent_proto::*;
use agent_runtime::{Delta, Encoder, ModelPort, Request};
use futures::stream::{self, BoxStream, StreamExt};
use std::sync::{Arc, Mutex};

pub use agent_runtime::ToolName;

/// Encoder version 1: `{"model", "seq_no", "system", "tools", "messages"}`
/// where `messages` are the renderings serialized verbatim. Deterministic.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonEncoder;

impl Encoder for JsonEncoder {
    fn version(&self) -> u32 {
        1
    }
    fn encode(&self, head: &SeqHead, body: &[Rendered], max_tokens: u32) -> Request {
        Request {
            encoder_version: 1,
            body: serde_json::json!({
                "model": head.model,
                "seq_no": head.seq_no,
                "system": head.system,
                "tools": head.tools,
                "messages": body,
            }),
            max_tokens,
        }
    }
}

#[derive(Debug, Clone)]
enum StepKind {
    Reply(AssistantMessage),
    Error(ModelError),
}

#[derive(Debug, Default)]
struct Inner {
    steps: Vec<StepKind>,
    cursor: usize,
    calls: u64,
    requests: Vec<Request>,
    check_prefix: bool,
}

/// A scripted model: each step is one model response.
///
/// ```ignore
/// let model = Script::new()
///     .call("edit", json!({"file": "README.md", "old": "foo", "new": "bar"}))
///     .say("Done");
/// ```
///
/// Clones share the script and the recorded requests.
#[derive(Debug, Clone)]
pub struct Script {
    inner: Arc<Mutex<Inner>>,
    caps: ModelCaps,
    encoder: JsonEncoder,
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

fn usage_for(msg_bytes: usize) -> Usage {
    Usage { input_tokens: 0, output_tokens: (msg_bytes as u32).div_ceil(4), ..Default::default() }
}

impl Script {
    pub fn new() -> Self {
        Script { inner: Arc::new(Mutex::new(Inner::default())), caps: ModelCaps::default(), encoder: JsonEncoder }
    }

    pub fn with_caps(mut self, caps: ModelCaps) -> Self {
        self.caps = caps;
        self
    }

    fn push(self, s: StepKind) -> Self {
        self.inner.lock().unwrap().steps.push(s);
        self
    }

    fn next_call_id(&self) -> CallId {
        let mut g = self.inner.lock().unwrap();
        g.calls += 1;
        CallId(format!("call_{}", g.calls))
    }

    fn tool_call(&self, tool: &impl ToolName, input: serde_json::Value) -> ToolCall {
        ToolCall { id: self.next_call_id(), name: tool.tool_name(), input, access: vec![], class: EffectClass::Pure }
    }

    /// A response with one tool call (stop reason `tool_use`).
    pub fn call(self, tool: impl ToolName, input: serde_json::Value) -> Self {
        let c = self.tool_call(&tool, input);
        let bytes = c.input.to_string().len();
        self.reply(AssistantMessage {
            content: vec![ContentBlock::ToolUse(c)],
            stop: StopReason::ToolUse,
            usage: usage_for(bytes),
        })
    }

    /// A response with several tool calls in one message.
    pub fn calls<T: ToolName>(self, calls: impl IntoIterator<Item = (T, serde_json::Value)>) -> Self {
        let mut content = vec![];
        let mut bytes = 0;
        for (t, input) in calls {
            let c = self.tool_call(&t, input);
            bytes += c.input.to_string().len();
            content.push(ContentBlock::ToolUse(c));
        }
        self.reply(AssistantMessage { content, stop: StopReason::ToolUse, usage: usage_for(bytes) })
    }

    /// A final text response (stop reason `end_turn`).
    pub fn say(self, text: impl Into<String>) -> Self {
        let text = text.into();
        let usage = usage_for(text.len());
        self.reply(AssistantMessage { content: vec![ContentBlock::Text { text }], stop: StopReason::EndTurn, usage })
    }

    /// A response containing only a (signed) thinking block. Use [`Script::reply`]
    /// for a message mixing thinking with text or tool calls.
    pub fn think(self, text: impl Into<String>) -> Self {
        let text = text.into();
        let usage = usage_for(text.len());
        self.reply(AssistantMessage {
            content: vec![ContentBlock::Thinking { text, signature: Some("sig".into()) }],
            stop: StopReason::EndTurn,
            usage,
        })
    }

    /// The stream fails with this error.
    pub fn error(self, e: ModelError) -> Self {
        self.push(StepKind::Error(e))
    }

    /// The stream fails with a context overflow.
    pub fn overflow(self) -> Self {
        self.error(ModelError::Overflow)
    }

    /// An arbitrary response.
    pub fn reply(self, m: AssistantMessage) -> Self {
        self.push(StepKind::Reply(m))
    }

    /// Assert every request's `messages` extends the previous request's
    /// `messages` (while model/seq/system/tools are unchanged). Violations panic.
    pub fn check_prefix(self) -> Self {
        self.inner.lock().unwrap().check_prefix = true;
        self
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<Request> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// Steps not yet consumed.
    pub fn remaining(&self) -> usize {
        let g = self.inner.lock().unwrap();
        g.steps.len() - g.cursor
    }

    /// Deltas for one step (text in chunks, tool input JSON in two chunks).
    pub fn deltas(m: &AssistantMessage) -> Vec<Delta> {
        let mut out = vec![];
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => out.extend(chunks(text).into_iter().map(Delta::Text)),
                ContentBlock::Thinking { text, signature } => {
                    out.extend(chunks(text).into_iter().map(Delta::Thinking));
                    if let Some(s) = signature {
                        out.push(Delta::ThinkingSignature(s.clone()));
                    }
                }
                ContentBlock::ToolUse(c) => {
                    out.push(Delta::ToolUseStart { id: c.id.clone(), name: c.name.clone() });
                    let json = c.input.to_string();
                    let (a, b) = split_half(&json);
                    out.push(Delta::ToolUseInput(a.to_string()));
                    out.push(Delta::ToolUseInput(b.to_string()));
                    out.push(Delta::ToolUseEnd);
                }
                ContentBlock::Opaque { vendor, data } => {
                    out.push(Delta::Opaque { vendor: vendor.clone(), data: data.clone() })
                }
            }
        }
        out.push(Delta::Usage(m.usage));
        out.push(Delta::Stop(m.stop));
        out
    }
}

fn split_half(s: &str) -> (&str, &str) {
    let mut mid = s.len() / 2;
    while !s.is_char_boundary(mid) {
        mid += 1;
    }
    s.split_at(mid)
}

/// Split text into up to 3 chunks (at char boundaries).
fn chunks(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let size = chars.len().div_ceil(3).max(1);
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

fn messages_prefix_ok(prev: &serde_json::Value, cur: &serde_json::Value) -> bool {
    let same_head = ["model", "seq_no", "system", "tools"].iter().all(|k| prev.get(k) == cur.get(k));
    if !same_head {
        return true; // a new sequence
    }
    match (prev.get("messages").and_then(|m| m.as_array()), cur.get("messages").and_then(|m| m.as_array())) {
        (Some(p), Some(c)) => c.len() >= p.len() && c[..p.len()] == p[..],
        _ => true,
    }
}

impl ModelPort for Script {
    fn caps(&self) -> &ModelCaps {
        &self.caps
    }
    fn encoder(&self) -> &dyn Encoder {
        &self.encoder
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        let mut g = self.inner.lock().unwrap();
        if g.check_prefix {
            if let Some(prev) = g.requests.last() {
                assert!(
                    messages_prefix_ok(&prev.body, &req.body),
                    "Script::check_prefix: request {} does not extend the previous request's messages",
                    g.requests.len()
                );
            }
        }
        let input_tokens = (req.body.to_string().len() as u32).div_ceil(4);
        g.requests.push(req);
        let step = g.steps.get(g.cursor).cloned();
        if step.is_some() {
            g.cursor += 1;
        }
        drop(g);
        let items: Vec<Result<Delta, ModelError>> = match step {
            Some(StepKind::Reply(mut m)) => {
                m.usage.input_tokens = input_tokens;
                Script::deltas(&m).into_iter().map(Ok).collect()
            }
            Some(StepKind::Error(e)) => vec![Err(e)],
            None => vec![Err(ModelError::Invalid { message: "script exhausted".into() })],
        };
        stream::iter(items).boxed()
    }
}

/// Assemble a delta stream into a complete message.
pub async fn collect_message(
    mut s: BoxStream<'_, Result<Delta, ModelError>>,
) -> Result<AssistantMessage, ModelError> {
    let mut content: Vec<ContentBlock> = vec![];
    let mut usage = Usage::default();
    let mut stop = StopReason::EndTurn;
    let mut tool: Option<(CallId, String, String)> = None;
    while let Some(d) = s.next().await {
        match d? {
            Delta::Text(t) => match content.last_mut() {
                Some(ContentBlock::Text { text }) => text.push_str(&t),
                _ => content.push(ContentBlock::Text { text: t }),
            },
            Delta::Thinking(t) => match content.last_mut() {
                Some(ContentBlock::Thinking { text, .. }) => text.push_str(&t),
                _ => content.push(ContentBlock::Thinking { text: t, signature: None }),
            },
            Delta::ThinkingSignature(sig) => {
                if let Some(ContentBlock::Thinking { signature, .. }) = content.last_mut() {
                    *signature = Some(sig);
                }
            }
            Delta::ToolUseStart { id, name } => tool = Some((id, name, String::new())),
            Delta::ToolUseInput(p) => {
                if let Some((_, _, buf)) = tool.as_mut() {
                    buf.push_str(&p);
                }
            }
            Delta::ToolUseEnd => {
                if let Some((id, name, buf)) = tool.take() {
                    let input = if buf.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&buf)
                            .map_err(|e| ModelError::Invalid { message: format!("tool input: {e}") })?
                    };
                    content.push(ContentBlock::ToolUse(ToolCall {
                        id,
                        name,
                        input,
                        access: vec![],
                        class: EffectClass::Pure,
                    }));
                }
            }
            Delta::Opaque { vendor, data } => content.push(ContentBlock::Opaque { vendor, data }),
            Delta::Usage(u) => usage = u,
            Delta::Stop(r) => stop = r,
        }
    }
    Ok(AssistantMessage { content, stop, usage })
}

/// Encode `prompt` with the model's encoder, stream it and collect the reply
/// into an `EffectResult` (blocking; for synchronous `World`s).
pub fn sample_blocking(model: &dyn ModelPort, prompt: &Prompt) -> EffectResult {
    let req = model.encoder().encode(&prompt.head, &prompt.body, prompt.max_tokens);
    match futures::executor::block_on(collect_message(model.stream(req))) {
        Ok(m) => EffectResult::Sampled(m),
        Err(e) => EffectResult::SampleFailed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn head() -> SeqHead {
        SeqHead {
            seq_no: 0,
            model: ModelId::new("scripted"),
            system: vec!["sys".into()],
            tools: vec![],
            render: RenderProfile::default(),
            encoder_version: 1,
        }
    }

    fn prompt(texts: &[&str]) -> Prompt {
        Prompt { head: head(), body: texts.iter().map(|t| Rendered::text(Role::User, *t)).collect(), max_tokens: 100 }
    }

    #[test]
    fn call_then_say() {
        let s = Script::new().call("edit", json!({"file": "README.md", "old": "foo", "new": "bar"})).say("Done");
        let m = match sample_blocking(&s, &prompt(&["Edit README"])) {
            EffectResult::Sampled(m) => m,
            other => panic!("{other:?}"),
        };
        let calls: Vec<_> = m.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].input["new"], "bar");
        assert_eq!(m.stop, StopReason::ToolUse);
        let m2 = match sample_blocking(&s, &prompt(&["Edit README", "ok"])) {
            EffectResult::Sampled(m) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(m2.text(), "Done");
        assert_eq!(s.requests().len(), 2);
        assert_eq!(s.remaining(), 0);
        assert!(matches!(sample_blocking(&s, &prompt(&[])), EffectResult::SampleFailed(ModelError::Invalid { .. })));
    }

    #[test]
    fn delta_shape() {
        let s = Script::new().call(String::from("read"), json!({"file": "a"}));
        let req = JsonEncoder.encode(&head(), &[], 10);
        let ds: Vec<_> = futures::executor::block_on(s.stream(req).collect::<Vec<_>>());
        let ds: Vec<Delta> = ds.into_iter().map(Result::unwrap).collect();
        assert!(matches!(ds[0], Delta::ToolUseStart { .. }));
        assert!(matches!(ds[1], Delta::ToolUseInput(_)));
        assert!(matches!(ds[2], Delta::ToolUseInput(_)));
        assert_eq!(ds[3], Delta::ToolUseEnd);
        assert!(matches!(ds[4], Delta::Usage(_)));
        assert_eq!(ds[5], Delta::Stop(StopReason::ToolUse));
        // multi-chunk text
        let d = Script::deltas(&AssistantMessage {
            content: vec![ContentBlock::Text { text: "hello world".into() }],
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        });
        assert_eq!(d.iter().filter(|x| matches!(x, Delta::Text(_))).count(), 3);
    }

    #[test]
    fn errors_overflow_and_think() {
        let s = Script::new().overflow().error(ModelError::Overloaded).think("hmm");
        assert_eq!(sample_blocking(&s, &prompt(&["a"])), EffectResult::SampleFailed(ModelError::Overflow));
        assert_eq!(sample_blocking(&s, &prompt(&["a"])), EffectResult::SampleFailed(ModelError::Overloaded));
        match sample_blocking(&s, &prompt(&["a"])) {
            EffectResult::Sampled(m) => assert!(matches!(&m.content[0],
                ContentBlock::Thinking { text, signature: Some(_) } if text == "hmm")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prefix_check_passes_on_extension() {
        let s = Script::new().say("a").say("b").say("c").check_prefix();
        sample_blocking(&s, &prompt(&["x"]));
        sample_blocking(&s, &prompt(&["x", "y"]));
        // a different head is a new sequence: no constraint
        let mut p = prompt(&["z"]);
        p.head.seq_no = 1;
        sample_blocking(&s, &p);
    }

    #[test]
    #[should_panic(expected = "check_prefix")]
    fn prefix_check_detects_rewrite() {
        let s = Script::new().say("a").say("b").check_prefix();
        sample_blocking(&s, &prompt(&["x", "y"]));
        sample_blocking(&s, &prompt(&["x2", "y"]));
    }

    #[test]
    fn encoder_is_deterministic() {
        let a = JsonEncoder.encode(&head(), &prompt(&["q"]).body, 5);
        let b = JsonEncoder.encode(&head(), &prompt(&["q"]).body, 5);
        assert_eq!(serde_json::to_vec(&a.body).unwrap(), serde_json::to_vec(&b.body).unwrap());
        assert_eq!(a.body["system"][0], "sys");
        assert_eq!(a.encoder_version, 1);
    }
}
