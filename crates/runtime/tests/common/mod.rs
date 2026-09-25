//! Toy decider, scripted model and tools shared by the runtime tests.
#![allow(dead_code)]

use agent_kernel::{Decider, Decision, Rejection};
use agent_proto::*;
use agent_runtime::*;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- toy decider

#[derive(Debug, Clone, Default)]
pub struct ToyState {
    /// Descriptions of every accepted input, in decide order.
    pub inputs: Vec<String>,
    pub outstanding: BTreeMap<EffectId, Effect>,
    pub next_n: u64,
    pub epoch: u32,
    pub replies: Vec<AssistantMessage>,
    pub results: Vec<ToolResult>,
    pub verdicts: Vec<(Verdict, Responder)>,
    /// `remember` flags of gate completions, in order.
    pub remembers: Vec<bool>,
    pub streamed: Vec<ToolCall>,
    pub other: Vec<EffectResult>,
}

pub struct Toy;

fn marker(desc: String) -> Draft<Event> {
    Draft::internal(Event::Plugin { kind: "input".into(), ignorable: true, data: json!(desc) })
}

pub fn prompt() -> Prompt {
    Prompt {
        head: SeqHead {
            seq_no: 0,
            model: ModelId::new("scripted"),
            system: vec!["sys".into()],
            tools: vec![],
            render: RenderProfile::default(),
            encoder_version: 1,
        },
        body: vec![Rendered::text(Role::User, "hi")],
        max_tokens: 100,
    }
}

fn issue(s: &ToyState, k: u64, effect: Effect, events: &mut Vec<Draft<Event>>, effects: &mut Vec<(EffectId, Effect)>) {
    let id = EffectId { epoch: s.epoch, n: s.next_n + k };
    events.push(Draft::internal(Event::EffectIssued { id, effect: effect.clone() }));
    effects.push((id, effect));
}

fn finish(s: &ToyState, k: u64, text: String, events: &mut Vec<Draft<Event>>, effects: &mut Vec<(EffectId, Effect)>) {
    let id = EffectId { epoch: s.epoch, n: s.next_n + k };
    let eff = Effect::Finish(TurnOutcome::Done { text });
    events.push(Draft::internal(Event::EffectIssued { id, effect: eff.clone() }));
    events.push(Draft::internal(Event::EffectSettled { id }));
    effects.push((id, eff));
}

fn describe(r: &EffectResult) -> String {
    match r {
        EffectResult::Sampled(m) => format!("sampled:{:?}", m.stop).to_lowercase(),
        EffectResult::Executed(_) => "executed".into(),
        EffectResult::Gated { .. } => "gated".into(),
        other => format!("{other:?}").chars().take(20).collect(),
    }
}

impl Toy {
    pub fn start() -> Decision {
        Decision { events: vec![marker("start".into())], effects: vec![] }
    }
}

impl Decider for Toy {
    type State = ToyState;

    fn decide(s: &ToyState, _at: Timestamp, input: Input) -> Result<Decision, Rejection> {
        let mut events = vec![];
        let mut effects = vec![];
        match input {
            Input::Signal(Signal::Submit { text, .. }) => {
                if text == "reject" {
                    return Err(Rejection::new("rejected by toy"));
                }
                events.push(marker("submit".into()));
                events.push(Draft {
                    parent: Parent::Head,
                    origin: Origin::User,
                    trust: Trust::User,
                    audience: Audience::Both,
                    body: Event::UserMessage { text, attachments: vec![] },
                    rendered: None,
                });
                issue(s, 0, Effect::Sample(prompt()), &mut events, &mut effects);
            }
            Input::Signal(Signal::Notify { key, text, .. }) if key == "gate" => {
                events.push(marker(format!("gate:{text}")));
                let (level, qid) = match text.strip_prefix("inv:") {
                    Some(q) => (ApprovalLevel::Invariant, q.to_string()),
                    None => (ApprovalLevel::Policy, text.clone()),
                };
                let req = GateRequest {
                    point: HookPoint::PreTool,
                    ring: Ring::Human,
                    subject: GateSubject::PreSample,
                    question: Some(Question {
                        id: QuestionId(qid.clone()),
                        prompt: "ok?".into(),
                        level,
                        ring: Ring::Human,
                        rules: vec![],
                        remember_destination: None,
                    }),
                    level,
                    tainted: false,
                };
                issue(s, 0, Effect::Gate(req), &mut events, &mut effects);
            }
            Input::Signal(Signal::Notify { key, text, .. }) if key == "hook" => {
                events.push(marker("hook".into()));
                let req = GateRequest {
                    point: HookPoint::PreTool,
                    ring: Ring::Hook,
                    subject: GateSubject::UserSubmit { text },
                    question: None,
                    level: ApprovalLevel::Policy,
                    tainted: false,
                };
                issue(s, 0, Effect::Gate(req), &mut events, &mut effects);
            }
            Input::Signal(Signal::Notify { key, text, .. }) if key == "exec" => {
                events.push(marker("exec".into()));
                let calls: Vec<ToolCall> = serde_json::from_str(&text).unwrap();
                let grants = calls.iter().map(|c| (c.id.clone(), c.access.clone())).collect();
                issue(s, 0, Effect::Execute(Batch { calls, grants }), &mut events, &mut effects);
            }
            Input::Streamed(id, call) => {
                events.push(marker(format!("streamed:{id}:{}", call.id)));
                events.push(Draft::internal(Event::Plugin {
                    kind: "streamed".into(),
                    ignorable: true,
                    data: serde_json::to_value(&call).unwrap(),
                }));
            }
            Input::Completed(id, r) => {
                if id.epoch != s.epoch || !s.outstanding.contains_key(&id) {
                    return Err(Rejection::new(format!("stale or unknown effect {id}")));
                }
                events.push(marker(format!("completed:{id}:{}", describe(&r))));
                match r {
                    EffectResult::Sampled(m) => {
                        let interrupted = m.stop == StopReason::Interrupted;
                        let calls: Vec<ToolCall> = m.tool_calls().cloned().collect();
                        let text = m.text();
                        events.push(Draft::internal(Event::AssistantReplied { message: m, effect: id }));
                        events.push(Draft::internal(Event::EffectSettled { id }));
                        if interrupted {
                        } else if !calls.is_empty() {
                            let grants = calls.iter().map(|c| (c.id.clone(), c.access.clone())).collect();
                            issue(s, 0, Effect::Execute(Batch { calls, grants }), &mut events, &mut effects);
                        } else {
                            finish(s, 0, text, &mut events, &mut effects);
                        }
                    }
                    EffectResult::Executed(rs) => {
                        for r in rs {
                            let call = ToolCall {
                                id: r.call_id.clone(),
                                name: String::new(),
                                input: json!({}),
                                access: vec![],
                                class: EffectClass::Pure,
                            };
                            events.push(Draft::internal(Event::ToolResulted { call, result: r }));
                        }
                        events.push(Draft::internal(Event::EffectSettled { id }));
                        issue(s, 0, Effect::Sample(prompt()), &mut events, &mut effects);
                    }
                    EffectResult::Gated { verdict, responder, remember } => {
                        events.push(Draft::internal(Event::Plugin {
                            kind: "remember".into(),
                            ignorable: true,
                            data: json!(remember),
                        }));
                        events.push(Draft::internal(Event::VerdictRecorded {
                            subject: GateRef::Session,
                            point: HookPoint::PreTool,
                            ring: Ring::Human,
                            verdict,
                            responder,
                        }));
                        events.push(Draft::internal(Event::EffectSettled { id }));
                        finish(s, 0, "gated".into(), &mut events, &mut effects);
                    }
                    other => {
                        events.push(Draft::internal(Event::Plugin {
                            kind: "other".into(),
                            ignorable: true,
                            data: serde_json::to_value(&other).unwrap(),
                        }));
                        events.push(Draft::internal(Event::EffectSettled { id }));
                        finish(s, 0, "other".into(), &mut events, &mut effects);
                    }
                }
            }
            Input::Control(Control::HardInterrupt) => {
                events.push(marker("control:hard_interrupt".into()));
                events.push(Draft::internal(Event::Interrupted { hard: true, epoch: s.epoch + 1 }));
            }
            Input::Control(c) => events.push(marker(format!("control:{c:?}"))),
            Input::Signal(sig) => events.push(marker(format!("signal:{sig:?}"))),
        }
        Ok(Decision { events, effects })
    }

    fn evolve(s: &mut ToyState, ev: &Envelope<Event>) {
        match &ev.body {
            Event::Plugin { kind, data, .. } if kind == "input" => s.inputs.push(data.as_str().unwrap().to_string()),
            Event::Plugin { kind, data, .. } if kind == "streamed" => {
                s.streamed.push(serde_json::from_value(data.clone()).unwrap())
            }
            Event::Plugin { kind, data, .. } if kind == "remember" => s.remembers.push(data.as_bool().unwrap()),
            Event::Plugin { kind, data, .. } if kind == "other" => {
                s.other.push(serde_json::from_value(data.clone()).unwrap())
            }
            Event::EffectIssued { id, effect } => {
                s.outstanding.insert(*id, effect.clone());
                s.next_n = s.next_n.max(id.n + 1);
            }
            Event::EffectSettled { id } => {
                s.outstanding.remove(id);
            }
            Event::AssistantReplied { message, .. } => s.replies.push(message.clone()),
            Event::ToolResulted { result, .. } => s.results.push(result.clone()),
            Event::VerdictRecorded { verdict, responder, .. } => s.verdicts.push((verdict.clone(), responder.clone())),
            Event::Interrupted { epoch, .. } => {
                s.epoch = *epoch;
                s.outstanding.clear();
            }
            _ => {}
        }
    }

    fn outstanding(s: &ToyState) -> Vec<(EffectId, Effect)> {
        s.outstanding.iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

// ---------------------------------------------------------------- scripted model

#[derive(Debug, Clone)]
pub enum Step {
    D(Delta),
    Fail(ModelError),
    Sleep(u64),
    Hang,
}

pub struct TestEncoder;
impl Encoder for TestEncoder {
    fn version(&self) -> u32 {
        1
    }
    fn encode(&self, head: &SeqHead, body: &[Rendered], max_tokens: u32) -> Request {
        Request { encoder_version: 1, body: json!({"system": head.system, "n": body.len()}), max_tokens }
    }
}

type Check = Arc<dyn Fn() + Send + Sync>;

pub struct ScriptModel {
    caps: ModelCaps,
    scripts: Mutex<VecDeque<Vec<Step>>>,
    pub calls: AtomicUsize,
    pub on_stream: Mutex<Option<Check>>,
}

impl ScriptModel {
    pub fn new(scripts: Vec<Vec<Step>>) -> Arc<Self> {
        Arc::new(ScriptModel {
            caps: ModelCaps::default(),
            scripts: Mutex::new(scripts.into()),
            calls: AtomicUsize::new(0),
            on_stream: Mutex::new(None),
        })
    }
}

pub fn text_reply(t: &str) -> Vec<Step> {
    vec![Step::D(Delta::Text(t.into())), Step::D(Delta::Stop(StopReason::EndTurn))]
}

impl ModelPort for ScriptModel {
    fn caps(&self) -> &ModelCaps {
        &self.caps
    }
    fn encoder(&self) -> &dyn Encoder {
        &TestEncoder
    }
    fn stream(&self, _req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = self.on_stream.lock().unwrap().clone() {
            c();
        }
        let steps = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| text_reply("done"));
        stream::unfold(steps.into_iter(), |mut it| async move {
            loop {
                match it.next()? {
                    Step::D(d) => return Some((Ok(d), it)),
                    Step::Fail(e) => return Some((Err(e), it)),
                    Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    Step::Hang => futures::future::pending::<()>().await,
                }
            }
        })
        .boxed()
    }
}

// ---------------------------------------------------------------- tools

pub const WS: &str = "/ws";

/// `echo {file, text, repeat}`: Pure, reads `fs:///ws/<file>`.
pub struct Echo {
    pub calls: AtomicUsize,
    pub check: Mutex<Option<Check>>,
}

impl Echo {
    pub fn new() -> Arc<Self> {
        Arc::new(Echo { calls: AtomicUsize::new(0), check: Mutex::new(None) })
    }
}

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: json!({"type":"object"}),
            class: EffectClass::Pure,
            subagent: false,
        }
    }
    fn access(&self, input: &serde_json::Value, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        let f = input.get("file").and_then(|v| v.as_str()).ok_or(ToolError::InvalidInput("file".into()))?;
        Ok(vec![Access::read(ResourceUri::fs(&format!("{}/{f}", ctx.workspace.display())))])
    }
    async fn call(&self, input: serde_json::Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = self.check.lock().unwrap().clone() {
            c();
        }
        if ctx.grants.is_empty() {
            return Err(ToolError::NotGranted("nothing granted".into()));
        }
        (ctx.progress)("working".into());
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let n = input.get("repeat").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        if text == "fail" {
            return Err(ToolError::Failed("boom".into()));
        }
        if text == "infra" {
            return Err(ToolError::Infra("disk on fire".into()));
        }
        Ok(ToolOutput::text(text.repeat(n)))
    }
}

/// A tool with a fixed class and access that counts calls; optionally hangs.
pub struct Fixed {
    pub name: String,
    pub class: EffectClass,
    pub access: Vec<Access>,
    pub calls: AtomicUsize,
    pub hang: bool,
}

impl Fixed {
    pub fn new(name: &str, class: EffectClass, access: Vec<Access>, hang: bool) -> Arc<Self> {
        Arc::new(Fixed { name: name.into(), class, access, calls: AtomicUsize::new(0), hang })
    }
}

#[async_trait]
impl Tool for Fixed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            class: self.class,
            subagent: false,
        }
    }
    fn access(&self, _input: &serde_json::Value, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(self.access.clone())
    }
    async fn call(&self, _input: serde_json::Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hang {
            ctx.cancel.cancelled().await;
            return Err(ToolError::Cancelled);
        }
        Ok(ToolOutput::text(format!("{} ran", self.name)))
    }
}

pub fn call(reg: &ToolRegistry, id: &str, name: &str, input: serde_json::Value) -> ToolCall {
    reg.enrich(CallId::new(id), name, input)
}

pub fn options() -> RuntimeOptions {
    RuntimeOptions { workspace: WS.into(), ..RuntimeOptions::default() }
}

pub fn submit(t: &str) -> Input {
    Input::Signal(Signal::Submit { text: t.into(), attachments: vec![] })
}

pub fn notify(key: &str, text: &str) -> Input {
    Input::Signal(Signal::Notify { source: "test".into(), key: key.into(), text: text.into(), untrusted: false })
}

/// Poll until `f` is true (max ~5s).
pub async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}
