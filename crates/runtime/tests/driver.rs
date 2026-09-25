mod common;

use agent_proto::*;
use agent_runtime::*;
use common::*;
use futures::StreamExt;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

type Rt = Runtime<Toy>;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn types(j: &MemJournal, s: &SessionId) -> Vec<String> {
    j.events(s).iter().map(|e| e.body.type_name().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_first_then_act_and_envelopes() {
    let journal = Arc::new(MemJournal::new());
    let echo = Echo::new();
    let model = ScriptModel::new(vec![vec![
        Step::D(Delta::ToolUseStart { id: CallId::new("c1"), name: "echo".into() }),
        Step::D(Delta::ToolUseInput(r#"{"file":"a","text":"hi"}"#.into())),
        Step::D(Delta::ToolUseEnd),
        Step::D(Delta::Stop(StopReason::ToolUse)),
    ]]);
    let s = sid("s1");
    // The model and the tool check the journal: the EffectIssued must be there.
    let saw_sample = Arc::new(AtomicBool::new(false));
    {
        let (j, s, flag) = (journal.clone(), s.clone(), saw_sample.clone());
        *model.on_stream.lock().unwrap() = Some(Arc::new(move || {
            let issued = j.events(&s).iter().any(|e| matches!(&e.body, Event::EffectIssued { effect: Effect::Sample(_), .. }));
            assert!(issued, "sample dispatched before its EffectIssued was logged");
            flag.store(true, Ordering::SeqCst);
        }));
    }
    let saw_exec = Arc::new(AtomicBool::new(false));
    {
        let (j, s, flag) = (journal.clone(), s.clone(), saw_exec.clone());
        *echo.check.lock().unwrap() = Some(Arc::new(move || {
            let issued = j.events(&s).iter().any(|e| matches!(&e.body, Event::EffectIssued { effect: Effect::Execute(_), .. }));
            assert!(issued, "execute dispatched before its EffectIssued was logged");
            flag.store(true, Ordering::SeqCst);
        }));
    }
    let rt: Rt = Runtime::builder()
        .journal(journal.clone())
        .model(model.clone())
        .tools(ToolRegistry::new(WS).with(echo.clone()))
        .ids(Arc::new(SeqIdGen::new()))
        .clock(Arc::new(ManualClock::new(1000)))
        .options(options())
        .build();
    let h = rt.create_session(s.clone(), Toy::start()).await.unwrap();
    let out = h.run(submit("go")).await.unwrap();
    assert_eq!(out, TurnOutcome::Done { text: "done".into() });
    assert!(saw_sample.load(Ordering::SeqCst) && saw_exec.load(Ordering::SeqCst));

    let evs = journal.events(&s);
    for (i, e) in evs.iter().enumerate() {
        assert_eq!(e.seq, i as u64);
        assert_eq!(e.at, Timestamp(1000));
        assert_eq!(e.schema, EVENT_SCHEMA);
        assert_eq!(e.id, EventId(format!("ev-{:012}", i + 1)));
        // Parent::Head chains to the previous event.
        assert_eq!(e.parent, if i == 0 { None } else { Some(evs[i - 1].id.clone()) });
    }
    assert_eq!(h.next_seq(), evs.len() as u64);
    // Rejections write nothing.
    let before = journal.len(&s);
    let r = h.send(submit("reject")).await;
    assert!(matches!(r, Err(DriverError::Rejected(_))));
    assert_eq!(journal.len(&s), before);
    // Tool results got the tool's output.
    let st = h.state();
    assert_eq!(st.results.len(), 1);
    assert_eq!(st.results[0].content, vec![ToolContent::Text { text: "hi".into() }]);
    assert_eq!(st.results[0].trust, Trust::Internal);
}

#[tokio::test]
async fn journal_seq_conflict_and_stale_lease() {
    let j = MemJournal::new();
    let s = sid("j");
    let l1 = j.acquire_lease(&s).await.unwrap();
    let mk = |seq: u64| Envelope {
        id: EventId(format!("e{seq}")),
        parent: None,
        seq,
        at: Timestamp(0),
        origin: Origin::System,
        trust: Trust::Internal,
        audience: Audience::None,
        schema: EVENT_SCHEMA,
        body: Event::Paused,
        rendered: None,
    };
    j.append(&s, l1, 0, &[mk(0), mk(1)]).await.unwrap();
    assert!(matches!(j.append(&s, l1, 1, &[mk(1)]).await, Err(StoreError::SeqConflict { expected: 1, found: 2 })));
    let l2 = j.acquire_lease(&s).await.unwrap();
    assert!(l2 > l1);
    assert!(matches!(j.append(&s, l1, 2, &[mk(2)]).await, Err(StoreError::StaleLease { .. })));
    j.append(&s, l2, 2, &[mk(2)]).await.unwrap();
    assert_eq!(j.next_seq(&s).await.unwrap(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stolen_lease_fences_the_driver() {
    let journal = Arc::new(MemJournal::new());
    let rt: Rt = Runtime::builder().journal(journal.clone()).options(options()).build();
    let s = sid("fence");
    let h = rt.create_session(s.clone(), Toy::start()).await.unwrap();
    // Another driver takes over.
    journal.acquire_lease(&s).await.unwrap();
    let before = journal.len(&s);
    assert!(matches!(h.send(notify("x", "y")).await, Err(DriverError::Fenced(_))));
    assert!(matches!(h.send(notify("x", "y")).await, Err(DriverError::Fenced(_))));
    assert_eq!(journal.len(&s), before);
    // Creating an existing session fails.
    let rt2: Rt = Runtime::builder().journal(journal.clone()).build();
    assert!(matches!(rt2.create_session(s.clone(), Toy::start()).await, Err(DriverError::AlreadyExists(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_assembly_and_streamed_tool_calls() {
    let model = ScriptModel::new(vec![vec![
        Step::D(Delta::Usage(Usage { input_tokens: 10, ..Default::default() })),
        Step::D(Delta::Thinking("hm".into())),
        Step::D(Delta::Thinking("m".into())),
        Step::D(Delta::ThinkingSignature("sig".into())),
        Step::D(Delta::Text("Hel".into())),
        Step::D(Delta::Text("lo".into())),
        Step::D(Delta::ToolUseStart { id: CallId::new("c1"), name: "echo".into() }),
        Step::D(Delta::ToolUseInput(r#"{"file":"#.into())),
        Step::D(Delta::ToolUseInput(r#""a.txt","text":"x"}"#.into())),
        Step::D(Delta::ToolUseEnd),
        Step::Sleep(30),
        Step::D(Delta::ToolUseStart { id: CallId::new("c2"), name: "nope".into() }),
        Step::D(Delta::ToolUseEnd),
        Step::D(Delta::Opaque { vendor: "v".into(), data: json!({"k":1}) }),
        Step::D(Delta::ToolUseStart { id: CallId::new("c3"), name: "echo".into() }),
        Step::D(Delta::ToolUseInput("{".into())),
        Step::D(Delta::Usage(Usage { output_tokens: 5, ..Default::default() })),
        Step::D(Delta::Stop(StopReason::ToolUse)),
    ]]);
    let reg = ToolRegistry::new(WS).with(Echo::new());
    let rt: Rt = Runtime::builder().model(model).tools(reg).options(options()).build();
    let h = rt.create_session(sid("asm"), Toy::start()).await.unwrap();
    let mut pulses = h.pulses();
    let out = h.run(submit("go")).await.unwrap();
    assert_eq!(out, TurnOutcome::Done { text: "done".into() });
    let st = h.state();
    let m = &st.replies[0];
    assert_eq!(m.usage.input_tokens, 10);
    assert_eq!(m.usage.output_tokens, 5);
    assert_eq!(m.stop, StopReason::ToolUse);
    let c1 = ToolCall {
        id: CallId::new("c1"),
        name: "echo".into(),
        input: json!({"file":"a.txt","text":"x"}),
        access: vec![Access::read(ResourceUri::fs("/ws/a.txt"))],
        class: EffectClass::Pure,
    };
    let c2 = ToolCall { id: CallId::new("c2"), name: "nope".into(), input: json!({}), access: vec![], class: EffectClass::Opaque };
    assert_eq!(
        m.content,
        vec![
            ContentBlock::Thinking { text: "hmm".into(), signature: Some("sig".into()) },
            ContentBlock::Text { text: "Hello".into() },
            ContentBlock::ToolUse(c1.clone()),
            ContentBlock::ToolUse(c2.clone()),
            ContentBlock::Opaque { vendor: "v".into(), data: json!({"k":1}) },
            // c3 was never completed: dropped.
        ]
    );
    // Streamed inputs arrived early, before the completion.
    assert_eq!(st.streamed, vec![c1, c2]);
    let i_streamed = st.inputs.iter().position(|i| i.starts_with("streamed:") && i.ends_with(":c1")).unwrap();
    let i_completed = st.inputs.iter().position(|i| i.contains("sampled:tooluse")).unwrap();
    assert!(i_streamed < i_completed, "{:?}", st.inputs);
    // Unknown tool result is an error result.
    assert!(st.results.iter().any(|r| r.call_id.0 == "c2" && r.is_error));
    // Pulses.
    let mut texts = vec![];
    let mut thinking = vec![];
    while let Ok(p) = pulses.try_recv() {
        match p {
            Pulse::TextDelta { text, .. } => texts.push(text),
            Pulse::ThinkingDelta { text, .. } => thinking.push(text),
            _ => {}
        }
    }
    assert_eq!(texts[..2], ["Hel".to_string(), "lo".to_string()]);
    assert_eq!(thinking, ["hm", "m"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sample_failure_is_reported() {
    let model = ScriptModel::new(vec![vec![Step::D(Delta::Text("x".into())), Step::Fail(ModelError::Overflow)]]);
    let rt: Rt = Runtime::builder().model(model).options(options()).build();
    let h = rt.create_session(sid("f"), Toy::start()).await.unwrap();
    h.run(submit("go")).await.unwrap();
    assert_eq!(h.state().other, vec![EffectResult::SampleFailed(ModelError::Overflow)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_execution_spill_trust_and_errors() {
    let blobs = Arc::new(MemBlobStore::new());
    let fetch = Fixed::new("fetch", EffectClass::Network, vec![Access::read(ResourceUri::net("example.com", 443))], false);
    let reg = ToolRegistry::new(WS).with(Echo::new()).with(fetch);
    let big = "0123456789".repeat(4 * 1024); // 40 KiB
    let calls = vec![
        call(&reg, "a", "echo", json!({"file":"f","text": big})),
        call(&reg, "b", "fetch", json!({})),
        call(&reg, "c", "echo", json!({"file":"f","text":"fail"})),
        call(&reg, "d", "echo", json!({"file":"f","text":"small"})),
    ];
    let rt: Rt = Runtime::builder().blobs(blobs.clone()).tools(reg).options(options()).build();
    let h = rt.create_session(sid("b"), Toy::start()).await.unwrap();
    h.run(notify("exec", &serde_json::to_string(&calls).unwrap())).await.unwrap();
    let rs = h.state().results;
    assert_eq!(rs.iter().map(|r| r.call_id.0.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
    let ToolContent::Blob { blob, preview } = &rs[0].content[0] else { panic!("{:?}", rs[0].content) };
    assert_eq!(blob.size as usize, big.len());
    assert!(preview.len() < 5 * 1024 && preview.contains("bytes omitted"));
    assert!(preview.starts_with("0123456789"));
    assert_eq!(blobs.get(blob).await.unwrap(), big.as_bytes());
    assert_eq!(rs[1].trust, Trust::Untrusted { source: "fetch".into() });
    assert!(rs[2].is_error);
    assert_eq!(rs[2].content, vec![ToolContent::Text { text: "boom".into() }]);
    assert_eq!(rs[3].content, vec![ToolContent::Text { text: "small".into() }]);
    assert_eq!(rs[3].trust, Trust::Internal);

    // Infra errors fail the whole batch.
    let reg = rt.tools().clone();
    let calls = vec![call(&reg, "e", "echo", json!({"file":"f","text":"infra"}))];
    h.run(notify("exec", &serde_json::to_string(&calls).unwrap())).await.unwrap();
    assert!(matches!(h.state().other.last(), Some(EffectResult::Failed { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_auto_rules_cas_answers_and_unattended() {
    let chain = GateChain::new().rule(Arc::new(FnRule::new("allow-all", |_, _| Some(Answer::Allow { remember: false }))));
    let rt: Rt = Runtime::builder().gates(Arc::new(chain)).options(options()).build();
    let h = rt.create_session(sid("g"), Toy::start()).await.unwrap();

    // Policy-level: the auto rule answers.
    h.run(notify("gate", "q1")).await.unwrap();
    assert_eq!(h.state().verdicts[0], (Verdict::Allow, Responder::AutoRule("allow-all".into())));
    // Late human answers lose.
    assert!(matches!(
        h.answer(QuestionId::new("q1"), Answer::Deny { reason: None }, "alice").await,
        Err(DriverError::Answer(AnswerError::AlreadyAnswered(_)))
    ));

    // Invariant-level: auto rules may not answer; a human must.
    let n = h.finish_count();
    h.send(notify("gate", "inv:q2")).await.unwrap();
    assert_eq!(h.pending_questions().iter().map(|q| q.id.0.clone()).collect::<Vec<_>>(), ["q2"]);
    let (a, b) = tokio::join!(
        h.command("k-alice", Command::Control(Control::Answer {
            question: QuestionId::new("q2"),
            answer: Answer::Deny { reason: Some("no".into()) },
            responder: "alice".into(),
        })),
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            h.answer(QuestionId::new("q2"), Answer::Allow { remember: false }, "bob").await
        }
    );
    assert!(a.is_ok());
    assert!(matches!(b, Err(DriverError::Answer(AnswerError::AlreadyAnswered(_)))));
    // Retried command key returns the original ack, not "already answered".
    let again = h
        .command("k-alice", Command::Control(Control::Answer {
            question: QuestionId::new("q2"),
            answer: Answer::Deny { reason: Some("no".into()) },
            responder: "alice".into(),
        }))
        .await;
    assert_eq!(again, a);
    h.wait_finish_after(n).await.unwrap();
    assert_eq!(h.state().verdicts[1], (Verdict::deny("no"), Responder::Human("alice".into())));
    assert!(h.pending_questions().is_empty());

    // Unattended: policy asks use OnAsk, invariant asks defer.
    let chain = GateChain::new().unattended(Some(OnAsk::Allow));
    let rt: Rt = Runtime::builder().gates(Arc::new(chain)).options(options()).build();
    let h = rt.create_session(sid("u"), Toy::start()).await.unwrap();
    h.run(notify("gate", "p")).await.unwrap();
    h.run(notify("gate", "inv:i")).await.unwrap();
    let v = h.state().verdicts;
    assert_eq!(v[0], (Verdict::Allow, Responder::Unattended));
    assert_eq!(v[1], (Verdict::Defer, Responder::Unattended));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_combine_and_fail_per_point() {
    let chain = GateChain::new()
        .hook(Arc::new(FnHook::new("note", vec![HookPoint::PreTool], |_| {
            Ok(Verdict::Annotate(Context { text: "a".into(), trust: Trust::Guidance }))
        })))
        .hook(Arc::new(FnHook::new("asker", vec![HookPoint::PreTool], |_| Ok(Verdict::ask("sure?")))))
        .hook(Arc::new(FnHook::new("broken", vec![HookPoint::PreTool], |_| Err("crash".into()))))
        .hook(Arc::new(FnHook::new("other", vec![HookPoint::Stop], |_| Ok(Verdict::deny("x")))));
    assert_eq!(chain.hooked_points(), vec![HookPoint::PreTool, HookPoint::Stop]);
    let rt: Rt = Runtime::builder().gates(Arc::new(chain)).options(options()).build();
    let h = rt.create_session(sid("h"), Toy::start()).await.unwrap();
    h.run(notify("hook", "t")).await.unwrap();
    // PreTool failures block.
    let (v, r) = h.state().verdicts[0].clone();
    assert!(matches!(&v, Verdict::Deny(reason) if reason.0.contains("broken")), "{v:?}");
    assert_eq!(r, Responder::Hook("broken".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_interrupt_submits_partial_then_interrupt() {
    let model = ScriptModel::new(vec![vec![
        Step::D(Delta::Text("partial ".into())),
        Step::D(Delta::ToolUseStart { id: CallId::new("c1"), name: "echo".into() }),
        Step::D(Delta::ToolUseInput("{".into())),
        Step::Hang,
    ]]);
    let rt: Rt = Runtime::builder().model(model).tools(ToolRegistry::new(WS).with(Echo::new())).options(options()).build();
    let h = rt.create_session(sid("hi"), Toy::start()).await.unwrap();
    let mut pulses = h.pulses();
    h.send(submit("go")).await.unwrap();
    // Wait until the text has been shown.
    loop {
        if let Pulse::TextDelta { .. } = pulses.recv().await.unwrap() {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    h.send(Input::Control(Control::HardInterrupt)).await.unwrap();
    let st = h.state();
    let n = st.inputs.len();
    assert!(st.inputs[n - 2].starts_with("completed:e0.0:sampled:interrupted"), "{:?}", st.inputs);
    assert_eq!(st.inputs[n - 1], "control:hard_interrupt");
    assert_eq!(st.replies[0].content, vec![ContentBlock::Text { text: "partial ".into() }]);
    assert_eq!(st.replies[0].stop, StopReason::Interrupted);
    assert_eq!(st.epoch, 1);
    // Nothing late sneaks in.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.state().inputs.len(), n);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_interrupt_cancels_running_tools() {
    let hang = Fixed::new("hang", EffectClass::Pure, vec![], true);
    let reg = ToolRegistry::new(WS).with(hang.clone());
    let calls = vec![call(&reg, "a", "hang", json!({}))];
    let rt: Rt = Runtime::builder().tools(reg).options(options()).build();
    let h = rt.create_session(sid("hc"), Toy::start()).await.unwrap();
    h.send(notify("exec", &serde_json::to_string(&calls).unwrap())).await.unwrap();
    eventually(|| hang.calls.load(Ordering::SeqCst) == 1).await;
    h.send(Input::Control(Control::HardInterrupt)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = h.state();
    assert_eq!(st.inputs.last().unwrap(), "control:hard_interrupt");
    assert!(st.results.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_replays_then_goes_live_without_gaps() {
    let journal = Arc::new(MemJournal::new());
    let opts = RuntimeOptions { recent_capacity: 3, ..options() };
    let rt: Rt = Runtime::builder().journal(journal.clone()).options(opts).build();
    let s = sid("sub");
    let h = rt.create_session(s.clone(), Toy::start()).await.unwrap();
    h.send(notify("a", "1")).await.unwrap();
    h.send(notify("a", "2")).await.unwrap();
    let mut sub0 = h.subscribe(0);
    let mut sub2 = h.subscribe(2);
    let total = h.next_seq() + 50;
    let writer = {
        let h = h.clone();
        tokio::spawn(async move {
            for i in 0..50 {
                h.send(notify("a", &i.to_string())).await.unwrap();
                if i % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
    };
    let mut got = vec![];
    while (got.len() as u64) < total {
        got.push(tokio::time::timeout(Duration::from_secs(5), sub0.next()).await.unwrap().unwrap().seq);
    }
    writer.await.unwrap();
    assert_eq!(got, (0..total).collect::<Vec<_>>());
    let mut got2 = vec![];
    while (got2.len() as u64) < total - 2 {
        got2.push(sub2.next().await.unwrap().seq);
    }
    assert_eq!(got2, (2..total).collect::<Vec<_>>());
    // Envelopes match the journal.
    let late: Vec<_> = h.subscribe(0).take(total as usize).collect().await;
    assert_eq!(late, journal.events(&s));
    // The stream ends when the session closes.
    let mut sub = h.subscribe(h.next_seq());
    rt.close_session(&s).await;
    assert!(tokio::time::timeout(Duration::from_secs(2), sub.next()).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_are_idempotent() {
    let journal = Arc::new(MemJournal::new());
    let rt: Rt = Runtime::builder().journal(journal.clone()).options(options()).build();
    let s = sid("idem");
    let h = rt.create_session(s.clone(), Toy::start()).await.unwrap();
    let cmd = Command::Signal(Signal::Notify { source: "c".into(), key: "k".into(), text: "t".into(), untrusted: false });
    let a = h.command("key-1", cmd.clone()).await.unwrap();
    let b = h.command("key-1", cmd.clone()).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(types(&journal, &s).len(), 2);
    h.command("key-2", cmd).await.unwrap();
    assert_eq!(types(&journal, &s).len(), 3);
}

struct Collect(std::sync::Mutex<Vec<Seq>>, bool);

#[async_trait::async_trait]
impl Observer for Collect {
    fn name(&self) -> &str {
        "collect"
    }
    async fn on_event(&self, _s: &SessionId, ev: &Envelope<Event>) -> Result<(), String> {
        self.0.lock().unwrap().push(ev.seq);
        if self.1 {
            Err("flaky".into())
        } else {
            Ok(())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observers_get_every_event_and_failures_do_not_block() {
    let ok = Arc::new(Collect(Default::default(), false));
    let bad = Arc::new(Collect(Default::default(), true));
    let rt: Rt = Runtime::builder().observer(ok.clone()).observer(bad.clone()).options(options()).build();
    let h = rt.create_session(sid("obs"), Toy::start()).await.unwrap();
    h.run(submit("go")).await.unwrap();
    let n = h.next_seq();
    eventually(|| ok.0.lock().unwrap().len() as u64 == n && bad.0.lock().unwrap().len() as u64 == n).await;
    assert_eq!(*ok.0.lock().unwrap(), (0..n).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_recovery_reruns_pure_and_reports_the_rest() {
    let journal = Arc::new(MemJournal::new());
    let s = sid("crash");
    let accesses = vec![Access::write(ResourceUri::fs("/ws/x"))];
    // First process: tools hang, then the process "crashes" mid-batch.
    {
        let reg = ToolRegistry::new(WS)
            .with(Fixed::new("pure", EffectClass::Pure, vec![Access::read(ResourceUri::fs("/ws/a"))], true))
            .with(Fixed::new("write", EffectClass::LocalWrite, accesses.clone(), true))
            .with(Fixed::new("danger", EffectClass::Opaque, vec![], true));
        let calls = vec![call(&reg, "p", "pure", json!({})), call(&reg, "w", "write", json!({})), call(&reg, "d", "danger", json!({}))];
        let rt: Rt = Runtime::builder().journal(journal.clone()).tools(reg).options(options()).build();
        let h = rt.create_session(s.clone(), Toy::start()).await.unwrap();
        h.send(notify("exec", &serde_json::to_string(&calls).unwrap())).await.unwrap();
        rt.close_session(&s).await;
    }
    // Second process.
    let pure = Fixed::new("pure", EffectClass::Pure, vec![], false);
    let write = Fixed::new("write", EffectClass::LocalWrite, vec![], false);
    let danger = Fixed::new("danger", EffectClass::Opaque, vec![], false);
    let reg = ToolRegistry::new(WS).with(pure.clone()).with(write.clone()).with(danger.clone());
    let rt: Rt = Runtime::builder().journal(journal.clone()).tools(reg).options(options()).build();
    let h = rt.resume_session(s.clone()).await.unwrap();
    // The recovered batch completes, then the toy samples (NoModel fails) and finishes.
    let n = h.finish_count();
    if n == 0 {
        h.wait_finish_after(0).await.unwrap();
    }
    let st = h.state();
    assert_eq!(st.results.len(), 3);
    assert_eq!(st.results[0].content, vec![ToolContent::Text { text: "pure ran".into() }]);
    assert_eq!(st.results[1].content, vec![ToolContent::Text { text: "write ran".into() }]);
    assert!(st.results[2].is_error);
    assert_eq!(st.results[2].content, vec![ToolContent::Text { text: agent_runtime::dispatch::NOT_RERUN.into() }]);
    assert_eq!(danger.calls.load(Ordering::SeqCst), 0);
    assert_eq!(pure.calls.load(Ordering::SeqCst), 1);
    // Resuming again after everything settled re-dispatches nothing new.
    assert!(matches!(rt.resume_session(sid("missing")).await, Err(DriverError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_registry_basics() {
    let blobs = Arc::new(MemBlobStore::new());
    let reg = TaskRegistry::new(blobs.clone());
    let a = reg.spawn("ok", None, |_| async { Ok(b"out".to_vec()) });
    let b = reg.spawn("hang", None, |c| async move {
        c.cancelled().await;
        Ok(vec![])
    });
    let c = reg.spawn("slow", Some(Duration::from_millis(20)), |_| async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(vec![])
    });
    let TaskStatus::Done { output: Some(blob) } = reg.wait(a).await.unwrap() else { panic!() };
    assert_eq!(blobs.get(&blob).await.unwrap(), b"out");
    assert!(reg.kill(b));
    assert_eq!(reg.wait(b).await.unwrap(), TaskStatus::Killed);
    assert_eq!(reg.wait(c).await.unwrap(), TaskStatus::TimedOut);
    assert_eq!(reg.list().len(), 3);
    assert!(!reg.kill(a));
}
