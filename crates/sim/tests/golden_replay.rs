//! Golden replay: a recorded session (JSON Lines of `Envelope<Event>` under
//! `tests/golden/`) is re-folded with `Kernel::evolve` through the read-time
//! upgrader. The resulting state must be unchanged after framework upgrades:
//!
//! - the final `agent_kernel::current_prompt` equals `golden/prompt.json`
//!   byte for byte;
//! - the fixture was recorded with event schema 1, where every `Sample` /
//!   `Compact` effect carried its full prompt. The upgrader turns them into
//!   `SampleRef` / `CompactRef`; at every one of them, the request rebuilt from
//!   the fold (`rebuild_effect`, and `Kernel::outstanding` right after) equals
//!   the prompt recorded in the old journal, byte for byte;
//! - running the scenario today journals exactly the upgraded fixture.
//!
//! The byte-exact vendor encodings of those prompts are checked by
//! `crates/bench/tests/golden_replay.rs` (agent-sim has no dependency on the
//! adapters).
//!
//! Regenerate the expected outputs (after an intentional change) with
//! `UPDATE_GOLDEN=1 cargo test -p agent-sim --test golden_replay`, then
//! `UPDATE_GOLDEN=1 cargo test -p agent-bench --test golden_replay`.
//! `session.jsonl` itself is an old recording and is never rewritten (it is
//! only created, by the deterministic `scenario()` below, when missing).

use agent_kernel::{current_prompt, rebuild_effect, start_session, Decider, Kernel, State};
use agent_proto::upgrade::read_envelope;
use agent_proto::*;
use agent_sim::{sample_blocking, KernelSim, Script, SeqIds, VirtualClock, World};
use serde_json::{json, Value};
use std::path::PathBuf;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("golden")
}

fn updating() -> bool {
    std::env::var("UPDATE_GOLDEN").map(|v| v == "1").unwrap_or(false)
}

// ---------------------------------------------------------------- scenario

fn config() -> KernelConfig {
    let mut c = KernelConfig::default();
    // A small window so that one session shows level-2 trims and a level-4
    // summary.
    c.caps.window = 1_500;
    c.caps.max_output = 512;
    c.caps.render.preview_bytes = 64;
    c.compaction.output_reserve = 200;
    c.compaction.keep_recent_tokens = 300;
    c.security.workspace_root = "/ws".into();
    c.security.workspace_trusted = true;
    c.system = vec!["You are a coding agent.".into(), "Be concise.".into()];
    c.tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: json!({"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}),
            class: EffectClass::Pure,
            subagent: false,
        },
        ToolSpec {
            name: "edit".into(),
            description: "Replace text in a file.".into(),
            input_schema: json!({"type":"object","properties":{"file":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["file","old","new"]}),
            class: EffectClass::LocalWrite,
            subagent: false,
        },
    ];
    c
}

/// The scripted model: one step per `Sample`.
fn script() -> Script {
    Script::new()
        // turn 1: read, edit (approved by the user), answer
        .call("read", json!({"file": "/ws/README.md"}))
        .call("edit", json!({"file": "/ws/README.md", "old": "teh", "new": "the"}))
        .say("Fixed the typo in README.md.")
        // turn 2: parallel reads with large outputs -> trims + summary
        .calls([("read", json!({"file": "/ws/src/a.rs"})), ("read", json!({"file": "/ws/src/b.rs"}))])
        .call("read", json!({"file": "/ws/src/c.rs"}))
        .call("read", json!({"file": "/ws/src/d.rs"}))
        .call("read", json!({"file": "/ws/src/e.rs"}))
        .call("read", json!({"file": "/ws/src/f.rs"}))
        .call("read", json!({"file": "/ws/src/g.rs"}))
        .say("The sources implement a small parser.")
        // (rewind to the end of turn 1)
        // turn 3: edit denied by the user
        .call("edit", json!({"file": "/ws/CHANGELOG.md", "old": "", "new": "- fix typo"}))
        .say("Understood, I left CHANGELOG.md unchanged.")
        // spare steps: a kernel change adding a sample fails loudly in the
        // outcome assertions rather than with an exhausted script
        .say("spare")
        .say("spare")
}

/// Deterministic world: samples come from the script (tool accesses filled in
/// like the runtime's registry does), tool outputs depend only on the call,
/// the user approves everything except edits to CHANGELOG.md.
struct Recorded {
    model: Script,
    checkpoints: u32,
}

fn access_for(name: &str, input: &Value) -> (Vec<Access>, EffectClass) {
    let file = input["file"].as_str().unwrap_or("/ws").to_string();
    match name {
        "edit" => (vec![Access::write(ResourceUri::fs(&file))], EffectClass::LocalWrite),
        _ => (vec![Access::read(ResourceUri::fs(&file))], EffectClass::Pure),
    }
}

fn output_for(call: &ToolCall) -> String {
    let file = call.input["file"].as_str().unwrap_or("?");
    match call.name.as_str() {
        "read" if file.ends_with(".rs") => {
            (0..40).map(|i| format!("fn item_{i}() -> u32 {{ {i} }} // {file}\n")).collect::<String>()
        }
        "read" => "# Project\n\nThis is teh readme.\n".into(),
        "edit" => format!("edited {file}"),
        other => format!("unknown tool {other}"),
    }
}

impl World for Recorded {
    fn resolve(&mut self, _id: EffectId, effect: &Effect) -> Option<EffectResult> {
        Some(match effect {
            Effect::Sample(p) => match sample_blocking(&self.model, p) {
                EffectResult::Sampled(mut m) => {
                    for b in &mut m.content {
                        if let ContentBlock::ToolUse(c) = b {
                            let (access, class) = access_for(&c.name, &c.input);
                            c.access = access;
                            c.class = class;
                        }
                    }
                    EffectResult::Sampled(m)
                }
                other => other,
            },
            Effect::Execute(b) => EffectResult::Executed(
                b.calls.iter().map(|c| ToolResult::text(c.id.clone(), output_for(c), false)).collect(),
            ),
            Effect::Gate(req) => {
                let deny = matches!(&req.subject, GateSubject::Tool { call } if call.input["file"] == "/ws/CHANGELOG.md");
                let verdict = if deny { Verdict::deny("not now") } else { Verdict::Allow };
                let responder = if req.question.is_some() { Responder::Human("alice".into()) } else { Responder::Kernel };
                EffectResult::Gated { verdict, responder, remember: false }
            }
            Effect::Checkpoint(_) => {
                self.checkpoints += 1;
                EffectResult::Checkpointed(CheckpointInfo {
                    id: CheckpointId::new(format!("cp{}", self.checkpoints)),
                    agent_changes: vec![],
                    external_changes: vec![],
                })
            }
            Effect::Compact(_) => EffectResult::Compacted {
                summary: "The user asked to fix a README typo (done) and to review src/*.rs (parser code).".into(),
                trust: Trust::Internal,
            },
            Effect::Restore(_) => EffectResult::Restored(RestoreReport::default()),
            Effect::Finish(_) => return None,
            Effect::SampleRef(_) | Effect::CompactRef(_) => panic!("journal reference dispatched: {effect:?}"),
        })
    }
}

fn submit(t: &str) -> Input {
    Input::Signal(Signal::Submit { text: t.into(), attachments: vec![] })
}

/// Run the deterministic scenario and return its journal.
fn scenario() -> Vec<Envelope<Event>> {
    let mut sim = KernelSim::<Kernel>::new()
        .with_clock(VirtualClock::new(1_750_000_000_000))
        .with_ids(SeqIds::new())
        .with_tick(1_000);
    sim.apply_decision(start_session(SessionId::new("golden"), "golden-profile".into(), config()));
    let mut world = Recorded { model: script(), checkpoints: 0 };

    sim.apply(submit("Fix the typo in README.md")).unwrap();
    sim.run_until_idle(&mut world);
    let mark = sim.journal().last().unwrap().id.clone();

    sim.apply(submit("Now review the parser sources under src/.")).unwrap();
    sim.run_until_idle(&mut world);
    assert!(
        sim.journal().iter().any(|e| matches!(&e.body, Event::Replaced(r) if r.kind == ReplacementKind::Trim)),
        "scenario must contain a trim"
    );
    assert!(
        sim.journal().iter().any(|e| matches!(&e.body, Event::Replaced(r) if r.kind == ReplacementKind::Summary)),
        "scenario must contain a summary"
    );

    sim.apply(Input::Control(Control::Rewind { to: mark })).unwrap();
    sim.run_until_idle(&mut world);
    assert!(sim.journal().iter().any(|e| matches!(e.body, Event::RewindCompleted { .. })), "rewind completed");

    sim.apply(submit("Add a changelog entry.")).unwrap();
    sim.run_until_idle(&mut world);

    assert!(sim.rejections().is_empty(), "rejections: {:?}", sim.rejections());
    assert!(sim.pending().is_empty(), "pending: {:?}", sim.pending());
    assert_eq!(
        agent_sim::outcomes(sim.journal()),
        vec![
            TurnOutcome::Done { text: "Fixed the typo in README.md.".into() },
            TurnOutcome::Done { text: "The sources implement a small parser.".into() },
            TurnOutcome::Done { text: "Understood, I left CHANGELOG.md unchanged.".into() },
        ]
    );
    assert_eq!(sim.journal().iter().filter(|e| matches!(e.body, Event::QuestionAnswered { .. })).count(), 2);
    sim.journal().to_vec()
}

/// Serialize the journal as JSON Lines. The first `user_message` is written
/// in the old schema-0 shape (no `attachments`) so that replay exercises the
/// read-time upgrader, like an old recording would.
fn to_jsonl(journal: &[Envelope<Event>]) -> String {
    let mut out = String::new();
    let mut downgraded = false;
    for e in journal {
        let mut v = serde_json::to_value(e).unwrap();
        if !downgraded && e.body.type_name() == "user_message" {
            v["schema"] = json!(0);
            v["body"].as_object_mut().unwrap().remove("attachments");
            downgraded = true;
        }
        out.push_str(&serde_json::to_string(&v).unwrap());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------- replay

fn raw_fixture() -> Vec<Value> {
    let text = std::fs::read_to_string(golden_dir().join("session.jsonl"))
        .expect("missing fixture: run with UPDATE_GOLDEN=1 to create it");
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).expect("fixture line is JSON")).collect()
}

fn load_fixture() -> Vec<Envelope<Event>> {
    raw_fixture()
        .into_iter()
        .map(|raw| read_envelope(raw).expect("fixture event upgrades").expect("no ignorable events in fixture"))
        .collect()
}

/// A request rebuilt from the fold, next to what the journal says about it.
struct Rebuilt {
    seq: Seq,
    id: EffectId,
    /// `rebuild_effect` on the fold just before the `EffectIssued`.
    before: Option<Effect>,
    /// What `Kernel::outstanding` hands out right after it (crash recovery).
    outstanding: Option<Effect>,
}

/// Fold the journal; returns the final state and every request (`Sample` /
/// `Compact`) rebuilt from the fold.
fn fold(journal: &[Envelope<Event>]) -> (State, Vec<Rebuilt>) {
    let mut s = State::default();
    let mut requests = vec![];
    for e in journal {
        let issued = match &e.body {
            Event::EffectIssued { id, effect } if effect.kind() == "sample" || effect.kind() == "compact" => {
                Some((*id, rebuild_effect(&s, effect)))
            }
            _ => None,
        };
        Kernel::evolve(&mut s, e);
        if let Some((id, before)) = issued {
            let outstanding = Kernel::outstanding(&s).into_iter().find(|(i, _)| *i == id).map(|(_, e)| e);
            requests.push(Rebuilt { seq: e.seq, id, before, outstanding });
        }
    }
    (s, requests)
}

fn prompt_json(p: &Option<Prompt>) -> String {
    let mut s = serde_json::to_string_pretty(p).unwrap();
    s.push('\n');
    s
}

#[test]
fn golden_replay_is_unchanged() {
    let dir = golden_dir();
    if updating() && !dir.join("session.jsonl").exists() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), to_jsonl(&scenario())).unwrap();
    }
    let journal = load_fixture();
    assert!(journal.len() > 50, "fixture too small: {}", journal.len());
    for (i, e) in journal.iter().enumerate() {
        assert_eq!(e.seq, i as u64, "seqs are dense");
        assert_eq!(e.schema, EVENT_SCHEMA, "upgraded on read");
        if let Event::EffectIssued { effect, .. } = &e.body {
            assert!(!matches!(effect, Effect::Sample(_) | Effect::Compact(_)), "seq {}: not upgraded to a reference", e.seq);
        }
    }

    // The old recording carries every request in full: the rebuilt ones must
    // match it byte for byte.
    let recorded: Vec<(Seq, String)> = raw_fixture()
        .iter()
        .filter(|v| v["body"]["type"] == "effect_issued")
        .filter(|v| v["body"]["effect"]["effect"] == "sample" || v["body"]["effect"]["effect"] == "compact")
        .map(|v| (v["seq"].as_u64().unwrap(), serde_json::to_string(&v["body"]["effect"]).unwrap()))
        .collect();
    let (state, requests) = fold(&journal);
    assert!(requests.len() >= 8, "fixture has {} requests", requests.len());
    assert!(
        recorded.iter().any(|(_, r)| r.contains("\"effect\":\"compact\"")),
        "fixture must contain a compaction request"
    );
    assert_eq!(requests.iter().map(|r| r.seq).collect::<Vec<_>>(), recorded.iter().map(|r| r.0).collect::<Vec<_>>());
    for (r, (seq, old)) in requests.iter().zip(&recorded) {
        let json = |e: &Option<Effect>| e.as_ref().map(|e| serde_json::to_string(e).unwrap());
        assert_eq!(json(&r.before).as_ref(), Some(old), "request {} rebuilt from the fold differs at seq {seq}", r.id);
        // Unless the same decision already settled it, recovery re-dispatches it identically.
        if r.outstanding.is_some() {
            assert_eq!(json(&r.outstanding).as_ref(), Some(old), "outstanding {} differs at seq {seq}", r.id);
        }
    }
    assert!(requests.iter().all(|r| r.outstanding.is_some()), "every request is outstanding right after its EffectIssued");
    // Folding again (a resumed driver) gives the same projection.
    let (again, _) = fold(&journal);
    assert_eq!(current_prompt(&again), current_prompt(&state));

    let expected_path = dir.join("prompt.json");
    let actual = prompt_json(&current_prompt(&state));
    if updating() {
        std::fs::write(&expected_path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&expected_path).expect("missing golden prompt: run with UPDATE_GOLDEN=1");
    assert!(actual == expected, "final current_prompt differs from {} (UPDATE_GOLDEN=1 to accept)", expected_path.display());
}

/// Running the scenario with today's kernel journals exactly the upgraded old
/// recording (requests by reference, same ids, same renderings).
#[test]
fn fresh_run_journals_the_upgraded_fixture() {
    let fresh = scenario();
    let old = load_fixture();
    assert_eq!(fresh.len(), old.len(), "event count differs");
    for (a, b) in fresh.iter().zip(&old) {
        assert_eq!(serde_json::to_string(a).unwrap(), serde_json::to_string(b).unwrap(), "event {} differs", a.seq);
    }
    // And it is linear: no request carries its prompt.
    let new_bytes = to_jsonl(&fresh).len();
    let old_bytes: usize = raw_fixture().iter().map(|v| v.to_string().len() + 1).sum();
    eprintln!("golden journal: {old_bytes} bytes at schema 1, {new_bytes} bytes now");
    assert!(new_bytes * 10 < old_bytes * 7, "journal did not shrink: {old_bytes} -> {new_bytes} bytes");
}

/// The scenario itself is deterministic (two runs give identical bytes).
#[test]
fn scenario_is_deterministic() {
    assert_eq!(to_jsonl(&scenario()), to_jsonl(&scenario()));
}
