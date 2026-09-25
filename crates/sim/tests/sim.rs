//! Tests of the simulation harness with a toy decider (independent of the real
//! kernel's behaviour).

use agent_kernel::{Decider, Decision, Rejection};
use agent_proto::*;
use agent_sim::*;
use proptest::prelude::*;
use serde_json::json;
use std::collections::BTreeMap;

// ------------------------------------------------------------------ toy decider

/// Echo-ish agent: samples, runs each tool call as its own effect (so the
/// scheduler can interleave them), records results in call order, samples
/// again, ends on a text reply.
struct Toy;

#[derive(Default, Debug, Clone)]
struct ToyState {
    busy: bool,
    next_n: u64,
    issued: BTreeMap<EffectId, Effect>,
    context: Vec<Rendered>,
    /// Current tool batch: effect id -> (position, call, result).
    batch: BTreeMap<EffectId, (usize, ToolCall, Option<ToolResult>)>,
}

fn head() -> SeqHead {
    SeqHead {
        seq_no: 0,
        model: ModelId::new("scripted"),
        system: vec!["toy".into()],
        tools: vec![],
        render: RenderProfile::default(),
        encoder_version: 1,
    }
}

fn visible(role_origin: Origin, trust: Trust, body: Event, rendered: Rendered) -> Draft<Event> {
    Draft { parent: Parent::Head, origin: role_origin, trust, audience: Audience::Both, body, rendered: Some(rendered) }
}

fn assistant_render(m: &AssistantMessage) -> Rendered {
    let blocks = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(RBlock::Text { text: text.clone() }),
            ContentBlock::ToolUse(c) => {
                Some(RBlock::ToolUse { id: c.id.clone(), name: c.name.clone(), input: c.input.clone() })
            }
            _ => None,
        })
        .collect();
    Rendered { role: Role::Assistant, blocks, tokens: 1, supersedable: false }
}

fn result_render(r: &ToolResult) -> Rendered {
    let text = r
        .content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect::<String>();
    Rendered {
        role: Role::User,
        blocks: vec![RBlock::ToolResult { id: r.call_id.clone(), content: vec![RBlock::Text { text }], is_error: r.is_error }],
        tokens: 1,
        supersedable: false,
    }
}

impl Toy {
    fn issue(s: &ToyState, k: u64, e: Effect, d: &mut Decision) {
        let id = EffectId { epoch: 0, n: s.next_n + k };
        d.events.push(Draft::internal(Event::EffectIssued { id, effect: e.clone() }));
        d.effects.push((id, e));
    }

    fn sample(s: &ToyState, extra: &[Rendered], k: u64, d: &mut Decision) {
        let mut body = s.context.clone();
        body.extend(extra.iter().cloned());
        Self::issue(s, k, Effect::Sample(Prompt { head: head(), body, max_tokens: 100 }), d);
    }

    fn end(s: &ToyState, outcome: TurnOutcome, d: &mut Decision) {
        d.events.push(Draft::internal(Event::TurnEnded { outcome: outcome.clone() }));
        Self::issue(s, 0, Effect::Finish(outcome), d);
    }
}

impl Decider for Toy {
    type State = ToyState;

    fn decide(s: &ToyState, _at: Timestamp, input: Input) -> Result<Decision, Rejection> {
        let mut d = Decision::default();
        match input {
            Input::Signal(Signal::Submit { text, .. }) => {
                if s.busy {
                    return Err(Rejection::new("busy"));
                }
                d.events.push(Draft::internal(Event::TurnStarted { cause: TurnCause::User }));
                let r = Rendered::text(Role::User, text.clone());
                d.events.push(visible(Origin::User, Trust::User, Event::UserMessage { text, attachments: vec![] }, r.clone()));
                Self::sample(s, &[r], 0, &mut d);
            }
            Input::Control(Control::Rewind { to }) => {
                d.events.push(Draft { parent: Parent::Explicit(to.clone()), ..Draft::internal(Event::Paused) });
                d.events.push(Draft::internal(Event::Resumed));
            }
            Input::Completed(id, res) => {
                let Some(effect) = s.issued.get(&id) else { return Err(Rejection::new("unknown effect")) };
                d.events.push(Draft::internal(Event::EffectSettled { id }));
                match (effect, res) {
                    (Effect::Sample(_), EffectResult::Sampled(m)) => {
                        let r = assistant_render(&m);
                        let calls: Vec<ToolCall> = m.tool_calls().cloned().collect();
                        d.events.push(visible(
                            Origin::Model,
                            Trust::Internal,
                            Event::AssistantReplied { message: m.clone(), effect: id },
                            r,
                        ));
                        if calls.is_empty() {
                            Self::end(s, TurnOutcome::Done { text: m.text() }, &mut d);
                        } else {
                            for (k, c) in calls.into_iter().enumerate() {
                                Self::issue(s, k as u64, Effect::Execute(Batch { calls: vec![c], grants: vec![] }), &mut d);
                            }
                        }
                    }
                    (Effect::Sample(_), EffectResult::SampleFailed(e)) => {
                        Self::end(s, TurnOutcome::Failed { error: e.to_string() }, &mut d)
                    }
                    (Effect::Execute(_), EffectResult::Executed(results)) => {
                        let result = results.into_iter().next().ok_or_else(|| Rejection::new("no result"))?;
                        // Journal the partial result (not model-visible yet).
                        d.events.push(Draft::internal(Event::Plugin {
                            kind: "partial".into(),
                            ignorable: true,
                            data: json!({ "id": id, "result": result }),
                        }));
                        let mut all = s.batch.clone();
                        if let Some(slot) = all.get_mut(&id) {
                            slot.2 = Some(result);
                        }
                        if all.values().all(|(_, _, r)| r.is_some()) {
                            let mut ordered: Vec<_> = all.into_values().collect();
                            ordered.sort_by_key(|(i, _, _)| *i);
                            let mut extra = vec![];
                            for (_, call, r) in ordered {
                                let r = r.unwrap();
                                let rr = result_render(&r);
                                extra.push(rr.clone());
                                d.events.push(visible(
                                    Origin::Tool(call.name.clone()),
                                    Trust::Internal,
                                    Event::ToolResulted { call, result: r },
                                    rr,
                                ));
                            }
                            Self::sample(s, &extra, 0, &mut d);
                        }
                    }
                    (_, other) => return Err(Rejection::new(format!("unexpected {other:?}"))),
                }
            }
            _ => return Err(Rejection::new("unsupported")),
        }
        Ok(d)
    }

    fn evolve(s: &mut ToyState, ev: &Envelope<Event>) {
        if ev.audience.model_visible() {
            if let Some(r) = &ev.rendered {
                s.context.push(r.clone());
            }
        }
        match &ev.body {
            Event::TurnStarted { .. } => s.busy = true,
            Event::TurnEnded { .. } => s.busy = false,
            Event::EffectIssued { id, effect } => {
                s.next_n = s.next_n.max(id.n + 1);
                s.issued.insert(*id, effect.clone());
                if let Effect::Execute(b) = effect {
                    // positions follow issue order within the batch
                    let pos = s.batch.len();
                    s.batch.insert(*id, (pos, b.calls[0].clone(), None));
                }
                if matches!(effect, Effect::Sample(_)) {
                    s.batch.clear();
                }
            }
            Event::EffectSettled { id } => {
                s.issued.remove(id);
            }
            Event::Plugin { kind, data, .. } if kind == "partial" => {
                let id: EffectId = serde_json::from_value(data["id"].clone()).unwrap();
                let r: ToolResult = serde_json::from_value(data["result"].clone()).unwrap();
                if let Some(slot) = s.batch.get_mut(&id) {
                    slot.2 = Some(r);
                }
            }
            _ => {}
        }
    }

    fn outstanding(s: &ToyState) -> Vec<(EffectId, Effect)> {
        s.issued.iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

// ------------------------------------------------------------------ worlds

fn last_user_text(p: &Prompt) -> String {
    p.body
        .iter()
        .rev()
        .find_map(|r| match (r.role, r.blocks.first()) {
            (Role::User, Some(RBlock::Text { text })) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Deterministic world: the reply depends only on the prompt, so re-running an
/// effect after a crash gives the same result.
#[derive(Default)]
struct EchoWorld {
    resolved: usize,
}

impl World for EchoWorld {
    fn resolve(&mut self, _id: EffectId, effect: &Effect) -> Option<EffectResult> {
        self.resolved += 1;
        match effect {
            Effect::Sample(p) => {
                let has_results = p.body.iter().any(|r| matches!(r.blocks.first(), Some(RBlock::ToolResult { .. })));
                let text = last_user_text(p);
                let msg = if let (Some(n), false) = (text.strip_prefix("tools:"), has_results) {
                    let n: usize = n.trim().parse().unwrap();
                    AssistantMessage {
                        content: (0..n)
                            .map(|i| {
                                ContentBlock::ToolUse(ToolCall {
                                    id: CallId(format!("c{i}")),
                                    name: "echo".into(),
                                    input: json!({ "text": format!("r{i}") }),
                                    access: vec![],
                                    class: EffectClass::Pure,
                                })
                            })
                            .collect(),
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    }
                } else {
                    AssistantMessage {
                        content: vec![ContentBlock::Text { text: format!("echo: {text}") }],
                        stop: StopReason::EndTurn,
                        usage: Usage::default(),
                    }
                };
                Some(EffectResult::Sampled(msg))
            }
            Effect::Execute(b) => Some(EffectResult::Executed(
                b.calls
                    .iter()
                    .map(|c| ToolResult::text(c.id.clone(), c.input["text"].as_str().unwrap_or(""), false))
                    .collect(),
            )),
            _ => None,
        }
    }
}

/// World backed by a scripted model.
struct ScriptWorld {
    model: Script,
}

impl World for ScriptWorld {
    fn resolve(&mut self, _id: EffectId, effect: &Effect) -> Option<EffectResult> {
        match effect {
            Effect::Sample(p) => Some(sample_blocking(&self.model, p)),
            Effect::Execute(b) => Some(EffectResult::Executed(
                b.calls.iter().map(|c| ToolResult::text(c.id.clone(), format!("ran {}", c.name), false)).collect(),
            )),
            _ => None,
        }
    }
}

fn submit(t: &str) -> Input {
    Input::Signal(Signal::Submit { text: t.into(), attachments: vec![] })
}

// ------------------------------------------------------------------ tests

#[test]
fn simple_turn() {
    let mut sim = KernelSim::<Toy>::new().with_tick(5);
    let queued = sim.apply(submit("hi")).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(sim.pending().len(), 1);
    let mut w = EchoWorld::default();
    assert_eq!(sim.run_until_idle(&mut w), Step::Idle);
    assert_eq!(outcomes(sim.journal()), vec![TurnOutcome::Done { text: "echo: hi".into() }]);
    assert_eq!(sim.outcomes().len(), 1);
    assert_eq!(model_view(sim.journal()).len(), 2);
    // seqs are dense, ids sort, parents chain, time comes from the virtual clock
    for (i, e) in sim.journal().iter().enumerate() {
        assert_eq!(e.seq, i as u64);
        if i > 0 {
            assert!(e.id > sim.journal()[i - 1].id);
            assert_eq!(e.parent.as_ref(), Some(&sim.journal()[i - 1].id));
        }
    }
    assert_eq!(sim.journal()[0].at, Timestamp(5));
    assert!(sim.journal().last().unwrap().at > Timestamp(5));
}

#[test]
fn rejection_writes_nothing() {
    let mut sim = KernelSim::<Toy>::new();
    sim.apply(submit("a")).unwrap();
    let n = sim.journal().len();
    assert!(sim.apply(submit("b")).is_err());
    assert_eq!(sim.journal().len(), n);
}

#[test]
fn explicit_parent() {
    let mut sim = KernelSim::<Toy>::new();
    sim.apply(submit("a")).unwrap();
    let first = sim.journal()[0].id.clone();
    sim.apply(Input::Control(Control::Rewind { to: first.clone() })).unwrap();
    let j = sim.journal();
    let paused = &j[j.len() - 2];
    assert_eq!(paused.parent.as_ref(), Some(&first));
    assert_eq!(j.last().unwrap().parent.as_ref(), Some(&paused.id));
}

#[test]
fn scripted_model_tool_loop_with_prefix_check() {
    let model = Script::new().call("edit", json!({"file": "README.md", "old": "foo", "new": "bar"})).say("Done").check_prefix();
    let mut w = ScriptWorld { model: model.clone() };
    let journal = KernelSim::<Toy>::new().run([submit("Edit README")], &mut w);
    assert_eq!(outcomes(&journal), vec![TurnOutcome::Done { text: "Done".into() }]);
    let reqs = model.requests();
    assert_eq!(reqs.len(), 2);
    let m0 = reqs[0].body["messages"].as_array().unwrap().clone();
    let m1 = reqs[1].body["messages"].as_array().unwrap().clone();
    assert_eq!(&m1[..m0.len()], &m0[..]);
    assert_eq!(m1.len(), 3);
}

#[test]
fn scripted_overflow_fails_turn() {
    let mut w = ScriptWorld { model: Script::new().overflow() };
    let journal = KernelSim::<Toy>::new().run([submit("x")], &mut w);
    assert!(matches!(&outcomes(&journal)[0], TurnOutcome::Failed { .. }));
}

#[test]
fn stalled_world() {
    struct Nothing;
    impl World for Nothing {
        fn resolve(&mut self, _: EffectId, _: &Effect) -> Option<EffectResult> {
            None
        }
    }
    let mut sim = KernelSim::<Toy>::new();
    sim.apply(submit("a")).unwrap();
    assert_eq!(sim.run_until_idle(&mut Nothing), Step::Stalled);
    assert_eq!(sim.pending().len(), 1);
}

#[test]
fn crash_recovery_reconciles_outstanding() {
    let mut sim = KernelSim::<Toy>::new();
    sim.apply(submit("tools: 3")).unwrap();
    let mut w = EchoWorld::default();
    assert!(matches!(sim.step_with(&mut w), Step::Delivered(_))); // sample -> 3 executes
    assert_eq!(sim.pending().len(), 3);
    sim.step_with(&mut w); // one execute done
    sim.crash_and_recover();
    assert_eq!(sim.pending().len(), 2);
    assert_eq!(sim.run_until_idle(&mut w), Step::Idle);
    assert_eq!(outcomes(sim.journal()), vec![TurnOutcome::Done { text: "echo: tools: 3".into() }]);
    assert!(sim.rejections().is_empty());
}

fn reference(input: &str) -> (Vec<Rendered>, Vec<TurnOutcome>, usize) {
    let mut sim = KernelSim::<Toy>::new();
    sim.apply(submit(input)).unwrap();
    sim.run_until_idle(&mut EchoWorld::default());
    let b = sim.boundaries();
    let j = sim.journal().to_vec();
    (model_view(&j), outcomes(&j), b)
}

#[test]
fn crash_at_every_effect_boundary_matches_fault_free() {
    let (view, outs, boundaries) = reference("tools: 3");
    assert!(boundaries >= 5);
    for k in 0..=boundaries {
        let j = KernelSim::<Toy>::new().crash_at_effect(k, [submit("tools: 3")], &mut EchoWorld::default());
        assert_eq!(model_view(&j), view, "k = {k}");
        assert_eq!(outcomes(&j), outs, "k = {k}");
    }
}

proptest! {
    /// Tool results reach the model in call order whatever the delivery order,
    /// and a crash at any boundary does not change what the model sees.
    #[test]
    fn interleaving_and_crashes_are_invisible(seed in any::<u64>(), n in 1usize..5, k in 0usize..12) {
        let input = format!("tools: {n}");
        let (view, outs, _) = reference(&input);
        let j = KernelSim::<Toy>::new()
            .with_scheduler(Scheduler::seeded(seed))
            .run([submit(&input)], &mut EchoWorld::default());
        prop_assert_eq!(&model_view(&j), &view);
        let j = KernelSim::<Toy>::new()
            .with_scheduler(Scheduler::seeded(seed))
            .crash_at_effect(k, [submit(&input)], &mut EchoWorld::default());
        prop_assert_eq!(&model_view(&j), &view);
        prop_assert_eq!(outcomes(&j), outs);
    }

    #[test]
    fn same_seed_same_journal(seed in any::<u64>()) {
        let run = || KernelSim::<Toy>::new()
            .with_scheduler(Scheduler::seeded(seed))
            .with_tick(1)
            .run([submit("tools: 4")], &mut EchoWorld::default());
        prop_assert_eq!(run(), run());
    }
}
