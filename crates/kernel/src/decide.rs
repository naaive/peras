//! `decide`: input → drafted events + effects.
//!
//! `decide` works on a private clone of the state: every drafted event is folded
//! into the clone with the same `evolve` the driver will run, so follow-up
//! decisions within one input see exactly the state the journal will produce.
//! The caller's state is never touched.

use crate::context::{self, SummaryPlan};
use crate::gate;
use crate::render::{self, RuleSet};
use crate::sched;
use crate::state::*;
use crate::{Decision, Rejection};
use agent_proto::*;

pub(crate) struct Cx {
    pub s: State,
    pub at: Timestamp,
    pub out: Decision,
    k: u64,
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Hash used for `ConfigChanged.profile_hash` when a config arrives through
/// `Control::Reconfigure` (FNV-1a over its canonical JSON).
pub fn config_hash(c: &KernelConfig) -> String {
    let v = serde_json::to_value(c).unwrap_or_default();
    fnv(&gate::canonical(&v))
}

fn user_draft(body: Event) -> Draft<Event> {
    Draft { audience: Audience::User, ..Draft::internal(body) }
}

pub fn decide(s: &State, at: Timestamp, input: Input) -> Result<Decision, Rejection> {
    if s.config.is_none() {
        return Err(Rejection::new("session not started: call start_session first"));
    }
    let mut cx = Cx { s: s.clone(), at, out: Decision::default(), k: 0 };
    match input {
        Input::Signal(sig) => cx.signal(sig)?,
        Input::Control(c) => cx.control(c)?,
        Input::Streamed(id, call) => {
            if !cx.streamed(id, call) {
                return Ok(Decision::default());
            }
        }
        Input::Completed(id, r) => {
            if !cx.completed(id, r)? {
                return Ok(Decision::default());
            }
        }
    }
    cx.advance();
    Ok(cx.out)
}

/// Drafts for a new session: `SessionStarted` + the first `SequenceOpened`.
pub fn start_session(session: SessionId, profile_hash: String, config: KernelConfig) -> Decision {
    let head = SeqHead {
        seq_no: 0,
        model: config.caps.model.clone(),
        system: config.system.clone(),
        tools: config.tools.clone(),
        render: config.caps.render.clone(),
        encoder_version: config.encoder_version,
    };
    Decision {
        events: vec![
            Draft {
                origin: Origin::System,
                ..user_draft(Event::SessionStarted { session, profile_hash, config, parent_session: None })
            },
            Draft::internal(Event::SequenceOpened { head }),
        ],
        effects: vec![],
    }
}

impl Cx {
    // ------------------------------------------------------------ emitting

    fn emit(&mut self, d: Draft<Event>) {
        let Draft { parent, origin, trust, audience, body, rendered } = d;
        let env = Envelope {
            id: EventId(format!("~draft.{}.{}", self.s.next_seq, self.k)),
            parent: None,
            seq: self.s.next_seq,
            at: self.at,
            origin,
            trust,
            audience,
            schema: EVENT_SCHEMA,
            body,
            rendered,
        };
        self.k += 1;
        evolve(&mut self.s, &env);
        let Envelope { origin, trust, audience, body, rendered, .. } = env;
        self.out.events.push(Draft { parent, origin, trust, audience, body, rendered });
    }

    fn internal(&mut self, ev: Event) {
        self.emit(Draft::internal(ev));
    }

    fn user(&mut self, ev: Event) {
        self.emit(user_draft(ev));
    }

    fn profile(&self) -> RenderProfile {
        self.s.head.as_ref().map(|h| h.render.clone()).unwrap_or_default()
    }

    /// A model-visible event, rendered at write time.
    fn visible(&mut self, origin: Origin, trust: Trust, body: Event) {
        let rendered = render::render(&RuleSet::default(), &self.profile(), &trust, &body);
        self.emit(Draft { parent: Parent::Head, origin, trust, audience: Audience::Both, body, rendered });
    }

    fn issue(&mut self, e: Effect) -> EffectId {
        let id = EffectId { epoch: self.s.epoch, n: self.s.next_n };
        let paused = self.s.paused;
        self.internal(Event::EffectIssued { id, effect: e.clone() });
        if !paused {
            self.out.effects.push((id, e));
        }
        id
    }

    fn settle(&mut self, id: EffectId) {
        self.internal(Event::EffectSettled { id });
    }

    fn replaced(&mut self, rep: Replacement) {
        self.emit(Draft { audience: Audience::Model, ..Draft::internal(Event::Replaced(rep)) });
    }

    fn cfg(&self) -> &KernelConfig {
        self.s.config.as_ref().expect("config present")
    }

    fn tainted(&self) -> bool {
        self.s.taint.tainted
    }

    fn gate_req(&self, point: HookPoint, ring: Ring, subject: GateSubject, question: Option<Question>) -> GateRequest {
        let level = question.as_ref().map(|q| q.level).unwrap_or(ApprovalLevel::Policy);
        GateRequest { point, ring, subject, question, level, tainted: self.tainted() }
    }

    fn tool_resulted(&mut self, call: &ToolCall, mut result: ToolResult, executed: bool) {
        result.call_id = call.id.clone();
        let trust = if executed {
            Trust::weakest(&result.trust, &gate::access_trust(&self.s, call))
        } else {
            result.trust.clone()
        };
        self.visible(Origin::Tool(call.name.clone()), trust, Event::ToolResulted { call: call.clone(), result });
    }

    fn end_turn(&mut self, outcome: TurnOutcome) {
        self.user(Event::TurnEnded { outcome: outcome.clone() });
        self.issue(Effect::Finish(outcome));
    }

    /// Close every recorded tool_use without a result, then end the turn.
    fn fail_turn(&mut self, error: String) {
        self.close_open(ToolResult::cancelled);
        self.end_turn(TurnOutcome::Failed { error });
    }

    fn close_open(&mut self, mk: impl Fn(CallId) -> ToolResult) {
        let open: Vec<ToolCall> = match &self.s.turn {
            Some(t) => t.slots.iter().filter(|sl| sl.in_reply && !sl.result).map(|sl| sl.call.clone()).collect(),
            None => vec![],
        };
        for c in open {
            self.tool_resulted(&c, mk(c.id.clone()), false);
        }
    }

    fn suspend(&mut self, reason: String, question: Option<Question>) {
        self.user(Event::Suspended { reason });
        self.end_turn(TurnOutcome::Suspended { question });
    }

    fn idle(&self) -> bool {
        self.s.turn.is_none() && self.s.restoring.is_none()
    }

    fn suspended(&self) -> bool {
        self.s.turn.as_ref().map(|t| t.suspended).unwrap_or(false)
    }

    fn pending(&mut self, sig: Signal) {
        let origin = match &sig {
            Signal::Submit { .. } | Signal::Queue { .. } | Signal::Steer { .. } => Origin::User,
            Signal::Notify { source, .. } | Signal::Wake { source, .. } => Origin::Session(source.clone()),
            Signal::Silent { .. } => Origin::System,
        };
        let data = serde_json::to_value(&sig).unwrap_or_default();
        self.emit(Draft {
            origin,
            ..user_draft(Event::Plugin { kind: PENDING_SIGNAL_KIND.into(), ignorable: true, data })
        });
    }

    fn start_turn(&mut self, cause: TurnCause, item: QueueItem) {
        self.user(Event::TurnStarted { cause });
        match item {
            QueueItem::User { text, attachments } => {
                self.visible(Origin::User, Trust::User, Event::UserMessage { text, attachments })
            }
            QueueItem::Wake { source, reason } => self.visible(
                Origin::Session(source.clone()),
                Trust::Guidance,
                Event::Injected { source: format!("wake:{source}"), text: reason },
            ),
        }
    }

    // ------------------------------------------------------------ signals

    fn signal(&mut self, sig: Signal) -> Result<(), Rejection> {
        match sig {
            Signal::Submit { text, attachments } => {
                if self.idle() {
                    self.start_turn(TurnCause::User, QueueItem::User { text, attachments });
                } else if self.suspended() {
                    self.close_open(|id| ToolResult::denied(id, "not approved: a new user message superseded it"));
                    self.start_turn(TurnCause::User, QueueItem::User { text, attachments });
                } else {
                    self.pending(Signal::Submit { text, attachments });
                }
            }
            Signal::Queue { text } | Signal::Steer { text } if self.idle() => {
                self.start_turn(TurnCause::User, QueueItem::User { text, attachments: vec![] });
            }
            Signal::Wake { source, reason } if self.idle() => {
                if self.s.continuations >= self.s.max_continuations() {
                    return Err(Rejection::new("continuation budget exhausted"));
                }
                self.start_turn(TurnCause::Wake, QueueItem::Wake { source, reason });
            }
            other => self.pending(other),
        }
        Ok(())
    }

    // ------------------------------------------------------------ controls

    fn control(&mut self, c: Control) -> Result<(), Rejection> {
        match c {
            Control::SoftInterrupt => {
                if let Some(t) = &self.s.turn {
                    if !t.suspended && !t.soft {
                        let epoch = self.s.epoch;
                        self.user(Event::Interrupted { hard: false, epoch });
                    }
                }
            }
            Control::HardInterrupt => {
                if self.s.restoring.is_some() {
                    return Ok(());
                }
                let epoch = self.s.epoch + 1;
                match &self.s.turn {
                    None => {
                        if !self.s.issued.is_empty() {
                            self.user(Event::Interrupted { hard: true, epoch });
                        }
                    }
                    Some(_) => {
                        self.user(Event::Interrupted { hard: true, epoch });
                        self.close_open(ToolResult::cancelled);
                        self.end_turn(TurnOutcome::Interrupted);
                    }
                }
            }
            Control::Pause => {
                if !self.s.paused {
                    self.user(Event::Paused);
                }
            }
            Control::Resume => {
                if self.s.paused {
                    let held: Vec<(EffectId, Effect)> = self
                        .s
                        .held
                        .iter()
                        .filter_map(|id| self.s.issued.get(id).map(|e| (*id, (**e).clone())))
                        .collect();
                    self.user(Event::Resumed);
                    self.out.effects.extend(held);
                } else if self.suspended() {
                    self.user(Event::TurnStarted { cause: TurnCause::Continuation });
                }
            }
            Control::Answer { question, answer, responder } => self.answer(question, answer, responder)?,
            Control::Rewind { to } => {
                if self.s.turn.is_some() || self.s.restoring.is_some() {
                    return Err(Rejection::new("rewind requires an idle session"));
                }
                let seq = self.s.index.get(&to).ok_or_else(|| Rejection::new("unknown rewind target"))?;
                if self.s.is_abandoned(seq) {
                    return Err(Rejection::new("rewind target is not on the current branch"));
                }
                if !context::well_paired(&project(&self.s, Some(seq))) {
                    return Err(Rejection::new("rewind target is inside a tool step; pick a turn boundary"));
                }
                let checkpoint = self
                    .s
                    .checkpoints
                    .iter()
                    .rev()
                    .find(|(q, _)| *q <= seq && !self.s.is_abandoned(*q))
                    .map(|(_, id)| id.clone());
                let plan = RestorePlan { to, checkpoint };
                self.user(Event::RewindPlanned { plan: plan.clone() });
                self.issue(Effect::Restore(plan));
            }
            Control::SwitchModel { model } => {
                let cur = self.s.caps.clone().unwrap_or_default();
                if cur.model != model {
                    let caps = self.s.caps_for(&model).unwrap_or_else(|| ModelCaps { model: model.clone(), ..cur });
                    self.switch_to(caps, "requested".into());
                }
            }
            Control::ClearTaint => self.user(Event::TaintCleared),
            Control::Reconfigure { config } => {
                if self.idle() {
                    self.apply_config(*config);
                } else {
                    let data = serde_json::to_value(&*config).unwrap_or_default();
                    self.user(Event::Plugin { kind: PENDING_CONFIG_KIND.into(), ignorable: true, data });
                }
            }
        }
        Ok(())
    }

    fn answer(&mut self, question: QuestionId, answer: Answer, responder: String) -> Result<(), Rejection> {
        let pq = self.s.questions.get(&question).cloned().ok_or_else(|| Rejection::new("no such pending question"))?;
        let responder = if responder == "code" { Responder::Code } else { Responder::Human(responder) };
        if pq.question.level == ApprovalLevel::Invariant
            && self.cfg().unattended.is_some()
            && responder == Responder::Code
        {
            return Err(Rejection::new("invariant-level approvals cannot be answered by code in unattended mode"));
        }
        let verdict = match &answer {
            Answer::Allow { .. } => Verdict::Allow,
            Answer::AllowWith(p) => Verdict::Rewrite(p.clone()),
            Answer::Deny { reason } => Verdict::deny(reason.clone().unwrap_or_else(|| "denied by the user".into())),
        };
        if self.suspended() {
            self.user(Event::TurnStarted { cause: TurnCause::Continuation });
        }
        self.user(Event::QuestionAnswered { question: question.clone(), answer: answer.clone(), responder: responder.clone() });
        if let Some(g) = pq.gate {
            if self.s.issued.contains_key(&g) {
                self.settle(g);
            }
        }
        if let (Answer::Allow { remember: true }, Some(d)) = (&answer, &pq.question.remember_destination) {
            self.user(Event::DestinationAllowed { destination: d.clone() });
        }
        self.internal(Event::VerdictRecorded {
            subject: pq.subject.clone(),
            point: pq.point,
            ring: Ring::Human,
            verdict: verdict.clone(),
            responder,
        });
        if !matches!(pq.subject, GateRef::Call(_)) {
            self.after_turn_verdict(pq.point, Ring::Human, &verdict);
        }
        Ok(())
    }

    // ------------------------------------------------------------ sequences / config

    fn open_sequence(&mut self, rerender: bool) {
        let cfg = self.cfg().clone();
        let caps = self.s.caps.clone().unwrap_or_else(|| cfg.caps.clone());
        let seq_no = self.s.head.as_ref().map(|h| h.seq_no + 1).unwrap_or(0);
        let head = SeqHead {
            seq_no,
            model: caps.model.clone(),
            system: cfg.system.clone(),
            tools: cfg.tools.clone(),
            render: caps.render.clone(),
            encoder_version: cfg.encoder_version,
        };
        self.internal(Event::SequenceOpened { head });
        if rerender {
            if let Some(rep) = context::rerender(&self.s) {
                self.replaced(rep);
            }
        }
    }

    fn switch_to(&mut self, caps: ModelCaps, reason: String) {
        let from = self.s.caps.as_ref().map(|c| c.model.clone()).unwrap_or_default();
        self.user(Event::ModelSwitched { from, to: caps.model.clone(), reason });
        self.open_sequence(true);
    }

    fn apply_config(&mut self, new: KernelConfig) {
        let head = self.s.head.clone();
        let hash = config_hash(&new);
        self.user(Event::ConfigChanged { profile_hash: hash, config: new.clone() });
        let Some(head) = head else {
            self.open_sequence(false);
            return;
        };
        let caps = &new.caps;
        let model_changed = caps.model != head.model || caps.render != head.render;
        let enc_changed = new.encoder_version != head.encoder_version;
        let static_changed = new.system != head.system || new.tools != head.tools;
        if model_changed || enc_changed {
            self.open_sequence(model_changed);
        } else if static_changed {
            if caps.mid_sequence_updates {
                let tools: Vec<&str> = new.tools.iter().map(|t| t.name.as_str()).collect();
                let text = format!(
                    "Configuration updated.\nSystem instructions:\n{}\nAvailable tools: {}",
                    new.system.join("\n"),
                    tools.join(", ")
                );
                self.visible(Origin::System, Trust::Guidance, Event::Injected { source: "system-update".into(), text });
            } else {
                self.open_sequence(false);
            }
        }
    }

    fn next_fallback(&self) -> Option<ModelCaps> {
        let cfg = self.cfg();
        let cur = self.s.caps.as_ref()?.model.clone();
        let chain: Vec<&ModelCaps> = std::iter::once(&cfg.caps).chain(cfg.fallbacks.iter()).collect();
        match chain.iter().position(|c| c.model == cur) {
            Some(i) => chain.get(i + 1).map(|c| (*c).clone()),
            None => cfg.fallbacks.iter().find(|c| c.model != cur).cloned(),
        }
    }

    // ------------------------------------------------------------ streaming

    fn streamed(&mut self, id: EffectId, call: ToolCall) -> bool {
        let Some(t) = &self.s.turn else { return false };
        if t.sample != Some(id) || id.epoch != self.s.epoch || t.soft || t.suspended {
            return false;
        }
        if t.slot(&call.id).is_some() || gate::side_effecting(&call) || self.s.hooked(HookPoint::PreTool) {
            return false;
        }
        let ordinal = t.calls + 1;
        let repeat = t.repeats.get(&gate::call_key(&call)).copied().unwrap_or(0) + 1;
        let kv = gate::evaluate(&self.s, self.at, &call, ordinal, repeat, 0);
        if kv.verdict != Verdict::Allow {
            return false;
        }
        self.internal(Event::VerdictRecorded {
            subject: GateRef::Call(call.id.clone()),
            point: HookPoint::PreTool,
            ring: kv.ring,
            verdict: Verdict::Allow,
            responder: kv.responder,
        });
        let grants = vec![(call.id.clone(), call.access.clone())];
        self.issue(Effect::Execute(Batch { calls: vec![call], grants }));
        true
    }

    // ------------------------------------------------------------ completions

    fn completed(&mut self, id: EffectId, r: EffectResult) -> Result<bool, Rejection> {
        if id.epoch != self.s.epoch {
            return Ok(false);
        }
        let Some(eff) = self.s.issued.get(&id).map(|e| (**e).clone()) else { return Ok(false) };
        if let EffectResult::Failed { error } = r {
            self.settle(id);
            self.on_failed(eff, error);
            return Ok(true);
        }
        match (eff, r) {
            (Effect::Sample(_), EffectResult::Sampled(message)) => {
                self.settle(id);
                self.visible(Origin::Model, Trust::Internal, Event::AssistantReplied { message, effect: id });
            }
            (Effect::Sample(_), EffectResult::SampleFailed(err)) => {
                self.settle(id);
                self.sample_failed(err);
            }
            (Effect::Execute(batch), EffectResult::Executed(results)) => {
                self.settle(id);
                self.executed(batch, results);
            }
            (Effect::Gate(req), EffectResult::Gated { verdict, responder, .. }) => {
                self.settle(id);
                self.gated(req, verdict, responder);
            }
            (Effect::Compact(job), EffectResult::Compacted { summary, trust }) => {
                self.settle(id);
                self.compacted(job, summary, trust);
            }
            (Effect::Compact(job), EffectResult::CompactFailed(err)) => {
                self.settle(id);
                if job.overflow {
                    self.fail_turn(format!("context overflow: compaction failed: {err}"));
                }
            }
            (Effect::Checkpoint(_), EffectResult::Checkpointed(info)) => {
                self.settle(id);
                self.user(Event::CheckpointTaken { info });
            }
            (Effect::Restore(plan), EffectResult::Restored(report)) => {
                self.settle(id);
                self.emit(Draft {
                    parent: Parent::Explicit(plan.to.clone()),
                    ..user_draft(Event::RewindCompleted { report, to: plan.to.clone() })
                });
                self.open_sequence(false);
            }
            (e, r) => {
                return Err(Rejection::new(format!(
                    "result {:?} does not match effect `{}`",
                    std::mem::discriminant(&r),
                    e.kind()
                )))
            }
        }
        Ok(true)
    }

    fn on_failed(&mut self, eff: Effect, error: String) {
        match eff {
            Effect::Execute(batch) => {
                for c in &batch.calls {
                    let open = self.s.turn.as_ref().and_then(|t| t.slot(&c.id)).map(|sl| !sl.result).unwrap_or(false);
                    if open {
                        self.tool_resulted(c, ToolResult::text(c.id.clone(), format!("infrastructure error: {error}"), true), false);
                    }
                }
                self.fail_turn(format!("infrastructure error: {error}"));
            }
            Effect::Gate(req) => {
                let v = match req.point.on_failure() {
                    FailureMode::Allow => Verdict::Allow,
                    FailureMode::Block | FailureMode::Human => Verdict::deny(format!("gate failed: {error}")),
                };
                self.gated(req, v, Responder::Kernel);
            }
            Effect::Sample(_) => self.fail_turn(error),
            Effect::Compact(job) => {
                if job.overflow {
                    self.fail_turn(format!("context overflow: compaction failed: {error}"));
                }
            }
            Effect::Checkpoint(_) | Effect::Restore(_) | Effect::Finish(_) => {}
        }
    }

    fn sample_failed(&mut self, err: ModelError) {
        if self.s.turn.is_none() {
            return;
        }
        match &err {
            ModelError::Overflow => {
                let trims = context::plan_trims(&self.s, true);
                let any = !trims.is_empty();
                for r in trims {
                    self.replaced(r);
                }
                if let Some(plan) = context::plan_overflow_segment(&self.s) {
                    self.issue_compact(plan, true);
                } else if !any {
                    self.fail_turn("context overflow: history cannot be shortened further".into());
                }
            }
            ModelError::Unavailable { message } => match self.next_fallback() {
                Some(next) => self.switch_to(next, format!("unavailable: {message}")),
                None => self.fail_turn(err.to_string()),
            },
            _ => self.fail_turn(err.to_string()),
        }
    }

    fn executed(&mut self, batch: Batch, results: Vec<ToolResult>) {
        let post = self.s.hooked(HookPoint::PostTool);
        for call in &batch.calls {
            let Some(t) = &self.s.turn else { return };
            let open = t.slot(&call.id).map(|sl| !sl.result).unwrap_or(false);
            if !open {
                continue;
            }
            let soft = t.soft;
            let r = results
                .iter()
                .find(|r| r.call_id == call.id)
                .cloned()
                .unwrap_or_else(|| ToolResult::text(call.id.clone(), "tool produced no result", true));
            if post && !soft {
                let req = self.gate_req(
                    HookPoint::PostTool,
                    Ring::Hook,
                    GateSubject::PostTool { call: call.clone(), result: r },
                    None,
                );
                self.issue(Effect::Gate(req));
            } else {
                self.tool_resulted(call, r, true);
            }
        }
    }

    fn check_human(&self, req: &GateRequest, verdict: Verdict, responder: &Responder) -> Verdict {
        if req.level != ApprovalLevel::Invariant || matches!(verdict, Verdict::Deny(_)) {
            return verdict;
        }
        let ok = match responder {
            Responder::Human(_) | Responder::DisposableEnv => true,
            Responder::Code => self.cfg().unattended.is_none(),
            _ => false,
        };
        if ok {
            verdict
        } else {
            Verdict::deny("invariant-level approval requires a human")
        }
    }

    fn answer_of(v: &Verdict) -> Answer {
        match v {
            Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_) => Answer::Allow { remember: false },
            Verdict::Rewrite(p) => Answer::AllowWith(p.clone()),
            Verdict::Deny(r) => Answer::Deny { reason: Some(r.0.clone()) },
            _ => Answer::Deny { reason: None },
        }
    }

    fn gated(&mut self, req: GateRequest, verdict: Verdict, responder: Responder) {
        let turn_no = self.s.turn.as_ref().map(|t| t.no).unwrap_or(0);
        let verdict = if req.ring == Ring::Human { self.check_human(&req, verdict, &responder) } else { verdict };
        if req.ring == Ring::Human {
            if let Some(q) = &req.question {
                if self.s.questions.contains_key(&q.id) {
                    self.user(Event::QuestionAnswered {
                        question: q.id.clone(),
                        answer: Self::answer_of(&verdict),
                        responder: responder.clone(),
                    });
                }
            }
        }
        match &req.subject {
            GateSubject::Tool { call } => {
                self.internal(Event::VerdictRecorded {
                    subject: GateRef::Call(call.id.clone()),
                    point: req.point,
                    ring: req.ring,
                    verdict,
                    responder,
                });
            }
            GateSubject::PostTool { call, result } => {
                self.internal(Event::VerdictRecorded {
                    subject: GateRef::Call(call.id.clone()),
                    point: HookPoint::PostTool,
                    ring: req.ring,
                    verdict: verdict.clone(),
                    responder,
                });
                let fin = match verdict {
                    Verdict::Rewrite(Proposal::Result(mut r)) => {
                        // A rewrite never launders trust.
                        r.trust = Trust::weakest(&result.trust, &r.trust);
                        r
                    }
                    _ => result.clone(),
                };
                let open = self.s.turn.as_ref().and_then(|t| t.slot(&call.id)).map(|sl| !sl.result).unwrap_or(false);
                if open {
                    self.tool_resulted(call, fin, true);
                }
            }
            GateSubject::UserSubmit { .. } | GateSubject::PreSample | GateSubject::Stop { .. } => {
                self.internal(Event::VerdictRecorded {
                    subject: GateRef::Turn(turn_no),
                    point: req.point,
                    ring: req.ring,
                    verdict: verdict.clone(),
                    responder,
                });
                self.after_turn_verdict(req.point, req.ring, &verdict);
            }
            GateSubject::SessionStart | GateSubject::PreCompact => {
                self.internal(Event::VerdictRecorded {
                    subject: GateRef::Session,
                    point: req.point,
                    ring: req.ring,
                    verdict,
                    responder,
                });
            }
        }
    }

    fn after_turn_verdict(&mut self, point: HookPoint, ring: Ring, verdict: &Verdict) {
        if self.s.turn.is_none() {
            return;
        }
        match point {
            HookPoint::UserSubmit => match verdict {
                Verdict::Deny(r) => {
                    self.visible(
                        Origin::Hook("user_submit".into()),
                        Trust::Guidance,
                        Event::Injected {
                            source: "hook:user_submit".into(),
                            text: format!("The user message above was blocked: {}", r.0),
                        },
                    );
                    self.end_turn(TurnOutcome::Failed { error: format!("blocked by UserSubmit hook: {}", r.0) });
                }
                Verdict::Rewrite(Proposal::UserText(t)) => self.visible(
                    Origin::Hook("user_submit".into()),
                    Trust::Guidance,
                    Event::Injected { source: "hook:user_submit".into(), text: t.clone() },
                ),
                _ => {}
            },
            HookPoint::PreSample => match verdict {
                Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_) => {}
                Verdict::Ask(q) if ring == Ring::Hook => {
                    let no = self.s.turn.as_ref().map(|t| t.no).unwrap_or(0);
                    let q = Question {
                        id: QuestionId(format!("q:turn{}:presample:{}", no, self.s.next_n)),
                        level: ApprovalLevel::Policy,
                        ring: Ring::Hook,
                        ..q.clone()
                    };
                    self.user(Event::QuestionAsked { question: q.clone(), subject: GateRef::Turn(no) });
                    let req = self.gate_req(HookPoint::PreSample, Ring::Human, GateSubject::PreSample, Some(q));
                    self.issue(Effect::Gate(req));
                }
                Verdict::Deny(r) => self.fail_turn(format!("blocked by PreSample gate: {}", r.0)),
                _ => self.fail_turn("PreSample gate did not allow sampling".into()),
            },
            HookPoint::Stop => {
                if let Verdict::Continue(r) | Verdict::Deny(r) = verdict {
                    let continuing = self.s.turn.as_ref().map(|t| t.reply.is_none()).unwrap_or(false);
                    if continuing {
                        self.visible(
                            Origin::Hook("stop".into()),
                            Trust::Guidance,
                            Event::Injected { source: "hook:stop".into(), text: r.0.clone() },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn issue_compact(&mut self, plan: SummaryPlan, overflow: bool) {
        let cfg = self.cfg().clone();
        let Some(head) = self.s.head.clone() else { return };
        let mut body = if overflow { plan.body } else { self.s.context.iter().map(|e| (*e.rendered).clone()).collect() };
        body.push(Rendered::text(Role::User, cfg.compaction.instruction.clone()));
        let max_tokens = self.s.caps.as_ref().map(|c| c.max_output).unwrap_or(cfg.caps.max_output);
        self.issue(Effect::Compact(CompactJob { prompt: Prompt { head, body, max_tokens }, range: plan.range, overflow }));
    }

    fn compacted(&mut self, job: CompactJob, summary: String, trust: Trust) {
        if self.s.turn.is_none() {
            return;
        }
        let entries = context::entries_in(&self.s, job.range);
        if entries.is_empty() {
            return;
        }
        let replaced_tokens: u32 = entries.iter().map(|e| e.rendered.tokens).sum();
        let mut sources: Vec<EventId> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        for e in &entries {
            if !sources.contains(&e.id) {
                sources.push(e.id.clone());
            }
            for l in &e.untrusted {
                if !labels.contains(l) {
                    labels.push(l.clone());
                }
            }
        }
        if let Trust::Untrusted { source } = &trust {
            if !labels.contains(source) {
                labels.push(source.clone());
            }
        }
        let rendered = render::summary(&RuleSet::default(), &self.profile(), &trust, &summary);
        if rendered.tokens >= replaced_tokens {
            if job.overflow {
                self.fail_turn("context overflow: summary did not shorten history".into());
            }
            return;
        }
        self.replaced(Replacement {
            kind: ReplacementKind::Summary,
            range: job.range,
            sources,
            untrusted_sources: labels,
            content: vec![rendered],
        });
        if job.overflow && context::usage(&self.s) > context::hard_limit(&self.s) {
            if let Some(plan) = context::plan_overflow_segment(&self.s) {
                self.issue_compact(plan, true);
            }
        }
    }

    // ------------------------------------------------------------ progress

    fn advance(&mut self) {
        for _ in 0..10_000 {
            if !self.step() {
                break;
            }
        }
    }

    fn step(&mut self) -> bool {
        if self.s.restoring.is_some() {
            return false;
        }
        let Some(t) = self.s.turn.clone() else { return self.idle_step() };
        if t.suspended {
            return false;
        }
        if t.soft || t.reply.as_ref().map(|r| r.interrupted).unwrap_or(false) {
            return self.wind_down(&t);
        }
        // Gate every recorded call.
        for sl in t.slots.iter().filter(|sl| sl.in_reply && !sl.result) {
            match &sl.gate {
                CallGate::Unchecked => {
                    let kv = gate::evaluate(&self.s, self.at, &sl.call, sl.ordinal, sl.repeat, sl.rewrites);
                    self.internal(Event::VerdictRecorded {
                        subject: GateRef::Call(sl.call.id.clone()),
                        point: HookPoint::PreTool,
                        ring: kv.ring,
                        verdict: kv.verdict,
                        responder: kv.responder,
                    });
                    return true;
                }
                CallGate::NeedHook(_) => {
                    let req = self.gate_req(HookPoint::PreTool, Ring::Hook, GateSubject::Tool { call: sl.call.clone() }, None);
                    self.issue(Effect::Gate(req));
                    return true;
                }
                CallGate::NeedHuman(q) => {
                    self.human_gate(&sl.call, q.clone());
                    return true;
                }
                CallGate::Closed(r) => {
                    self.tool_resulted(&sl.call, r.clone(), false);
                    return true;
                }
                _ => {}
            }
        }
        if t.sample.is_some() || t.compact.is_some() {
            return false;
        }
        if matches!(t.submit, TGate::Waiting(_)) || matches!(t.presample, TGate::Waiting(_)) || matches!(t.stop, TGate::Waiting(_)) {
            return false;
        }
        if let Some(sl) = t.slots.iter().find(|sl| !sl.result && sl.gate == CallGate::Deferred) {
            self.suspend(format!("gate deferred `{}`", sl.call.name), None);
            return true;
        }
        if t.slots.iter().any(|sl| !sl.result && matches!(sl.gate, CallGate::AwaitHook(..) | CallGate::AwaitHuman(..))) {
            return false;
        }
        let running = t.slots.iter().any(|sl| matches!(sl.exec, Exec::Running(_)) || sl.post.is_some());
        let ready: Vec<&ToolCall> = t
            .slots
            .iter()
            .filter(|sl| sl.in_reply && !sl.result && sl.gate == CallGate::Permitted && sl.exec == Exec::Idle)
            .map(|sl| &sl.call)
            .collect();
        if !ready.is_empty() {
            if running {
                return false;
            }
            let idx = sched::next_batch(&ready);
            let chosen: Vec<&ToolCall> = idx.iter().map(|&i| ready[i]).collect();
            if sched::needs_checkpoint(&chosen) {
                match t.pre_ckpt {
                    PreCkpt::Pending(_) => return false,
                    PreCkpt::None => {
                        let declared_writes = sched::batch_writes(&chosen);
                        self.issue(Effect::Checkpoint(CheckpointScope { declared_writes, safe_point: false }));
                        return true;
                    }
                    PreCkpt::Done => {}
                }
            }
            let calls: Vec<ToolCall> = chosen.iter().map(|c| (*c).clone()).collect();
            let grants = calls.iter().map(|c| (c.id.clone(), c.access.clone())).collect();
            self.issue(Effect::Execute(Batch { calls, grants }));
            return true;
        }
        if running {
            return false;
        }
        match &t.reply {
            Some(r) if !r.has_calls && !self.s.mailbox.iter().any(|m| m.steer) => self.stop_step(&t, r.text.clone()),
            _ => self.prepare_and_sample(),
        }
    }

    fn human_gate(&mut self, call: &ToolCall, q: Question) {
        let cfg = self.cfg().clone();
        let subject = GateRef::Call(call.id.clone());
        if cfg.unattended.is_some() && q.level == ApprovalLevel::Invariant {
            if cfg.security.disposable_env {
                self.internal(Event::VerdictRecorded {
                    subject,
                    point: HookPoint::Permission,
                    ring: Ring::Human,
                    verdict: Verdict::Allow,
                    responder: Responder::DisposableEnv,
                });
            } else {
                self.user(Event::QuestionAsked { question: q.clone(), subject });
                self.suspend(format!("approval required for `{}` ({})", call.name, q.rules.join(", ")), Some(q));
            }
            return;
        }
        self.user(Event::QuestionAsked { question: q.clone(), subject });
        let req = self.gate_req(HookPoint::Permission, Ring::Human, GateSubject::Tool { call: call.clone() }, Some(q));
        self.issue(Effect::Gate(req));
    }

    fn wind_down(&mut self, t: &Turn) -> bool {
        if let Some(sl) = t
            .slots
            .iter()
            .find(|sl| sl.in_reply && !sl.result && !matches!(sl.exec, Exec::Running(_)) && sl.post.is_none())
        {
            let c = sl.call.clone();
            self.tool_resulted(&c, ToolResult::cancelled(c.id.clone()), false);
            return true;
        }
        let busy = t.sample.is_some()
            || t.compact.is_some()
            || t.slots.iter().any(|sl| matches!(sl.exec, Exec::Running(_)) || sl.post.is_some());
        if busy {
            return false;
        }
        self.end_turn(TurnOutcome::Interrupted);
        true
    }

    fn stop_step(&mut self, t: &Turn, text: String) -> bool {
        if self.s.hooked(HookPoint::Stop) {
            match t.stop {
                TGate::None => {
                    let req = self.gate_req(HookPoint::Stop, Ring::Hook, GateSubject::Stop { final_text: text }, None);
                    self.issue(Effect::Gate(req));
                    return true;
                }
                TGate::Waiting(_) => return false,
                TGate::Passed => {}
            }
        }
        self.end_turn(TurnOutcome::Done { text });
        true
    }

    fn turn_budget_exhausted(&self) -> Option<String> {
        let cfg = self.cfg();
        let b = &cfg.budgets;
        let t = self.s.turn.as_ref()?;
        if b.max_tokens > 0 && self.s.tokens_used >= b.max_tokens {
            return Some("tokens".into());
        }
        if b.max_cost_micros > 0 && self.s.cost_used >= b.max_cost_micros {
            return Some("cost".into());
        }
        if b.max_turn_ms > 0 && self.at.saturating_sub(t.started_at) > b.max_turn_ms {
            return Some("turn time".into());
        }
        if b.max_calls_per_turn > 0 && t.calls >= b.max_calls_per_turn {
            return Some("tool calls per turn".into());
        }
        None
    }

    fn last_user_text(&self) -> String {
        self.s
            .context
            .iter()
            .rev()
            .find_map(|e| match e.source.as_deref() {
                Some((Event::UserMessage { text, .. }, _)) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Safe point: deliver the mailbox, append changed snapshots, checkpoint,
    /// relieve pressure, run the PreSample gate, then sample.
    fn prepare_and_sample(&mut self) -> bool {
        let (cause, submit) = match &self.s.turn {
            Some(t) => (t.cause, t.submit),
            None => return false,
        };
        if self.s.hooked(HookPoint::UserSubmit)
            && matches!(cause, TurnCause::User | TurnCause::Queued)
            && submit == TGate::None
        {
            let text = self.last_user_text();
            let req = self.gate_req(HookPoint::UserSubmit, Ring::Hook, GateSubject::UserSubmit { text }, None);
            self.issue(Effect::Gate(req));
            return true;
        }
        for m in self.s.mailbox.clone() {
            self.visible(m.origin, m.trust, Event::Injected { source: m.source, text: m.text });
        }
        self.snapshots();
        let (executed, sp_ckpt) = self.s.turn.as_ref().map(|t| (t.executed_step, t.sp_ckpt)).unwrap_or((false, true));
        if executed && !sp_ckpt {
            self.issue(Effect::Checkpoint(CheckpointScope { declared_writes: vec![], safe_point: true }));
        }
        if let Some(what) = self.turn_budget_exhausted() {
            self.end_turn(TurnOutcome::BudgetExhausted { what });
            return true;
        }
        let relief = self.s.turn.as_ref().map(|t| t.relief).unwrap_or(true);
        if !relief && context::under_pressure(&self.s) {
            for r in context::plan_trims(&self.s, false) {
                self.replaced(r);
            }
            if context::under_pressure(&self.s) {
                if let Some(plan) = context::plan_summary(&self.s) {
                    self.issue_compact(plan, false);
                    return true;
                }
            }
        }
        if self.s.hooked(HookPoint::PreSample) {
            match self.s.turn.as_ref().map(|t| t.presample) {
                Some(TGate::None) => {
                    let req = self.gate_req(HookPoint::PreSample, Ring::Hook, GateSubject::PreSample, None);
                    self.issue(Effect::Gate(req));
                    return true;
                }
                Some(TGate::Waiting(_)) => return true,
                _ => {}
            }
        }
        let prompt = self.prompt();
        match prompt {
            Some(p) => {
                self.issue(Effect::Sample(p));
            }
            None => self.fail_turn("no request sequence open".into()),
        }
        true
    }

    fn prompt(&self) -> Option<Prompt> {
        crate::current_prompt(&self.s)
    }

    fn snapshots(&mut self) {
        let rules = self.cfg().snapshots.clone();
        for (key, value) in self.s.silent.clone() {
            let last = self.s.snaps.get(&key).cloned();
            if value.is_empty() {
                if last.is_some() {
                    self.visible(Origin::System, Trust::Guidance, Event::SnapshotCleared { key });
                }
                continue;
            }
            if let Some((text, at0)) = &last {
                if text == &value {
                    continue;
                }
                if let Some(r) = rules.iter().find(|r| r.key == key) {
                    if r.min_interval_ms > 0 && self.at.saturating_sub(*at0) < r.min_interval_ms {
                        continue;
                    }
                }
            }
            self.visible(Origin::System, Trust::Guidance, Event::StateSnapshot { key, text: value });
        }
    }

    fn idle_step(&mut self) -> bool {
        if let Some(cfg) = self.s.pending_config.clone() {
            self.apply_config(cfg);
            return true;
        }
        let can_wake = self.s.continuations < self.s.max_continuations();
        let pos = self.s.queue.iter().position(|q| match q {
            QueueItem::User { .. } => true,
            QueueItem::Wake { .. } => can_wake,
        });
        if let Some(i) = pos {
            let item = self.s.queue[i].clone();
            let cause = match item {
                QueueItem::User { .. } => TurnCause::Queued,
                QueueItem::Wake { .. } => TurnCause::Wake,
            };
            self.start_turn(cause, item);
            return true;
        }
        false
    }
}
