//! Golden replay, vendor side: the recorded session in
//! `crates/sim/tests/golden/session.jsonl` is re-folded with `Kernel::evolve`
//! (through the read-time upgrader); for every `Sample` in the journal (a
//! `SampleRef` since event schema 2) the prompt rebuilt from the fold is encoded with the frozen
//! `AnthropicEncoderV1` and `OpenAiEncoderV1`, and the results must equal
//! `requests.anthropic.jsonl` / `requests.openai.jsonl` byte for byte.
//! This guards "historical requests are rebuilt byte-for-byte after upgrades".
//!
//! (It lives here because agent-sim has no dependency on agent-adapters; the
//! fixture itself and the final-prompt golden are checked by
//! `crates/sim/tests/golden_replay.rs`.)
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p agent-bench --test golden_replay`
//! (after regenerating the fixture with the agent-sim test, if needed).

use agent_adapters::{AnthropicEncoderV1, OpenAiEncoderV1};
use agent_kernel::{current_prompt, rebuild_effect, Decider, Kernel, State};
use agent_proto::upgrade::read_envelope;
use agent_proto::*;
use agent_runtime::Encoder;
use serde_json::{json, Value};
use std::path::PathBuf;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sim/tests/golden")
}

fn updating() -> bool {
    std::env::var("UPDATE_GOLDEN").map(|v| v == "1").unwrap_or(false)
}

fn load_fixture() -> Vec<Envelope<Event>> {
    let text = std::fs::read_to_string(golden_dir().join("session.jsonl")).expect("missing golden session fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let raw: Value = serde_json::from_str(l).expect("fixture line is JSON");
            read_envelope(raw).expect("fixture event upgrades").expect("no ignorable events in fixture")
        })
        .collect()
}

/// `(seq of the EffectIssued, prompt rebuilt from the fold)` for every Sample.
fn rebuilt_prompts() -> Vec<(Seq, Prompt)> {
    let mut s = State::default();
    let mut out = vec![];
    for e in load_fixture() {
        // Upgraded to a reference (schema 2): rebuilt from the fold before it,
        // which is the prompt `current_prompt` gives at that point.
        if let Event::EffectIssued { effect: r @ Effect::SampleRef(_), .. } = &e.body {
            let Some(Effect::Sample(rebuilt)) = rebuild_effect(&s, r) else { panic!("seq {}: no rebuild", e.seq) };
            assert_eq!(Some(&rebuilt), current_prompt(&s).as_ref(), "rebuilt prompt differs at seq {}", e.seq);
            out.push((e.seq, rebuilt));
        }
        Kernel::evolve(&mut s, &e);
    }
    out
}

fn encode_all(enc: &dyn Encoder, prompts: &[(Seq, Prompt)]) -> String {
    let mut out = String::new();
    for (seq, p) in prompts {
        let req = enc.encode(&p.head, &p.body, p.max_tokens);
        let line = json!({
            "seq": seq,
            "encoder_version": req.encoder_version,
            "max_tokens": req.max_tokens,
            "body": req.body,
        });
        out.push_str(&serde_json::to_string(&line).unwrap());
        out.push('\n');
    }
    out
}

fn check(file: &str, actual: String) {
    let path = golden_dir().join(file);
    if updating() {
        std::fs::write(&path, &actual).unwrap();
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {}: run with UPDATE_GOLDEN=1", path.display()));
    if actual != expected {
        let (a, e): (Vec<_>, Vec<_>) = (actual.lines().collect(), expected.lines().collect());
        let first = a.iter().zip(&e).position(|(x, y)| x != y).unwrap_or(a.len().min(e.len()));
        panic!(
            "{} differs from the rebuilt requests (first differing line {}, {} vs {} lines); \
             UPDATE_GOLDEN=1 to accept an intentional change",
            path.display(),
            first + 1,
            a.len(),
            e.len()
        );
    }
}

#[test]
fn anthropic_requests_rebuild_byte_for_byte() {
    let prompts = rebuilt_prompts();
    assert!(prompts.len() >= 8, "fixture has {} samples", prompts.len());
    check("requests.anthropic.jsonl", encode_all(&AnthropicEncoderV1::default(), &prompts));
}

#[test]
fn openai_requests_rebuild_byte_for_byte() {
    let prompts = rebuilt_prompts();
    check("requests.openai.jsonl", encode_all(&OpenAiEncoderV1, &prompts));
}
