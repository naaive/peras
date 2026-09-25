//! Property tests: a random driver interleaves model replies, tool results, gate
//! verdicts, signals and controls; afterwards the journal is checked for the
//! execution, cache and security invariants.

// `is_multiple_of` needs a newer toolchain than the workspace MSRV.
#![allow(unknown_lints, clippy::manual_is_multiple_of)]

mod common;
use agent_kernel::*;
use agent_proto::*;
use common::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy)]
enum CallKind {
    ReadWs,
    ReadEnv,
    ReadNet,
    WriteWs,
    WritePersist,
    WriteSelf,
    Bash,
}

#[derive(Debug, Clone)]
enum Act {
    Complete(usize, u8),
    Streamed(usize),
    Steer,
    Notify(u8),
    Queue,
    Wake,
    Silent(u8, u8),
    Soft,
    Hard,
    Pause,
    Resume,
    Answer(u8, u8),
    ClearTaint,
    Submit,
    SwitchModel,
    Rewind(usize),
}

#[derive(Debug, Clone)]
struct Scenario {
    trusted: bool,
    unattended: u8,
    disposable: bool,
    hooked: u8,
    small_window: bool,
    fallbacks: bool,
    max_cont: u32,
    replies: Vec<Vec<CallKind>>,
    acts: Vec<Act>,
}

fn call_kind() -> impl Strategy<Value = CallKind> {
    prop_oneof![
        4 => Just(CallKind::ReadWs),
        1 => Just(CallKind::ReadEnv),
        2 => Just(CallKind::ReadNet),
        3 => Just(CallKind::WriteWs),
        1 => Just(CallKind::WritePersist),
        1 => Just(CallKind::WriteSelf),
        2 => Just(CallKind::Bash),
    ]
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        30 => (any::<usize>(), any::<u8>()).prop_map(|(i, v)| Act::Complete(i, v)),
        4 => any::<usize>().prop_map(Act::Streamed),
        3 => Just(Act::Steer),
        2 => any::<u8>().prop_map(Act::Notify),
        2 => Just(Act::Queue),
        2 => Just(Act::Wake),
        2 => (any::<u8>(), any::<u8>()).prop_map(|(k, v)| Act::Silent(k, v)),
        1 => Just(Act::Soft),
        1 => Just(Act::Hard),
        1 => Just(Act::Pause),
        2 => Just(Act::Resume),
        4 => (any::<u8>(), any::<u8>()).prop_map(|(i, v)| Act::Answer(i, v)),
        1 => Just(Act::ClearTaint),
        3 => Just(Act::Submit),
        1 => Just(Act::SwitchModel),
        1 => any::<usize>().prop_map(Act::Rewind),
    ]
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        any::<bool>(),
        0u8..3,
        any::<bool>(),
        any::<u8>(),
        any::<bool>(),
        any::<bool>(),
        0u32..3,
        prop::collection::vec(prop::collection::vec(call_kind(), 0..4), 1..10),
        prop::collection::vec(act(), 0..150),
    )
        .prop_map(|(trusted, unattended, disposable, hooked, small_window, fallbacks, max_cont, replies, acts)| Scenario {
            trusted,
            unattended,
            disposable,
            hooked,
            small_window,
            fallbacks,
            max_cont,
            replies,
            acts,
        })
}

fn make_call(k: CallKind, id: String) -> ToolCall {
    match k {
        CallKind::ReadWs => read_call(&id, "/ws/src/lib.rs"),
        CallKind::ReadEnv => read_call(&id, "/ws/.env"),
        CallKind::ReadNet => net_call(&id, "docs.example"),
        CallKind::WriteWs => write_call(&id, "/ws/src/lib.rs"),
        CallKind::WritePersist => write_call(&id, "/ws/.git/hooks/pre-commit"),
        CallKind::WriteSelf => write_call(&id, "/ws/.agent/hooks.toml"),
        CallKind::Bash => bash_call(&id, "make"),
    }
}

fn config_for(sc: &Scenario) -> KernelConfig {
    let mut c = cfg();
    c.security.workspace_trusted = sc.trusted;
    c.security.disposable_env = sc.disposable;
    c.unattended = match sc.unattended {
        0 => None,
        1 => Some(OnAsk::Defer),
        _ => Some(OnAsk::Deny),
    };
    let points = [HookPoint::PreTool, HookPoint::PostTool, HookPoint::Stop, HookPoint::UserSubmit, HookPoint::PreSample];
    c.hooked = points.iter().enumerate().filter(|(i, _)| sc.hooked & (1 << i) != 0 && sc.hooked & 0x80 != 0).map(|(_, p)| *p).collect();
    if sc.small_window {
        c.caps.window = 600;
        c.compaction.output_reserve = 20;
        c.compaction.keep_recent_tokens = 60;
        c.caps.render.preview_bytes = 16;
    }
    if sc.fallbacks {
        let mut fb = c.caps.clone();
        fb.model = "backup".into();
        c.fallbacks = vec![fb];
    }
    c.budgets.max_continuations = sc.max_cont;
    c.budgets.max_repeat_calls = 3;
    c
}

struct Driver {
    h: H,
    script: VecDeque<Vec<ToolCall>>,
    steers: Vec<String>,
    n: u64,
    /// Sample the script front was streamed to (its call ids are spent otherwise).
    streamed_for: Option<EffectId>,
}

impl Driver {
    fn try_input(&mut self, i: Input) {
        let _ = self.h.input(i);
    }

    fn next_reply(&mut self, text: &str) -> AssistantMessage {
        let calls = self.script.pop_front().unwrap_or_default();
        reply(text, calls)
    }

    fn result_for(&self, c: &ToolCall) -> ToolResult {
        let mut r = ok(c, &format!("output of {} {}", c.name, "o".repeat(120)));
        if c.name == "fetch" {
            r.trust = Trust::Untrusted { source: "web".into() };
        }
        r
    }

    fn complete(&mut self, idx: usize, v: u8, benign: bool) {
        if self.h.pending.is_empty() {
            return;
        }
        let i = idx % self.h.pending.len();
        let (id, eff) = self.h.pending.remove(i);
        if matches!(eff, Effect::Sample(_)) {
            if let Some(sid) = self.streamed_for {
                if sid != id {
                    self.streamed_for = None;
                    self.script.pop_front();
                }
            }
        }
        let r = match eff {
            Effect::Finish(_) => return,
            Effect::Sample(_) => {
                if benign {
                    EffectResult::Sampled(reply("done", vec![]))
                } else {
                    match v % 12 {
                        10 | 11 => {
                            if self.streamed_for == Some(id) {
                                self.streamed_for = None;
                                self.script.pop_front();
                            }
                            if v % 12 == 10 {
                                EffectResult::SampleFailed(ModelError::Overflow)
                            } else {
                                EffectResult::SampleFailed(ModelError::Unavailable { message: "x".into() })
                            }
                        }
                        _ => {
                            self.streamed_for = None;
                            EffectResult::Sampled(self.next_reply(""))
                        }
                    }
                }
            }
            Effect::Execute(b) => {
                if !benign && v % 20 == 19 {
                    EffectResult::Failed { error: "infra".into() }
                } else {
                    EffectResult::Executed(b.calls.iter().map(|c| self.result_for(c)).collect())
                }
            }
            Effect::Gate(req) => {
                let (verdict, responder) = if benign {
                    (Verdict::Allow, Responder::Human("u".into()))
                } else {
                    gate_verdict(&req, v)
                };
                EffectResult::Gated { verdict, responder, remember: false }
            }
            Effect::Compact(_) => {
                if !benign && v % 10 == 9 {
                    EffectResult::CompactFailed(ModelError::Overloaded)
                } else {
                    EffectResult::Compacted { summary: "sum".into(), trust: Trust::Internal }
                }
            }
            Effect::Checkpoint(_) => {
                self.n += 1;
                EffectResult::Checkpointed(CheckpointInfo {
                    id: CheckpointId(format!("cp{}", self.n)),
                    agent_changes: vec![],
                    external_changes: vec![],
                })
            }
            Effect::Restore(_) => EffectResult::Restored(RestoreReport::default()),
        };
        self.try_input(Input::Completed(id, r));
    }

    fn act(&mut self, a: &Act) {
        match a {
            Act::Complete(i, v) => self.complete(*i, *v, false),
            Act::Streamed(k) => {
                let sid = self.h.pending.iter().find(|(_, e)| e.kind() == "sample").map(|(id, _)| *id);
                if let (Some(sid), Some(next)) = (sid, self.script.front()) {
                    if !next.is_empty() {
                        let c = next[k % next.len()].clone();
                        if self.streamed_for.map(|s| s != sid).unwrap_or(false) {
                            return;
                        }
                        self.streamed_for = Some(sid);
                        self.try_input(Input::Streamed(sid, c));
                    }
                }
            }
            Act::Steer => {
                self.n += 1;
                let t = format!("steer-{}", self.n);
                if self.h.input(Input::Signal(Signal::Steer { text: t.clone() })).is_ok() {
                    self.steers.push(t);
                }
            }
            Act::Notify(k) => self.try_input(Input::Signal(Signal::Notify {
                source: "ci".into(),
                key: format!("k{}", k % 3),
                text: format!("note {k}"),
                untrusted: k % 2 == 0,
            })),
            Act::Queue => self.try_input(Input::Signal(Signal::Queue { text: "queued".into() })),
            Act::Wake => self.try_input(Input::Signal(Signal::Wake { source: "timer".into(), reason: "tick".into() })),
            Act::Silent(k, v) => self.try_input(Input::Signal(Signal::Silent {
                key: format!("s{}", k % 2),
                value: if v % 3 == 0 { String::new() } else { format!("v{}", v % 4) },
            })),
            Act::Soft => self.try_input(Input::Control(Control::SoftInterrupt)),
            Act::Hard => {
                // Runtime contract: deliver the partial reply first.
                if let Some(i) = self.h.pending.iter().position(|(_, e)| e.kind() == "sample") {
                    let (sid, _) = self.h.pending.remove(i);
                    if self.streamed_for.take().map(|s| s != sid).unwrap_or(false) {
                        self.script.pop_front();
                    }
                    let mut m = self.next_reply("partial");
                    m.stop = StopReason::Interrupted;
                    self.try_input(Input::Completed(sid, EffectResult::Sampled(m)));
                }
                self.try_input(Input::Control(Control::HardInterrupt));
            }
            Act::Pause => self.try_input(Input::Control(Control::Pause)),
            Act::Resume => self.try_input(Input::Control(Control::Resume)),
            Act::Answer(i, v) => {
                let qs = pending_questions(&self.h.s);
                if qs.is_empty() {
                    return;
                }
                let q = &qs[*i as usize % qs.len()];
                let answer = match v % 3 {
                    0 => Answer::Deny { reason: None },
                    _ => Answer::Allow { remember: v % 5 == 0 },
                };
                let responder = if v % 7 == 0 { "code" } else { "user" };
                self.try_input(Input::Control(Control::Answer {
                    question: q.id.clone(),
                    answer,
                    responder: responder.into(),
                }));
            }
            Act::ClearTaint => self.try_input(Input::Control(Control::ClearTaint)),
            Act::Submit => self.try_input(Input::Signal(Signal::Submit { text: "go on".into(), attachments: vec![] })),
            Act::SwitchModel => self.try_input(Input::Control(Control::SwitchModel { model: "other".into() })),
            Act::Rewind(i) => {
                if !self.h.log.is_empty() {
                    let to = self.h.log[i % self.h.log.len()].id.clone();
                    self.try_input(Input::Control(Control::Rewind { to }));
                }
            }
        }
    }

    /// Run everything to quiescence with benign answers. Panics if stuck.
    fn drain(&mut self) {
        for _ in 0..2_000 {
            self.h.pending.retain(|(_, e)| e.kind() != "finish");
            if is_paused(&self.h.s) {
                self.try_input(Input::Control(Control::Resume));
                continue;
            }
            if !self.h.pending.is_empty() {
                self.complete(0, 0, true);
                continue;
            }
            let qs = pending_questions(&self.h.s);
            if let Some(q) = qs.first() {
                self.try_input(Input::Control(Control::Answer {
                    question: q.id.clone(),
                    answer: Answer::Allow { remember: false },
                    responder: "user".into(),
                }));
                continue;
            }
            match phase(&self.h.s) {
                Phase::Idle => return,
                Phase::Suspended => {
                    self.try_input(Input::Signal(Signal::Submit { text: "flush".into(), attachments: vec![] }));
                }
                p => panic!("stuck in {p:?} with nothing pending; outstanding: {:?}", Kernel::outstanding(&self.h.s)),
            }
        }
        panic!("drain did not terminate");
    }
}

fn gate_verdict(req: &GateRequest, v: u8) -> (Verdict, Responder) {
    let rewrite_call = |c: &ToolCall| {
        let mut c2 = c.clone();
        c2.input = serde_json::json!({ "rewritten": v });
        Verdict::Rewrite(Proposal::Call(c2))
    };
    match (&req.subject, req.ring) {
        (GateSubject::Tool { call }, Ring::Human) => {
            let responder = if v % 9 == 0 { Responder::AutoRule("r".into()) } else { Responder::Human("u".into()) };
            let verdict = match v % 5 {
                0 | 1 => Verdict::Allow,
                2 => Verdict::deny("no"),
                3 => rewrite_call(call),
                _ => Verdict::Defer,
            };
            (verdict, responder)
        }
        (GateSubject::Tool { call }, _) => {
            let verdict = match v % 8 {
                0..=2 => Verdict::Allow,
                3 => Verdict::deny("hook says no"),
                4 => Verdict::ask("hook asks"),
                5 => rewrite_call(call),
                6 => Verdict::Defer,
                _ => Verdict::Annotate(agent_proto::Context { text: "note".into(), trust: Trust::Guidance }),
            };
            (verdict, Responder::Hook("h".into()))
        }
        (GateSubject::PostTool { call, .. }, _) => {
            let verdict = match v % 3 {
                0 => Verdict::Allow,
                1 => {
                    let mut r = ToolResult::text(call.id.clone(), "rewritten", false);
                    r.trust = Trust::Guidance;
                    Verdict::Rewrite(Proposal::Result(r))
                }
                _ => Verdict::Annotate(agent_proto::Context { text: "post".into(), trust: Trust::Guidance }),
            };
            (verdict, Responder::Hook("h".into()))
        }
        (GateSubject::Stop { .. }, _) => {
            let verdict = if v % 2 == 0 { Verdict::Continue("keep going".into()) } else { Verdict::Allow };
            (verdict, Responder::Hook("stop".into()))
        }
        _ => {
            let verdict = match v % 6 {
                0 => Verdict::deny("blocked"),
                1 => Verdict::ask("sure?"),
                _ => Verdict::Allow,
            };
            (verdict, Responder::Hook("h".into()))
        }
    }
}

fn run(sc: &Scenario) -> Driver {
    let c = config_for(sc);
    let mut n = 0;
    let script = sc
        .replies
        .iter()
        .map(|r| {
            r.iter()
                .map(|k| {
                    n += 1;
                    make_call(*k, format!("c{n}"))
                })
                .collect()
        })
        .collect();
    let mut d = Driver { h: H::new(c), script, steers: vec![], n: 0, streamed_for: None };
    d.try_input(Input::Signal(Signal::Submit { text: "start".into(), attachments: vec![] }));
    for a in &sc.acts {
        d.act(a);
    }
    d.drain();
    // A final turn delivers anything still in the mailbox.
    d.script.clear();
    d.try_input(Input::Signal(Signal::Submit { text: "final".into(), attachments: vec![] }));
    d.drain();
    d
}

// ------------------------------------------------------------------ checks

fn check_tool_pairs(d: &Driver) {
    // In the journal: at most one result per call id.
    let mut counts: BTreeMap<CallId, usize> = BTreeMap::new();
    for e in &d.h.log {
        if let Event::ToolResulted { result, .. } = &e.body {
            *counts.entry(result.call_id.clone()).or_default() += 1;
        }
    }
    for (id, n) in &counts {
        if *n != 1 {
            dump(d);
        }
        assert_eq!(*n, 1, "call {id} has {n} results");
    }
    // In the final (idle) context: every tool_use has exactly one later result.
    let mut open: BTreeSet<CallId> = BTreeSet::new();
    let mut seen: BTreeSet<CallId> = BTreeSet::new();
    for r in context(&d.h.s) {
        for b in &r.blocks {
            match b {
                RBlock::ToolUse { id, .. } => {
                    assert!(open.insert(id.clone()), "duplicate tool_use {id}");
                }
                RBlock::ToolResult { id, .. } => {
                    assert!(open.remove(id), "tool_result {id} without a pending tool_use");
                    assert!(seen.insert(id.clone()));
                }
                _ => {}
            }
        }
    }
    if !open.is_empty() {
        dump(d);
    }
    assert!(open.is_empty(), "tool_use without result: {open:?}");
}

fn dump(d: &Driver) {
    for e in &d.h.log {
        let detail = match &e.body {
            Event::EffectIssued { id, effect } => format!("{id} {}", effect.kind()),
            Event::EffectSettled { id } => format!("{id}"),
            Event::ToolResulted { result, .. } => format!("{}", result.call_id),
            Event::AssistantReplied { message, .. } => {
                format!("{:?} {:?}", message.stop, message.tool_calls().map(|c| c.id.0.clone()).collect::<Vec<_>>())
            }
            Event::VerdictRecorded { subject, ring, verdict, point, .. } => format!("{subject:?} {point:?} {ring:?} {verdict:?}"),
            Event::TurnStarted { cause } => format!("{cause:?}"),
            Event::TurnEnded { outcome } => format!("{outcome:?}"),
            Event::Injected { source, text } => format!("{source} {text}"),
            Event::Replaced(r) => format!("{:?} {:?}", r.kind, r.range),
            _ => String::new(),
        };
        eprintln!("{:>4} {:<18} {}", e.seq, e.body.type_name(), detail);
    }
}

fn check_steers(d: &Driver) {
    for s in &d.steers {
        let delivered = d.h.log.iter().any(|e| match &e.body {
            Event::Injected { text, .. } => text == s,
            Event::UserMessage { text, .. } => text == s,
            _ => false,
        });
        assert!(delivered, "steer {s} lost");
    }
}

fn check_continuations(d: &Driver, max: u32) {
    let mut n = 0u32;
    for e in &d.h.log {
        match &e.body {
            Event::TurnStarted { cause: TurnCause::User | TurnCause::Queued } => n = 0,
            Event::TurnStarted { cause: TurnCause::Wake } => n += 1,
            Event::Injected { source, .. } if source == "hook:stop" => n += 1,
            _ => {}
        }
        assert!(n <= max, "continuations {n} > {max}");
    }
}

#[derive(Default, Clone, Debug)]
struct Chain {
    kernel: bool,
    blocked: bool,
    need_human: bool,
    invariant: bool,
    rewrites: u32,
}

fn check_gates(d: &Driver) {
    let mut chains: BTreeMap<CallId, Chain> = BTreeMap::new();
    for e in &d.h.log {
        match &e.body {
            Event::VerdictRecorded { subject: GateRef::Call(id), point, ring, verdict, responder } => {
                if *point == HookPoint::PostTool {
                    continue;
                }
                let c = chains.entry(id.clone()).or_default();
                let reset = |c: &mut Chain| {
                    c.rewrites += 1;
                    *c = Chain { rewrites: c.rewrites, ..Chain::default() };
                };
                match ring {
                    Ring::Invariant | Ring::Policy | Ring::Budget => {
                        // A kernel verdict starts a fresh evaluation of the (current) call.
                        c.kernel = true;
                        c.need_human = false;
                        c.invariant = false;
                        match verdict {
                            Verdict::Deny(_) => c.blocked = true,
                            Verdict::Ask(q) => {
                                c.need_human = true;
                                c.invariant = q.level == ApprovalLevel::Invariant;
                            }
                            _ => {}
                        }
                    }
                    Ring::Hook => match verdict {
                        Verdict::Deny(_) | Verdict::Rewrite(Proposal::Result(_)) => c.blocked = true,
                        Verdict::Ask(_) => c.need_human = true,
                        Verdict::Rewrite(Proposal::Call(_)) => reset(c),
                        _ => {}
                    },
                    Ring::Human => match verdict {
                        Verdict::Allow => {
                            if c.invariant {
                                assert!(
                                    matches!(responder, Responder::Human(_) | Responder::DisposableEnv | Responder::Code),
                                    "invariant ask answered by {responder:?}"
                                );
                            }
                            c.need_human = false;
                        }
                        Verdict::Rewrite(Proposal::Call(_)) => reset(c),
                        Verdict::Defer => {}
                        _ => c.blocked = true,
                    },
                }
                assert!(c.rewrites <= 4, "rewrite depth not bounded");
            }
            Event::EffectIssued { effect: Effect::Execute(b), .. } => {
                for call in &b.calls {
                    let c = chains.get(&call.id).cloned().unwrap_or_default();
                    if !c.kernel || c.blocked || c.need_human {
                        dump(d);
                    }
                    assert!(c.kernel, "{} executed without kernel verdict", call.id);
                    assert!(!c.blocked, "{} executed after a deny: {c:?}", call.id);
                    assert!(!c.need_human, "{} executed with an unanswered ask", call.id);
                }
            }
            _ => {}
        }
    }
}

fn check_taint_monotone(d: &Driver) {
    let mut s = State::default();
    let mut was = false;
    for e in &d.h.log {
        Kernel::evolve(&mut s, e);
        let now = is_tainted(&s);
        if was && !now {
            assert!(matches!(e.body, Event::TaintCleared), "taint dropped by {}", e.body.type_name());
        }
        was = now;
    }
}

fn check_cache_prefix(d: &Driver) {
    let mut last: Option<(u32, Vec<String>)> = None;
    let mut replaced = false;
    for e in &d.h.log {
        match &e.body {
            Event::Replaced(_) | Event::RewindCompleted { .. } => replaced = true,
            Event::SequenceOpened { .. } => last = None,
            Event::EffectIssued { effect: Effect::Sample(p), .. } => {
                let body: Vec<String> = p.body.iter().map(|r| serde_json::to_string(r).unwrap()).collect();
                if let Some((seq, prev)) = &last {
                    if !replaced && *seq == p.head.seq_no {
                        assert!(body.len() >= prev.len(), "prompt shrank without a replacement");
                        assert_eq!(&body[..prev.len()], &prev[..], "prompt is not a prefix extension");
                    }
                }
                last = Some((p.head.seq_no, body));
                replaced = false;
            }
            _ => {}
        }
    }
}

fn check_replay(d: &Driver) {
    let r = d.h.replay();
    assert_eq!(context(&r), context(&d.h.s));
    assert_eq!(Kernel::outstanding(&r), Kernel::outstanding(&d.h.s));
    assert_eq!(phase(&r), phase(&d.h.s));
    assert_eq!(is_tainted(&r), is_tainted(&d.h.s));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("KERNEL_PROP_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(300), failure_persistence: None, .. ProptestConfig::default() })]

    #[test]
    fn execution_invariants(sc in scenario()) {
        let d = run(&sc);
        check_tool_pairs(&d);
        check_steers(&d);
        check_continuations(&d, sc.max_cont);
        check_gates(&d);
        check_taint_monotone(&d);
        check_cache_prefix(&d);
        check_replay(&d);
        if std::env::var("KERNEL_PROP_STATS").is_ok() {
            let mut kinds: BTreeSet<String> = BTreeSet::new();
            for e in &d.h.log {
                let k = match &e.body {
                    Event::EffectIssued { effect: Effect::Compact(j), .. } => format!("compact:{}", j.overflow),
                    Event::Replaced(r) => format!("replaced:{:?}", r.kind),
                    Event::TurnEnded { outcome } => format!("ended:{}", serde_json::to_value(outcome).unwrap()["kind"]),
                    b => b.type_name().to_string(),
                };
                kinds.insert(k);
            }
            eprintln!("STATS {}", kinds.into_iter().collect::<Vec<_>>().join(" "));
        }
    }
}

#[test]
fn tighten_only_property() {
    use agent_kernel::gate::tighten;
    let vs = [
        Verdict::Allow,
        Verdict::Annotate(agent_proto::Context { text: "a".into(), trust: Trust::Guidance }),
        Verdict::Rewrite(Proposal::UserText("x".into())),
        Verdict::ask("q"),
        Verdict::Defer,
        Verdict::deny("d"),
    ];
    for a in &vs {
        for b in &vs {
            let t = tighten(a, b);
            assert!(t.strictness() >= a.strictness() && t.strictness() >= b.strictness().min(a.strictness()));
            if matches!(a, Verdict::Deny(_)) {
                assert!(matches!(t, Verdict::Deny(_)));
            }
        }
    }
}
