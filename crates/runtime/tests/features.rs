//! State snapshots, remember-destination, request verification, metrics,
//! tracing spans, observer cursors and heartbeats.
mod common;

use agent_kernel::{Decider, Decision, Rejection};
use agent_proto::*;
use agent_runtime::*;
use async_trait::async_trait;
use common::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

// ---------------------------------------------------------------- snapshots

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CState {
    n: u64,
    texts: Vec<String>,
}

/// Every submit appends one event; the state records them all.
struct Counter;

impl Decider for Counter {
    type State = CState;
    fn decide(_s: &CState, _at: Timestamp, input: Input) -> Result<Decision, Rejection> {
        match input {
            Input::Signal(Signal::Submit { text, .. }) => Ok(Decision {
                events: vec![Draft::internal(Event::Plugin { kind: "t".into(), ignorable: true, data: json!(text) })],
                effects: vec![],
            }),
            _ => Err(Rejection::new("unsupported")),
        }
    }
    fn evolve(s: &mut CState, ev: &Envelope<Event>) {
        s.n += 1;
        if let Event::Plugin { data, .. } = &ev.body {
            s.texts.push(data.as_str().unwrap_or_default().to_string());
        }
    }
    fn outstanding(_s: &CState) -> Vec<(EffectId, Effect)> {
        vec![]
    }
}

fn counter_start() -> Decision {
    Decision {
        events: vec![Draft::internal(Event::Plugin { kind: "t".into(), ignorable: true, data: json!("start") })],
        effects: vec![],
    }
}

struct CountingCodec {
    inner: JsonCodec<CState>,
    encodes: AtomicUsize,
    decodes: AtomicUsize,
}

impl CountingCodec {
    fn new(version: u32) -> Arc<Self> {
        Arc::new(CountingCodec { inner: JsonCodec::new(version), encodes: AtomicUsize::new(0), decodes: AtomicUsize::new(0) })
    }
}

impl StateCodec<CState> for CountingCodec {
    fn encode(&self, s: &CState) -> Result<Vec<u8>, String> {
        self.encodes.fetch_add(1, Ordering::SeqCst);
        self.inner.encode(s)
    }
    fn decode(&self, b: &[u8]) -> Result<CState, String> {
        self.decodes.fetch_add(1, Ordering::SeqCst);
        self.inner.decode(b)
    }
}

fn counter_rt(journal: Arc<MemJournal>, codec: Arc<CountingCodec>, every: u64) -> Runtime<Counter> {
    Runtime::<Counter>::builder()
        .journal(journal)
        .state_codec(codec)
        .options(RuntimeOptions { snapshot_every: every, ..options() })
        .build()
}

async fn full_fold(j: &MemJournal, s: &SessionId) -> CState {
    let mut st = CState::default();
    for e in j.load(s, 0).await.unwrap() {
        Counter::evolve(&mut st, &e);
    }
    st
}

#[tokio::test]
async fn snapshots_are_saved_and_resume_equals_full_fold() {
    let journal = Arc::new(MemJournal::new());
    let codec = CountingCodec::new(1);
    let s = sid("snap");
    let rt = counter_rt(journal.clone(), codec.clone(), 3);
    let h = rt.create_session(s.clone(), counter_start()).await.unwrap();
    for i in 0..10 {
        h.send(submit(&format!("m{i}"))).await.unwrap();
    }
    // 11 events: snapshots after seq 3, 6 and 9.
    assert_eq!(codec.encodes.load(Ordering::SeqCst), 3);
    let (seq, _) = journal.load_snapshot(&s).await.unwrap().unwrap();
    assert_eq!(seq, 9);
    rt.close_session(&s).await;

    // Resume in a fresh runtime: snapshot + tail equals the full fold.
    let rt2 = counter_rt(journal.clone(), codec.clone(), 3);
    let h2 = rt2.resume_session(s.clone()).await.unwrap();
    assert_eq!(codec.decodes.load(Ordering::SeqCst), 1);
    assert_eq!(h2.state(), full_fold(&journal, &s).await);
    assert_eq!(h2.next_seq(), 11);
    // It keeps appending at the right seq, and snapshots continue from there.
    h2.send(submit("after")).await.unwrap();
    assert_eq!(h2.next_seq(), 12);
    assert_eq!(journal.load_snapshot(&s).await.unwrap().unwrap().0, 12);
    assert_eq!(h2.state(), full_fold(&journal, &s).await);
    rt2.close_session(&s).await;

    // A snapshot exactly at the head (no tail events) still resumes.
    let h3 = counter_rt(journal.clone(), codec.clone(), 3).resume_session(s.clone()).await.unwrap();
    assert_eq!(h3.state(), full_fold(&journal, &s).await);
    assert_eq!(h3.next_seq(), 12);
}

#[tokio::test]
async fn corrupt_or_foreign_snapshot_falls_back_to_full_fold() {
    let journal = Arc::new(MemJournal::new());
    let s = sid("corrupt");
    let rt = counter_rt(journal.clone(), CountingCodec::new(1), 2);
    let h = rt.create_session(s.clone(), counter_start()).await.unwrap();
    for i in 0..5 {
        h.send(submit(&format!("m{i}"))).await.unwrap();
    }
    rt.close_session(&s).await;
    let expected = full_fold(&journal, &s).await;

    // Garbage bytes.
    journal.save_snapshot(&s, 4, b"\x00not a snapshot".to_vec()).await.unwrap();
    let codec = CountingCodec::new(1);
    let rt2 = counter_rt(journal.clone(), codec.clone(), 0);
    let h2 = rt2.resume_session(s.clone()).await.unwrap();
    assert_eq!(codec.decodes.load(Ordering::SeqCst), 1, "the snapshot was tried");
    assert_eq!(h2.state(), expected);
    rt2.close_session(&s).await;

    // A snapshot from another state version.
    let rt3 = counter_rt(journal.clone(), CountingCodec::new(1), 1);
    let h3 = rt3.resume_session(s.clone()).await.unwrap();
    h3.send(submit("x")).await.unwrap();
    rt3.close_session(&s).await;
    let expected = full_fold(&journal, &s).await;
    let h4 = counter_rt(journal.clone(), CountingCodec::new(2), 0).resume_session(s.clone()).await.unwrap();
    assert_eq!(h4.state(), expected);

    // A snapshot claiming a seq beyond the head is ignored.
    journal.save_snapshot(&s, 1_000, JsonCodec::<CState>::new(1).encode(&CState::default()).unwrap()).await.unwrap();
    let h5 = counter_rt(journal.clone(), CountingCodec::new(1), 0).resume_session(s.clone()).await.unwrap();
    assert_eq!(h5.state(), expected);
}

// ---------------------------------------------------------------- remember

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remember_is_carried_into_gated() {
    let rt: Runtime<Toy> = Runtime::builder().options(options()).build();
    let h = rt.create_session(sid("rem"), Toy::start()).await.unwrap();
    for (q, remember) in [("q1", true), ("q2", false)] {
        let n = h.finish_count();
        h.send(notify("gate", q)).await.unwrap();
        eventually(|| h.pending_questions().iter().any(|p| p.id.0 == q)).await;
        h.answer(QuestionId(q.into()), Answer::Allow { remember }, "alice").await.unwrap();
        h.wait_finish_after(n).await.unwrap();
    }
    assert_eq!(h.state().remembers, vec![true, false]);

    // Auto rules carry it too.
    let chain = GateChain::new().rule(Arc::new(FnRule::new("net", |_r: &GateRequest, _q: &Question| {
        Some(Answer::Allow { remember: true })
    })));
    let rt: Runtime<Toy> = Runtime::builder().gates(Arc::new(chain)).options(options()).build();
    let h = rt.create_session(sid("rem2"), Toy::start()).await.unwrap();
    let n = h.finish_count();
    h.send(notify("gate", "q3")).await.unwrap();
    h.wait_finish_after(n).await.unwrap();
    let st = h.state();
    assert_eq!(st.remembers, vec![true]);
    assert_eq!(st.verdicts[0].1, Responder::AutoRule("net".into()));
}

// ---------------------------------------------------------------- request verification

struct FnRebuilder<F>(F);
impl<F> PromptRebuilder for FnRebuilder<F>
where
    F: Fn(&[Envelope<Event>], EffectId) -> Result<Prompt, String> + Send + Sync,
{
    fn rebuild(&self, events: &[Envelope<Event>], effect: EffectId) -> Result<Prompt, String> {
        (self.0)(events, effect)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_verification_passes_and_fails_loudly() {
    // Matching rebuilder: the sample runs normally. It sees the journal up to
    // (and including) the sample's EffectIssued.
    let seen = Arc::new(Mutex::new(vec![]));
    let seen2 = seen.clone();
    let good = FnRebuilder(move |evs: &[Envelope<Event>], id: EffectId| {
        let last = evs.last().unwrap();
        assert!(matches!(&last.body, Event::EffectIssued { id: i, effect: Effect::Sample(_) } if *i == id));
        seen2.lock().unwrap().push(id);
        Ok(prompt())
    });
    let model = ScriptModel::new(vec![text_reply("hi")]);
    let rt: Runtime<Toy> = Runtime::builder()
        .model(model.clone())
        .prompt_rebuilder(Arc::new(good))
        .options(RuntimeOptions { verify_requests: true, ..options() })
        .build();
    let h = rt.create_session(sid("v1"), Toy::start()).await.unwrap();
    assert_eq!(h.run(submit("go")).await.unwrap(), TurnOutcome::Done { text: "hi".into() });
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(rt.metrics().snapshot().request_mismatches, 0);

    // Diverging rebuilder: the sample fails without reaching the model.
    let bad = FnRebuilder(|_: &[Envelope<Event>], _| {
        let mut p = prompt();
        p.head.system = vec!["something else".into()];
        Ok(p)
    });
    let model = ScriptModel::new(vec![]);
    let rt: Runtime<Toy> = Runtime::builder()
        .model(model.clone())
        .prompt_rebuilder(Arc::new(bad))
        .options(RuntimeOptions { verify_requests: true, ..options() })
        .build();
    let h = rt.create_session(sid("v2"), Toy::start()).await.unwrap();
    assert_eq!(h.run(submit("go")).await.unwrap(), TurnOutcome::Done { text: "other".into() });
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    match &h.state().other[..] {
        [EffectResult::SampleFailed(ModelError::Invalid { message })] => {
            assert!(message.contains("request consistency check failed"), "{message}")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(rt.metrics().snapshot().request_mismatches, 1);

    // Turned off: the rebuilder is not consulted.
    let rt: Runtime<Toy> = Runtime::builder()
        .model(ScriptModel::new(vec![text_reply("ok")]))
        .prompt_rebuilder(Arc::new(FnRebuilder(|_: &[Envelope<Event>], _| Err("never".to_string()))))
        .options(RuntimeOptions { verify_requests: false, ..options() })
        .build();
    let h = rt.create_session(sid("v3"), Toy::start()).await.unwrap();
    assert_eq!(h.run(submit("go")).await.unwrap(), TurnOutcome::Done { text: "ok".into() });
}

// ---------------------------------------------------------------- metrics

fn env_of(seq: u64, body: Event) -> Envelope<Event> {
    Envelope {
        id: EventId(format!("e{seq}")),
        parent: None,
        seq,
        at: Timestamp(0),
        origin: Origin::System,
        trust: Trust::Internal,
        audience: Audience::Both,
        schema: EVENT_SCHEMA,
        body,
        rendered: None,
    }
}

fn question(id: &str, rules: &[&str]) -> Question {
    Question {
        id: QuestionId(id.into()),
        prompt: "?".into(),
        level: ApprovalLevel::Policy,
        ring: Ring::Human,
        rules: rules.iter().map(|r| r.to_string()).collect(),
        remember_destination: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_cover_latency_cache_tools_and_approvals() {
    let model = ScriptModel::new(vec![
        vec![
            Step::Sleep(5),
            Step::D(Delta::ToolUseStart { id: CallId::new("c1"), name: "echo".into() }),
            Step::D(Delta::ToolUseInput(r#"{"file":"a","text":"x"}"#.into())),
            Step::D(Delta::ToolUseEnd),
            Step::D(Delta::Usage(Usage { input_tokens: 100, cache_read_tokens: 300, ..Default::default() })),
            Step::D(Delta::Stop(StopReason::ToolUse)),
        ],
        vec![
            Step::D(Delta::Text("done".into())),
            Step::D(Delta::Usage(Usage { input_tokens: 100, cache_read_tokens: 500, ..Default::default() })),
            Step::D(Delta::Stop(StopReason::EndTurn)),
        ],
    ]);
    let rt: Runtime<Toy> = Runtime::builder()
        .model(model)
        .tools(ToolRegistry::new(WS).with(Echo::new()))
        .options(options())
        .build();
    let h = rt.create_session(sid("m"), Toy::start()).await.unwrap();
    h.run(submit("go")).await.unwrap();
    let m = rt.metrics().snapshot();
    assert_eq!(m.samples, 2);
    assert_eq!(m.first_token.count, 2);
    assert!(m.first_token.max_us >= 5_000, "{:?}", m.first_token);
    assert_eq!((m.input_tokens, m.cache_read_tokens), (200, 800));
    assert!((m.cache_hit_ratio - 0.8).abs() < 1e-9);
    assert_eq!(m.tool_calls.count, 1);
    assert!(m.effects_dispatched >= 3);

    // Event-derived metrics.
    let metrics = Metrics::new();
    let s = sid("x");
    let head = SeqHead {
        seq_no: 0,
        model: ModelId::new("m"),
        system: vec![],
        tools: vec![],
        render: RenderProfile::default(),
        encoder_version: 1,
    };
    let evs = vec![
        Event::SequenceOpened { head: head.clone() },
        Event::SequenceOpened { head },
        Event::Replaced(Replacement {
            kind: ReplacementKind::Trim,
            range: (0, 1),
            sources: vec![],
            untrusted_sources: vec![],
            content: vec![],
        }),
        Event::QuestionAsked { question: question("q1", &["net", "write"]), subject: GateRef::Session },
        Event::QuestionAnswered {
            question: QuestionId("q1".into()),
            answer: Answer::Allow { remember: false },
            responder: Responder::Human("a".into()),
        },
        Event::VerdictRecorded {
            subject: GateRef::Session,
            point: HookPoint::Permission,
            ring: Ring::Human,
            verdict: Verdict::Allow,
            responder: Responder::Human("a".into()),
        },
        Event::QuestionAsked { question: question("q2", &["net"]), subject: GateRef::Session },
        Event::QuestionAnswered {
            question: QuestionId("q2".into()),
            answer: Answer::Deny { reason: None },
            responder: Responder::Human("a".into()),
        },
        Event::QuestionAnswered {
            question: QuestionId("q-unknown".into()),
            answer: Answer::Allow { remember: false },
            responder: Responder::Human("a".into()),
        },
        Event::VerdictRecorded {
            subject: GateRef::Session,
            point: HookPoint::PreTool,
            ring: Ring::Hook,
            verdict: Verdict::deny("no"),
            responder: Responder::Hook("fmt".into()),
        },
        Event::VerdictRecorded {
            subject: GateRef::Session,
            point: HookPoint::PreTool,
            ring: Ring::Policy,
            verdict: Verdict::Allow,
            responder: Responder::Policy("reads".into()),
        },
    ];
    for (i, e) in evs.into_iter().enumerate() {
        metrics.observe_event(&s, &env_of(i as u64, e));
    }
    let m = metrics.snapshot();
    assert_eq!((m.sequences_opened, m.replacements), (2, 1));
    let r = |k: &str| m.rules.get(k).copied().unwrap_or_default();
    assert_eq!(r("net"), RuleStats { decisions: 2, approvals: 1 });
    assert_eq!(r("write"), RuleStats { decisions: 1, approvals: 1 });
    assert_eq!(r("(unlabeled)"), RuleStats { decisions: 1, approvals: 1 });
    assert_eq!(r("hook:fmt"), RuleStats { decisions: 1, approvals: 0 });
    assert_eq!(r("policy:reads"), RuleStats { decisions: 1, approvals: 1 });
    assert!((r("net").approval_rate() - 0.5).abs() < 1e-9);
    assert!((m.approval_rate() - 4.0 / 6.0).abs() < 1e-9);
}

#[tokio::test]
async fn checkpoint_durations_are_measured() {
    let rt: Runtime<Toy> = Runtime::builder().options(options()).build();
    let env = rt.env().clone();
    let r = agent_runtime::dispatch::checkpoint(&env, &CheckpointScope { declared_writes: vec![], safe_point: true }).await;
    assert!(matches!(r, EffectResult::Checkpointed(_)));
    assert_eq!(rt.metrics().snapshot().checkpoints.count, 1);
}

// ---------------------------------------------------------------- observer cursors

struct Rec {
    name: String,
    seqs: Mutex<Vec<Seq>>,
}

impl Rec {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Rec { name: name.into(), seqs: Mutex::new(vec![]) })
    }
    fn seqs(&self) -> Vec<Seq> {
        self.seqs.lock().unwrap().clone()
    }
}

#[async_trait]
impl Observer for Rec {
    fn name(&self) -> &str {
        &self.name
    }
    async fn on_event(&self, _s: &SessionId, ev: &Envelope<Event>) -> Result<(), String> {
        self.seqs.lock().unwrap().push(ev.seq);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observers_resume_from_persisted_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cursors.json");
    let journal = Arc::new(MemJournal::new());
    let s = sid("obs");

    let obs = Rec::new("audit");
    let rt: Runtime<Counter> = Runtime::builder()
        .journal(journal.clone())
        .observer(obs.clone())
        .observer_cursors(Arc::new(FileCursors::open(&path).unwrap()))
        .build();
    let h = rt.create_session(s.clone(), counter_start()).await.unwrap();
    h.send(submit("a")).await.unwrap();
    h.send(submit("b")).await.unwrap();
    eventually(|| obs.seqs().len() == 3).await;
    // Wait for the cursor write after the last delivery.
    for _ in 0..500 {
        if FileCursors::open(&path).unwrap().load(&s, "audit").await.unwrap() == Some(3) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let persisted: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted, json!({"obs": {"audit": 3}}));
    rt.close_session(&s).await;

    // Another process: the observer continues after its cursor.
    let obs2 = Rec::new("audit");
    let fresh = Rec::new("fresh");
    let rt2: Runtime<Counter> = Runtime::builder()
        .journal(journal.clone())
        .observer(obs2.clone())
        .observer(fresh.clone())
        .observer_cursors(Arc::new(FileCursors::open(&path).unwrap()))
        .build();
    let h2 = rt2.resume_session(s.clone()).await.unwrap();
    h2.send(submit("c")).await.unwrap();
    eventually(|| obs2.seqs() == vec![3]).await;
    // An observer with no cursor yet replays from the start.
    eventually(|| fresh.seqs() == vec![0, 1, 2, 3]).await;
    rt2.close_session(&s).await;

    // Explicit modes override the store.
    for mode in [ObserverResume::Replay, ObserverResume::Live] {
        let o = Rec::new("audit");
        let rt3: Runtime<Counter> = Runtime::builder()
            .journal(journal.clone())
            .observer(o.clone())
            .observer_cursors(Arc::new(MemCursors::new()))
            .options(RuntimeOptions { observer_resume: mode, ..options() })
            .build();
        let h3 = rt3.resume_session(s.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        h3.send(submit(&format!("{mode:?}"))).await.unwrap();
        let last = h3.next_seq() - 1;
        eventually(|| o.seqs().last() == Some(&last)).await;
        let got = o.seqs();
        let want: Vec<Seq> = if mode == ObserverResume::Replay { (0..=last).collect() } else { vec![last] };
        assert_eq!(got, want, "{mode:?}");
        rt3.close_session(&s).await;
    }
}

#[tokio::test]
async fn file_cursors_roundtrip_and_reject_corrupt_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/c.json");
    let c = FileCursors::open(&path).unwrap();
    assert_eq!(c.load(&sid("s"), "o").await.unwrap(), None);
    c.save(&sid("s"), "o", 5).await.unwrap();
    c.save(&sid("s"), "o", 3).await.unwrap(); // never moves backwards
    assert_eq!(FileCursors::open(&path).unwrap().load(&sid("s"), "o").await.unwrap(), Some(5));
    std::fs::write(&path, "{broken").unwrap();
    assert!(FileCursors::open(&path).is_err());
}

// ---------------------------------------------------------------- heartbeats

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_while_effects_are_in_flight() {
    let rt: Runtime<Toy> = Runtime::builder()
        .model(ScriptModel::new(vec![vec![Step::Hang]]))
        .options(RuntimeOptions { heartbeat_ms: 20, ..options() })
        .build();
    let h = rt.create_session(sid("hb"), Toy::start()).await.unwrap();
    let mut pulses = h.pulses();
    // Idle: no heartbeats.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(pulses.try_recv().is_err());
    h.send(submit("go")).await.unwrap();
    let mut beats = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while beats < 2 && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(Pulse::Heartbeat { seq })) = tokio::time::timeout(Duration::from_millis(500), pulses.recv()).await {
            assert_eq!(seq, h.next_seq());
            beats += 1;
        }
    }
    assert_eq!(beats, 2);
    // Nothing in flight after a hard interrupt: heartbeats stop.
    h.send(Input::Control(Control::HardInterrupt)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    while pulses.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!matches!(pulses.try_recv(), Ok(Pulse::Heartbeat { .. })));

    // Off.
    let rt: Runtime<Toy> = Runtime::builder()
        .model(ScriptModel::new(vec![vec![Step::Hang]]))
        .options(RuntimeOptions { heartbeat_ms: 0, ..options() })
        .build();
    let h = rt.create_session(sid("hb0"), Toy::start()).await.unwrap();
    let mut pulses = h.pulses();
    h.send(submit("go")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(pulses.try_recv().is_err());
}
