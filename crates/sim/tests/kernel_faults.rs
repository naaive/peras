//! Fault injection against the real kernel: crash after the k-th effect
//! boundary, recover by folding the journal and reconciling outstanding
//! effects, finish — the model-visible journal and the outcomes must match a
//! fault-free run, for every k.

use agent_kernel::{start_session, Kernel};
use agent_proto::*;
use agent_sim::{model_view, outcomes, KernelSim, World};
use serde_json::json;
use std::collections::BTreeMap;

/// A deterministic world: the model first calls `read` then `edit`, then answers.
/// Tool results depend only on the call; every gate allows.
#[derive(Default, Clone)]
struct Scripted {
    samples: usize,
}

fn call(id: &str, name: &str, class: EffectClass, access: Vec<Access>) -> ToolCall {
    ToolCall { id: CallId::new(id), name: name.into(), input: json!({ "file": "a.txt" }), access, class }
}

impl World for Scripted {
    fn resolve(&mut self, _id: EffectId, effect: &Effect) -> Option<EffectResult> {
        Some(match effect {
            Effect::Sample(_) => {
                self.samples += 1;
                let content = match self.samples {
                    1 => vec![ContentBlock::ToolUse(call("c1", "read", EffectClass::Pure, vec![Access::read(ResourceUri::fs("/w/a.txt"))]))],
                    2 => vec![ContentBlock::ToolUse(call("c2", "edit", EffectClass::LocalWrite, vec![Access::write(ResourceUri::fs("/w/a.txt"))]))],
                    _ => vec![ContentBlock::Text { text: "done".into() }],
                };
                let stop = if self.samples < 3 { StopReason::ToolUse } else { StopReason::EndTurn };
                EffectResult::Sampled(AssistantMessage { content, stop, usage: Usage { input_tokens: 100, output_tokens: 10, ..Default::default() } })
            }
            Effect::Execute(b) => EffectResult::Executed(
                b.calls.iter().map(|c| ToolResult::text(c.id.clone(), format!("{} ok", c.name), false)).collect(),
            ),
            Effect::Gate(_) => EffectResult::Gated { verdict: Verdict::Allow, responder: Responder::Human("sim".into()), remember: false },
            Effect::Checkpoint(_) => EffectResult::Checkpointed(CheckpointInfo { id: CheckpointId::new("cp"), agent_changes: vec![], external_changes: vec![] }),
            Effect::Compact(_) => EffectResult::Compacted { summary: "summary".into(), trust: Trust::Internal },
            Effect::Restore(_) => EffectResult::Restored(RestoreReport::default()),
            Effect::Finish(_) => return None,
            Effect::SampleRef(_) | Effect::CompactRef(_) => panic!("journal reference dispatched: {effect:?}"),
        })
    }
}

fn config() -> KernelConfig {
    let mut c = KernelConfig::default();
    c.security.workspace_root = "/w".into();
    c.rules.push(PolicyRule {
        name: "allow-workspace".into(),
        resource: Some("fs:///w/**".into()),
        tool: None,
        mode: None,
        action: PolicyAction::Allow,
        layer: Layer::Cli,
    });
    c
}

fn sim() -> KernelSim<Kernel> {
    let mut s = KernelSim::<Kernel>::new();
    s.apply_decision(start_session(SessionId::new("s"), "h".into(), config()));
    s
}

fn inputs() -> Vec<Input> {
    vec![Input::Signal(Signal::Submit { text: "fix a.txt".into(), attachments: vec![] })]
}

/// The world's sample counter must reflect how many samples were *recorded* —
/// a sample re-dispatched after a crash repeats the same model reply, like a
/// deterministic model would.
struct Replaying {
    inner: Scripted,
}

impl World for Replaying {
    fn resolve(&mut self, id: EffectId, effect: &Effect) -> Option<EffectResult> {
        if let Effect::Sample(p) = effect {
            // Number of assistant replies already in the prompt = samples done.
            let done = p.body.iter().filter(|r| r.role == Role::Assistant).count();
            self.inner.samples = done;
        }
        self.inner.resolve(id, effect)
    }
}

#[test]
fn fault_free_run_completes() {
    let journal = sim().run(inputs(), &mut Replaying { inner: Scripted::default() });
    assert_eq!(outcomes(&journal), vec![TurnOutcome::Done { text: "done".into() }]);
    let tools = journal.iter().filter(|e| matches!(e.body, Event::ToolResulted { .. })).count();
    assert_eq!(tools, 2);
}

#[test]
fn crash_at_every_effect_boundary_converges() {
    let reference = sim().run(inputs(), &mut Replaying { inner: Scripted::default() });
    let ref_view = model_view(&reference);
    let ref_out = outcomes(&reference);
    let boundaries = {
        let mut s = sim();
        for i in inputs() {
            s.apply(i).unwrap();
        }
        s.run_until_idle(&mut Replaying { inner: Scripted::default() });
        s.boundaries()
    };
    assert!(boundaries >= 4, "scenario too small: {boundaries}");
    for k in 0..=boundaries {
        let j = sim().crash_at_effect(k, inputs(), &mut Replaying { inner: Scripted::default() });
        assert_eq!(outcomes(&j), ref_out, "outcome differs after crash at boundary {k}");
        assert_eq!(model_view(&j), ref_view, "model view differs after crash at boundary {k}");
    }
}

/// Records the JSON of every request (Sample / Compact) the world is handed,
/// by effect id, and checks that a re-dispatch of the same id is identical.
struct Recording<W> {
    inner: W,
    requests: BTreeMap<EffectId, String>,
}

impl<W: World> World for Recording<W> {
    fn resolve(&mut self, id: EffectId, effect: &Effect) -> Option<EffectResult> {
        if matches!(effect, Effect::Sample(_) | Effect::Compact(_)) {
            let j = serde_json::to_string(effect).unwrap();
            if let Some(prev) = self.requests.insert(id, j.clone()) {
                assert_eq!(prev, j, "{id}: request re-dispatched with different bytes");
            }
        }
        self.inner.resolve(id, effect)
    }
}

/// Requests are journaled by reference; after a crash at any effect boundary,
/// the outstanding requests rebuilt from the journal are byte-identical to the
/// ones dispatched before the crash, and to the fault-free run's.
#[test]
fn recovered_requests_are_byte_identical() {
    let mut reference = Recording { inner: Replaying { inner: Scripted::default() }, requests: BTreeMap::new() };
    let journal = sim().run(inputs(), &mut reference);
    assert!(reference.requests.len() >= 3, "{:?}", reference.requests.keys());
    for e in &journal {
        if let Event::EffectIssued { id, effect } = &e.body {
            assert!(!matches!(effect, Effect::Sample(_) | Effect::Compact(_)), "{id}: full prompt journaled");
        }
    }
    let mut k = 0;
    loop {
        let mut s = sim();
        for i in inputs() {
            s.apply(i).unwrap();
        }
        let mut world = Recording { inner: Replaying { inner: Scripted::default() }, requests: BTreeMap::new() };
        while s.boundaries() < k {
            match s.step_with(&mut world) {
                agent_sim::Step::Delivered(_) | agent_sim::Step::Finished(_) => {}
                _ => break,
            }
        }
        if s.boundaries() < k {
            break;
        }
        let before: BTreeMap<EffectId, String> =
            s.pending().iter().map(|(id, e)| (*id, serde_json::to_string(e).unwrap())).collect();
        s.crash_and_recover();
        let after: BTreeMap<EffectId, String> =
            s.pending().iter().map(|(id, e)| (*id, serde_json::to_string(e).unwrap())).collect();
        for (id, e) in s.pending() {
            if let Effect::Sample(_) | Effect::Compact(_) = e {
                assert_eq!(before.get(id), after.get(id), "boundary {k}: {id} rebuilt differently after the crash");
                assert_eq!(reference.requests.get(id), after.get(id), "boundary {k}: {id} differs from the fault-free run");
            }
        }
        for (id, e) in &before {
            if e.contains("\"effect\":\"sample\"") {
                assert!(after.contains_key(id), "boundary {k}: outstanding sample {id} lost in recovery");
            }
        }
        s.run_until_idle(&mut world);
        for (id, j) in &world.requests {
            assert_eq!(reference.requests.get(id), Some(j), "boundary {k}: {id} differs from the fault-free run");
        }
        k += 1;
    }
    assert!(k >= 4, "scenario too small: {k}");
}
