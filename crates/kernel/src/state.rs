//! Kernel state and `evolve` (the only way state changes).

use crate::context::{self, Entry, EntryKind, Op};
use crate::gate::{self, Matchers};
use agent_proto::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub use crate::decide::decide;

/// `Event::Plugin` kind used to journal a signal that could not be acted on
/// immediately (queued user input, steer, notification, wake, silent state).
pub const PENDING_SIGNAL_KIND: &str = "kernel.pending_signal";
/// `Event::Plugin` kind used to journal a `Control::Reconfigure` received while
/// busy; it is applied at the next idle.
pub const PENDING_CONFIG_KIND: &str = "kernel.pending_config";

/// Execution phase (a projection of state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    Idle,
    Sampling,
    Compacting,
    Gated,
    Acting,
    Suspended,
    Restoring,
}

/// Session taint (derived from trust annotations; sticky until `TaintCleared`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taint {
    pub tainted: bool,
    /// Events that introduced untrusted content.
    pub sources: Vec<EventId>,
    /// Untrusted source labels seen.
    pub labels: BTreeSet<String>,
    /// Private data was read in this session.
    pub private_read: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mail {
    pub source: String,
    pub key: Option<String>,
    pub text: String,
    pub trust: Trust,
    pub origin: Origin,
    pub steer: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QueueItem {
    User { text: String, attachments: Vec<Attachment> },
    Wake { source: String, reason: String },
}

/// Gate progress of one call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallGate {
    /// Rings 1–3 still to run.
    Unchecked,
    /// Ring 4 hook to issue; carries the kernel's Ask (None = Allow).
    NeedHook(Option<Question>),
    AwaitHook(EffectId, Option<Question>),
    /// Ring 5 to ask.
    NeedHuman(Question),
    AwaitHuman(QuestionId, Option<EffectId>),
    Permitted,
    /// Answered without execution (denied / rewritten result): result to write.
    Closed(ToolResult),
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exec {
    Idle,
    Running(EffectId),
    Done,
}

#[derive(Debug, Clone)]
pub(crate) struct Slot {
    pub call: ToolCall,
    pub gate: CallGate,
    pub exec: Exec,
    pub rewrites: u32,
    /// Part of a recorded `AssistantReplied` (tool_use is in the context).
    pub in_reply: bool,
    /// `ToolResulted` written.
    pub result: bool,
    /// PostTool gate outstanding.
    pub post: Option<EffectId>,
    pub ordinal: u32,
    pub repeat: u32,
    /// Result of an early-executed call waiting for its reply to be recorded.
    pub deferred: Option<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TGate {
    None,
    Waiting(EffectId),
    Passed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreCkpt {
    None,
    Pending(EffectId),
    Done,
}

#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub has_calls: bool,
    pub interrupted: bool,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CompactInfo {
    pub id: EffectId,
}

#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub no: u64,
    pub cause: TurnCause,
    pub started_at: Timestamp,
    pub sample: Option<EffectId>,
    pub compact: Option<CompactInfo>,
    pub reply: Option<Reply>,
    pub slots: Vec<Slot>,
    pub soft: bool,
    pub suspended: bool,
    pub submit: TGate,
    pub presample: TGate,
    pub stop: TGate,
    pub pre_ckpt: PreCkpt,
    pub sp_ckpt: bool,
    pub relief: bool,
    pub executed_step: bool,
    pub calls: u32,
    pub repeats: BTreeMap<String, u32>,
}

impl Turn {
    fn new(no: u64, cause: TurnCause, at: Timestamp) -> Turn {
        Turn {
            no,
            cause,
            started_at: at,
            sample: None,
            compact: None,
            reply: None,
            slots: vec![],
            soft: false,
            suspended: false,
            submit: TGate::None,
            presample: TGate::None,
            stop: TGate::None,
            pre_ckpt: PreCkpt::None,
            sp_ckpt: false,
            relief: false,
            executed_step: false,
            calls: 0,
            repeats: BTreeMap::new(),
        }
    }

    pub fn slot_mut(&mut self, id: &CallId) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| &s.call.id == id)
    }

    pub fn slot(&self, id: &CallId) -> Option<&Slot> {
        self.slots.iter().find(|s| &s.call.id == id)
    }

    fn new_slot(&mut self, call: ToolCall, in_reply: bool) -> &mut Slot {
        self.calls += 1;
        let key = gate::call_key(&call);
        let r = self.repeats.entry(key).or_insert(0);
        *r += 1;
        let repeat = *r;
        let ordinal = self.calls;
        self.slots.push(Slot {
            call,
            gate: CallGate::Unchecked,
            exec: Exec::Idle,
            rewrites: 0,
            in_reply,
            result: false,
            post: None,
            ordinal,
            repeat,
            deferred: None,
        });
        self.slots.last_mut().unwrap()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingQuestion {
    pub question: Question,
    pub subject: GateRef,
    pub point: HookPoint,
    pub gate: Option<EffectId>,
}

/// Event id → seq, split so that cloning stays cheap: a large shared frozen part
/// plus a small recent part merged in chunks.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeqIndex {
    frozen: Arc<BTreeMap<EventId, Seq>>,
    recent: BTreeMap<EventId, Seq>,
}

impl SeqIndex {
    pub fn insert(&mut self, id: EventId, seq: Seq) {
        self.recent.insert(id, seq);
        if self.recent.len() >= 512 {
            let recent = std::mem::take(&mut self.recent);
            Arc::make_mut(&mut self.frozen).extend(recent);
        }
    }
    pub fn get(&self, id: &EventId) -> Option<Seq> {
        self.recent.get(id).or_else(|| self.frozen.get(id)).copied()
    }
}

/// At most this many introducing events are recorded per taint.
const MAX_TAINT_SOURCES: usize = 256;

/// Kernel state: a fold of the journal. Cloning is cheap (large parts are shared).
#[derive(Debug, Clone, Default)]
pub struct State {
    pub(crate) session: Option<SessionId>,
    pub(crate) profile_hash: String,
    pub(crate) config: Option<KernelConfig>,
    pub(crate) m: Arc<Matchers>,
    /// Caps of the model currently in use (may be a fallback).
    pub(crate) caps: Option<ModelCaps>,
    pub(crate) head: Option<SeqHead>,
    pub(crate) pending_config: Option<KernelConfig>,
    pub(crate) epoch: u32,
    pub(crate) next_n: u64,
    pub(crate) issued: BTreeMap<EffectId, Arc<Effect>>,
    /// Issued while paused: dispatched on resume.
    pub(crate) held: Vec<EffectId>,
    pub(crate) paused: bool,
    pub(crate) next_seq: Seq,
    pub(crate) index: SeqIndex,
    pub(crate) ops: Vec<Arc<Op>>,
    pub(crate) abandoned: Vec<(Seq, Seq)>,
    pub(crate) context: Vec<Entry>,
    pub(crate) usage_basis: Option<u32>,
    pub(crate) est_since: u32,
    pub(crate) turn: Option<Turn>,
    pub(crate) turns: u64,
    pub(crate) mailbox: Vec<Mail>,
    pub(crate) queue: Vec<QueueItem>,
    pub(crate) silent: BTreeMap<String, String>,
    pub(crate) snaps: BTreeMap<String, (String, Timestamp)>,
    pub(crate) taint: Taint,
    pub(crate) continuations: u32,
    pub(crate) tokens_used: u64,
    pub(crate) cost_used: u64,
    pub(crate) checkpoints: Arc<Vec<(Seq, CheckpointId)>>,
    pub(crate) questions: BTreeMap<QuestionId, PendingQuestion>,
    pub(crate) destinations: BTreeSet<String>,
    pub(crate) restoring: Option<(EffectId, EventId)>,
}

impl State {
    pub(crate) fn hooked(&self, p: HookPoint) -> bool {
        self.config.as_ref().map(|c| c.hooked.contains(&p)).unwrap_or(false)
    }

    pub(crate) fn is_abandoned(&self, seq: Seq) -> bool {
        self.abandoned.iter().any(|(a, b)| seq >= *a && seq <= *b)
    }

    pub(crate) fn max_continuations(&self) -> u32 {
        self.config.as_ref().map(|c| c.budgets.max_continuations).unwrap_or(0)
    }

    /// Caps for a model id among the configured chain (primary + fallbacks).
    pub(crate) fn caps_for(&self, model: &ModelId) -> Option<ModelCaps> {
        let cfg = self.config.as_ref()?;
        std::iter::once(&cfg.caps).chain(cfg.fallbacks.iter()).find(|c| &c.model == model).cloned()
    }
}

// ------------------------------------------------------------------ queries

pub fn outstanding(s: &State) -> Vec<(EffectId, Effect)> {
    s.issued.iter().map(|(k, v)| (*k, (**v).clone())).collect()
}

pub fn phase(s: &State) -> Phase {
    if s.restoring.is_some() {
        return Phase::Restoring;
    }
    let Some(t) = &s.turn else { return Phase::Idle };
    if t.suspended {
        return Phase::Suspended;
    }
    if t.compact.is_some() {
        return Phase::Compacting;
    }
    if t.sample.is_some() {
        return Phase::Sampling;
    }
    let gated = t.slots.iter().any(|sl| {
        !sl.result && matches!(sl.gate, CallGate::AwaitHook(..) | CallGate::AwaitHuman(..) | CallGate::NeedHuman(_))
    }) || matches!(t.submit, TGate::Waiting(_))
        || matches!(t.presample, TGate::Waiting(_))
        || matches!(t.stop, TGate::Waiting(_));
    if gated {
        return Phase::Gated;
    }
    if t.slots.iter().any(|sl| matches!(sl.exec, Exec::Running(_)) || sl.post.is_some()) {
        return Phase::Acting;
    }
    Phase::Sampling
}

// ------------------------------------------------------------------ evolve

fn taint(s: &mut State, id: &EventId, label: &str) {
    s.taint.tainted = true;
    if s.taint.sources.len() < MAX_TAINT_SOURCES && !s.taint.sources.contains(id) {
        s.taint.sources.push(id.clone());
    }
    s.taint.labels.insert(label.to_string());
}

fn entry_kind(ev: &Event) -> EntryKind {
    match ev {
        Event::UserMessage { .. } => EntryKind::User,
        Event::AssistantReplied { .. } => EntryKind::Assistant,
        Event::ToolResulted { .. } => EntryKind::ToolResult,
        Event::StateSnapshot { key, .. } | Event::SnapshotCleared { key } => EntryKind::Snapshot { key: key.clone() },
        _ => EntryKind::Other,
    }
}

fn make_entry(ev: &Envelope<Event>) -> Option<Entry> {
    if !ev.audience.model_visible() {
        return None;
    }
    let rendered = ev.rendered.clone()?;
    let untrusted = match &ev.trust {
        Trust::Untrusted { source } => vec![source.clone()],
        _ => vec![],
    };
    Some(Entry {
        seq: ev.seq,
        id: ev.id.clone(),
        kind: entry_kind(&ev.body),
        rendered: Arc::new(rendered),
        untrusted,
        source: Some(Arc::new((ev.body.clone(), ev.trust.clone()))),
        trimmed: false,
        at: ev.at,
    })
}

fn append(s: &mut State, e: Entry) {
    s.est_since = s.est_since.saturating_add(e.rendered.tokens);
    s.ops.push(Arc::new(Op::Append(e.clone())));
    s.context.push(e);
}

/// Project the context from the context operations, skipping abandoned branches
/// and (optionally) everything after `until`.
pub(crate) fn project(s: &State, until: Option<Seq>) -> Vec<Entry> {
    let mut ctx = Vec::new();
    for op in &s.ops {
        if s.is_abandoned(op.seq()) || until.map(|u| op.seq() > u).unwrap_or(false) {
            continue;
        }
        match &**op {
            Op::Append(e) => ctx.push(e.clone()),
            Op::Replace { id, at, rep, .. } => context::apply_replacement(&mut ctx, id, *at, rep),
        }
    }
    ctx
}

fn rebuild_context(s: &mut State) {
    s.context = project(s, None);
    s.snaps.clear();
    for e in &s.context {
        if let (EntryKind::Snapshot { key }, Some(src)) = (&e.kind, &e.source) {
            match &src.0 {
                Event::StateSnapshot { text, .. } => {
                    s.snaps.insert(key.clone(), (text.clone(), e.at));
                }
                _ => {
                    s.snaps.remove(key);
                }
            }
        }
    }
    s.usage_basis = None;
}

pub fn evolve(s: &mut State, ev: &Envelope<Event>) {
    s.next_seq = s.next_seq.max(ev.seq + 1);
    s.index.insert(ev.id.clone(), ev.seq);
    if ev.audience.model_visible() {
        if let Trust::Untrusted { source } = &ev.trust {
            taint(s, &ev.id, source);
        }
    }
    match &ev.body {
        Event::SessionStarted { session, profile_hash, config, .. } => {
            if s.config.is_some() {
                return;
            }
            s.session = Some(session.clone());
            s.profile_hash = profile_hash.clone();
            s.m = Arc::new(Matchers::compile(config));
            s.caps = Some(config.caps.clone());
            s.config = Some(config.clone());
        }
        Event::SequenceOpened { head } => {
            s.head = Some(head.clone());
            s.usage_basis = None;
        }
        Event::ConfigChanged { profile_hash, config } => {
            s.profile_hash = profile_hash.clone();
            s.m = Arc::new(Matchers::compile(config));
            s.caps = Some(config.caps.clone());
            s.config = Some(config.clone());
            s.pending_config = None;
        }
        Event::ModelSwitched { to, .. } => {
            let caps = s.caps_for(to).unwrap_or_else(|| {
                let mut c = s.caps.clone().unwrap_or_default();
                c.model = to.clone();
                c
            });
            s.caps = Some(caps);
        }
        Event::TurnStarted { cause } => on_turn_started(s, *cause, ev.at),
        Event::UserMessage { .. } => {
            if let Some(e) = make_entry(ev) {
                append(s, e);
            }
        }
        Event::Injected { source, text } => {
            if let Some(i) = s.mailbox.iter().position(|m| &m.source == source && &m.text == text) {
                s.mailbox.remove(i);
            }
            if let Some(e) = make_entry(ev) {
                append(s, e);
            }
        }
        Event::AssistantReplied { message, effect } => on_reply(s, ev, message, *effect),
        Event::ToolResulted { call, result } => {
            let id = result.call_id.clone();
            let entry = make_entry(ev);
            let mut deferred = None;
            if let Some(t) = s.turn.as_mut() {
                if let Some(sl) = t.slot_mut(&id) {
                    sl.result = true;
                    sl.exec = Exec::Done;
                    sl.post = None;
                    if !sl.in_reply {
                        sl.deferred = entry.clone();
                        deferred = Some(());
                    }
                } else if let Some(sl) = t.slot_mut(&call.id) {
                    sl.result = true;
                    sl.exec = Exec::Done;
                }
            }
            if deferred.is_none() {
                if let Some(e) = entry {
                    append(s, e);
                }
            }
        }
        Event::TurnEnded { outcome } => match outcome {
            TurnOutcome::Suspended { .. } => {
                if let Some(t) = s.turn.as_mut() {
                    t.suspended = true;
                }
            }
            _ => {
                s.turn = None;
                s.questions.clear();
                s.issued.retain(|_, e| matches!(**e, Effect::Checkpoint(_) | Effect::Restore(_)));
                let issued = &s.issued;
                s.held.retain(|id| issued.contains_key(id));
            }
        },
        Event::EffectIssued { id, effect } => on_issued(s, *id, effect),
        Event::EffectSettled { id } => {
            s.issued.remove(id);
            s.held.retain(|h| h != id);
            if let Some((rid, _)) = &s.restoring {
                if rid == id {
                    s.restoring = None;
                }
            }
            if let Some(t) = s.turn.as_mut() {
                if t.sample == Some(*id) {
                    t.sample = None;
                }
                if t.compact.as_ref().map(|c| c.id) == Some(*id) {
                    t.compact = None;
                }
                if t.pre_ckpt == PreCkpt::Pending(*id) {
                    t.pre_ckpt = PreCkpt::Done;
                }
            }
        }
        Event::VerdictRecorded { subject, point, ring, verdict, responder } => {
            on_verdict(s, subject, *point, *ring, verdict, responder)
        }
        Event::QuestionAsked { question, subject } => {
            let point = match subject {
                GateRef::Call(_) => HookPoint::Permission,
                _ => HookPoint::PreSample,
            };
            s.questions.insert(
                question.id.clone(),
                PendingQuestion { question: question.clone(), subject: subject.clone(), point, gate: None },
            );
            if let (GateRef::Call(cid), Some(t)) = (subject, s.turn.as_mut()) {
                if let Some(sl) = t.slot_mut(cid) {
                    sl.gate = CallGate::AwaitHuman(question.id.clone(), None);
                }
            }
        }
        Event::QuestionAnswered { question, .. } => {
            s.questions.remove(question);
        }
        Event::Interrupted { hard, epoch } => {
            if *hard {
                s.epoch = *epoch;
                s.issued.clear();
                s.held.clear();
                s.questions.clear();
                if let Some(t) = s.turn.as_mut() {
                    t.sample = None;
                    t.compact = None;
                    t.soft = true;
                    for sl in &mut t.slots {
                        sl.post = None;
                        if let Exec::Running(_) = sl.exec {
                            sl.exec = Exec::Idle;
                        }
                    }
                }
            } else if let Some(t) = s.turn.as_mut() {
                t.soft = true;
            }
        }
        Event::Paused => s.paused = true,
        Event::Resumed => {
            s.paused = false;
            s.held.clear();
        }
        Event::Suspended { .. } => {}
        Event::Replaced(rep) => {
            context::apply_replacement(&mut s.context, &ev.id, ev.at, rep);
            s.ops.push(Arc::new(Op::Replace { seq: ev.seq, id: ev.id.clone(), at: ev.at, rep: Arc::new(rep.clone()) }));
            s.usage_basis = None;
            for l in &rep.untrusted_sources {
                taint(s, &ev.id, l);
            }
            if let Some(t) = s.turn.as_mut() {
                t.relief = true;
            }
        }
        Event::StateSnapshot { key, text } => {
            s.snaps.insert(key.clone(), (text.clone(), ev.at));
            if let Some(e) = make_entry(ev) {
                append(s, e);
            }
        }
        Event::SnapshotCleared { key } => {
            s.snaps.remove(key);
            if let Some(e) = make_entry(ev) {
                append(s, e);
            }
        }
        Event::InstructionsInjected { .. } | Event::MemoryLoaded { .. } => {
            if let Some(e) = make_entry(ev) {
                append(s, e);
            }
        }
        Event::TaintCleared => {
            let private = s.taint.private_read;
            s.taint = Taint { private_read: private, ..Taint::default() };
        }
        Event::DestinationAllowed { destination } => {
            s.destinations.insert(destination.clone());
        }
        Event::CheckpointTaken { info } => Arc::make_mut(&mut s.checkpoints).push((ev.seq, info.id.clone())),
        Event::RewindPlanned { .. } => {}
        Event::RewindCompleted { to, .. } => {
            s.restoring = None;
            if let Some(t) = s.index.get(to) {
                if ev.seq > t + 1 {
                    s.abandoned.push((t + 1, ev.seq - 1));
                }
                rebuild_context(s);
            }
        }
        Event::Plugin { kind, data, .. } => {
            if kind == PENDING_SIGNAL_KIND {
                if let Ok(sig) = serde_json::from_value::<Signal>(data.clone()) {
                    on_pending_signal(s, sig);
                }
            } else if kind == PENDING_CONFIG_KIND {
                if let Ok(cfg) = serde_json::from_value::<KernelConfig>(data.clone()) {
                    s.pending_config = Some(cfg);
                }
            }
        }
        Event::SubagentStarted { .. } | Event::SubagentFinished { .. } | Event::Tombstone { .. } => {}
    }
}

fn on_pending_signal(s: &mut State, sig: Signal) {
    match sig {
        Signal::Submit { text, attachments } => s.queue.push(QueueItem::User { text, attachments }),
        Signal::Queue { text } => s.queue.push(QueueItem::User { text, attachments: vec![] }),
        Signal::Wake { source, reason } => s.queue.push(QueueItem::Wake { source, reason }),
        Signal::Steer { text } => s.mailbox.push(Mail {
            source: "user".into(),
            key: None,
            text,
            trust: Trust::User,
            origin: Origin::User,
            steer: true,
        }),
        Signal::Notify { source, key, text, untrusted } => {
            let trust = if untrusted { Trust::Untrusted { source: source.clone() } } else { Trust::Guidance };
            if let Some(m) = s.mailbox.iter_mut().find(|m| !m.steer && m.key.as_deref() == Some(key.as_str())) {
                m.text = text;
                m.trust = Trust::weakest(&m.trust, &trust);
                m.source = source;
            } else {
                s.mailbox.push(Mail {
                    source: source.clone(),
                    key: Some(key),
                    text,
                    trust,
                    origin: Origin::System,
                    steer: false,
                });
            }
        }
        Signal::Silent { key, value } => {
            s.silent.insert(key, value);
        }
    }
}

fn on_turn_started(s: &mut State, cause: TurnCause, at: Timestamp) {
    if cause == TurnCause::Continuation {
        if let Some(t) = s.turn.as_mut() {
            if t.suspended {
                t.suspended = false;
                t.started_at = at;
                for sl in &mut t.slots {
                    if !sl.result
                        && matches!(sl.gate, CallGate::Deferred | CallGate::AwaitHuman(_, None) | CallGate::NeedHuman(_))
                    {
                        sl.gate = CallGate::Unchecked;
                    }
                }
                s.questions.retain(|_, q| q.gate.is_some());
                return;
            }
        }
    }
    s.questions.clear();
    s.turns += 1;
    s.turn = Some(Turn::new(s.turns, cause, at));
    match cause {
        TurnCause::User => s.continuations = 0,
        TurnCause::Queued => {
            s.continuations = 0;
            if let Some(i) = s.queue.iter().position(|q| matches!(q, QueueItem::User { .. })) {
                s.queue.remove(i);
            }
        }
        TurnCause::Wake => {
            s.continuations += 1;
            if let Some(i) = s.queue.iter().position(|q| matches!(q, QueueItem::Wake { .. })) {
                s.queue.remove(i);
            }
        }
        TurnCause::Continuation => s.continuations += 1,
    }
}

fn on_reply(s: &mut State, ev: &Envelope<Event>, message: &AssistantMessage, effect: EffectId) {
    let u = &message.usage;
    s.tokens_used += (u.input_tokens as u64) + (u.output_tokens as u64) + (u.cache_read_tokens as u64) + (u.cache_write_tokens as u64);
    s.cost_used += u.cost_micros;
    if let Some(e) = make_entry(ev) {
        append(s, e);
    }
    let total = u.total_context();
    s.usage_basis = if total > 0 { Some(total) } else { None };
    s.est_since = 0;
    let Some(t) = s.turn.as_mut() else { return };
    if t.sample == Some(effect) {
        t.sample = None;
    }
    let calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
    t.reply = Some(Reply {
        has_calls: !calls.is_empty(),
        interrupted: message.stop == StopReason::Interrupted,
        text: message.text(),
    });
    let mut deferred = Vec::new();
    for c in calls {
        match t.slot_mut(&c.id) {
            Some(sl) => {
                sl.in_reply = true;
                if let Some(e) = sl.deferred.take() {
                    deferred.push(e);
                }
            }
            None => {
                t.new_slot(c, true);
            }
        }
    }
    // Early calls the final reply does not contain never reach the context.
    t.slots.retain(|sl| sl.in_reply);
    for e in deferred {
        append(s, e);
    }
}

fn on_issued(s: &mut State, id: EffectId, effect: &Effect) {
    s.next_n = s.next_n.max(id.n + 1);
    if !matches!(effect, Effect::Finish(_)) {
        s.issued.insert(id, Arc::new(effect.clone()));
        if s.paused {
            s.held.push(id);
        }
    }
    match effect {
        Effect::Restore(plan) => s.restoring = Some((id, plan.to.clone())),
        Effect::Execute(batch) => {
            let private = batch.calls.iter().any(|c| gate::reads_private(s, c));
            if private {
                s.taint.private_read = true;
            }
            if let Some(t) = s.turn.as_mut() {
                t.pre_ckpt = PreCkpt::None;
                t.executed_step = true;
                for c in &batch.calls {
                    if t.slot(&c.id).is_none() {
                        t.new_slot(c.clone(), false);
                    }
                    let sl = t.slot_mut(&c.id).unwrap();
                    sl.gate = CallGate::Permitted;
                    sl.exec = Exec::Running(id);
                }
            }
        }
        _ => {}
    }
    let Some(t) = s.turn.as_mut() else { return };
    match effect {
        Effect::Sample(_) => {
            t.sample = Some(id);
            t.reply = None;
            t.slots.clear();
            t.presample = TGate::None;
            t.stop = TGate::None;
            t.relief = false;
            t.sp_ckpt = false;
            t.pre_ckpt = PreCkpt::None;
            t.executed_step = false;
        }
        Effect::Gate(req) => match (&req.subject, req.ring) {
            (GateSubject::Tool { call }, Ring::Hook) => {
                if let Some(sl) = t.slot_mut(&call.id) {
                    let prior = match &sl.gate {
                        CallGate::NeedHook(q) => q.clone(),
                        _ => None,
                    };
                    sl.gate = CallGate::AwaitHook(id, prior);
                }
            }
            (GateSubject::Tool { call }, _) => {
                if let Some(sl) = t.slot_mut(&call.id) {
                    let qid = req.question.as_ref().map(|q| q.id.clone()).unwrap_or_default();
                    sl.gate = CallGate::AwaitHuman(qid.clone(), Some(id));
                    if let Some(pq) = s.questions.get_mut(&qid) {
                        pq.gate = Some(id);
                    }
                }
            }
            (GateSubject::PostTool { call, .. }, _) => {
                if let Some(sl) = t.slot_mut(&call.id) {
                    sl.post = Some(id);
                    sl.exec = Exec::Done;
                }
            }
            (GateSubject::UserSubmit { .. }, _) => t.submit = TGate::Waiting(id),
            (GateSubject::PreSample, _) => {
                t.presample = TGate::Waiting(id);
                if let Some(q) = &req.question {
                    if let Some(pq) = s.questions.get_mut(&q.id) {
                        pq.gate = Some(id);
                    }
                }
            }
            (GateSubject::Stop { .. }, _) => t.stop = TGate::Waiting(id),
            _ => {}
        },
        Effect::Compact(job) => {
            let _ = job;
            t.compact = Some(CompactInfo { id });
            t.relief = true;
        }
        Effect::Checkpoint(scope) => {
            if scope.safe_point {
                t.sp_ckpt = true;
            } else {
                t.pre_ckpt = PreCkpt::Pending(id);
            }
        }
        _ => {}
    }
}

fn annotate(s: &mut State, source: String, ctx: &agent_proto::Context) {
    let trust = if ctx.trust.is_untrusted() { ctx.trust.clone() } else { Trust::Guidance };
    s.mailbox.push(Mail { source: source.clone(), key: None, text: ctx.text.clone(), trust, origin: Origin::Hook(source), steer: false });
}

fn responder_source(r: &Responder) -> String {
    match r {
        Responder::Hook(n) => format!("hook:{n}"),
        Responder::Human(n) => format!("human:{n}"),
        Responder::AutoRule(n) => format!("rule:{n}"),
        _ => "gate".into(),
    }
}

fn stricter(a: Option<Question>, b: Question) -> Question {
    match a {
        Some(q) if q.level >= b.level => q,
        _ => b,
    }
}

fn with_id(mut c: ToolCall, id: &CallId) -> ToolCall {
    c.id = id.clone();
    c
}

fn on_verdict(s: &mut State, subject: &GateRef, point: HookPoint, ring: Ring, verdict: &Verdict, responder: &Responder) {
    if let Verdict::Annotate(ctx) = verdict {
        annotate(s, responder_source(responder), ctx);
    }
    let hooked_pre = s.hooked(HookPoint::PreTool);
    let max_cont = s.max_continuations();
    let continuations = s.continuations;
    let Some(t) = s.turn.as_mut() else { return };
    match subject {
        GateRef::Call(cid) => {
            if point == HookPoint::PostTool {
                return;
            }
            let Some(sl) = t.slot_mut(cid) else { return };
            if sl.result {
                return;
            }
            let original = sl.call.id.clone();
            match ring {
                Ring::Invariant | Ring::Policy | Ring::Budget => {
                    sl.gate = match verdict {
                        Verdict::Deny(r) => CallGate::Closed(ToolResult::denied(original, &r.0)),
                        Verdict::Ask(q) => {
                            if hooked_pre {
                                CallGate::NeedHook(Some(q.clone()))
                            } else {
                                CallGate::NeedHuman(q.clone())
                            }
                        }
                        _ => {
                            if hooked_pre {
                                CallGate::NeedHook(None)
                            } else {
                                CallGate::Permitted
                            }
                        }
                    }
                }
                Ring::Hook => {
                    let prior = match &sl.gate {
                        CallGate::AwaitHook(_, p) | CallGate::NeedHook(p) => p.clone(),
                        _ => None,
                    };
                    sl.gate = match verdict {
                        Verdict::Deny(r) => CallGate::Closed(ToolResult::denied(original, &r.0)),
                        Verdict::Defer => CallGate::Deferred,
                        Verdict::Rewrite(Proposal::Call(c)) => {
                            sl.call = with_id(c.clone(), &original);
                            sl.rewrites += 1;
                            CallGate::Unchecked
                        }
                        Verdict::Rewrite(Proposal::Result(r)) => {
                            let mut r = r.clone();
                            r.call_id = original;
                            CallGate::Closed(r)
                        }
                        Verdict::Rewrite(Proposal::UserText(_)) => {
                            CallGate::Closed(ToolResult::denied(original, "invalid rewrite for a tool call"))
                        }
                        Verdict::Ask(q) => {
                            let mut q = q.clone();
                            q.level = ApprovalLevel::Policy;
                            q.ring = Ring::Hook;
                            q.id = gate::question_id(&original, sl.rewrites);
                            CallGate::NeedHuman(stricter(prior, q))
                        }
                        Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_) => match prior {
                            Some(q) => CallGate::NeedHuman(q),
                            None => CallGate::Permitted,
                        },
                    }
                }
                Ring::Human => {
                    sl.gate = match verdict {
                        Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_) => CallGate::Permitted,
                        Verdict::Deny(r) => CallGate::Closed(ToolResult::denied(original, &r.0)),
                        Verdict::Defer => CallGate::Deferred,
                        Verdict::Rewrite(Proposal::Call(c)) => {
                            sl.call = with_id(c.clone(), &original);
                            sl.rewrites += 1;
                            CallGate::Unchecked
                        }
                        Verdict::Rewrite(Proposal::Result(r)) => {
                            let mut r = r.clone();
                            r.call_id = original;
                            CallGate::Closed(r)
                        }
                        Verdict::Rewrite(Proposal::UserText(_)) | Verdict::Ask(_) => {
                            CallGate::Closed(ToolResult::denied(original, "not approved"))
                        }
                    }
                }
            }
        }
        GateRef::Turn(_) | GateRef::Session => match point {
            HookPoint::UserSubmit => t.submit = TGate::Passed,
            HookPoint::PreSample => {
                if matches!(verdict, Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_)) {
                    t.presample = TGate::Passed;
                }
            }
            HookPoint::Stop => {
                t.stop = TGate::Passed;
                if matches!(verdict, Verdict::Continue(_) | Verdict::Deny(_)) && continuations < max_cont {
                    t.reply = None;
                    s.continuations += 1;
                }
            }
            _ => {}
        },
    }
}
