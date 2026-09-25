//! Kernel-backed prompt rebuilder for the debug request-consistency check:
//! fold the journal up to (not including) the `EffectIssued` of a sample and
//! re-derive the prompt from the projected context.
//!
//! Since event schema 2 the journal holds the sample by reference
//! (`SampleRef { seq_no, entries, max_tokens }`). The rebuild does not trust
//! the reference: it derives the prompt from the projection
//! (`current_prompt`) and checks that the reference names the open sequence
//! and the whole context, so a kernel that dispatched anything else is caught.

use agent_kernel::{Decider, Kernel, State};
use agent_proto::*;
use agent_runtime::PromptRebuilder;

pub(crate) struct KernelRebuilder;

impl PromptRebuilder for KernelRebuilder {
    fn rebuild(&self, events: &[Envelope<Event>], effect: EffectId) -> Result<Prompt, String> {
        let mut s = State::default();
        let mut issued = None;
        for ev in events {
            if let Event::EffectIssued { id, effect: e @ (Effect::Sample(_) | Effect::SampleRef(_)) } = &ev.body {
                if *id == effect {
                    issued = Some(e.clone());
                    break;
                }
            }
            Kernel::evolve(&mut s, ev);
        }
        let issued = issued.ok_or_else(|| format!("sample {effect} not found in journal"))?;
        let mut p = agent_kernel::current_prompt(&s).ok_or("no open request sequence")?;
        match issued {
            Effect::SampleRef(r) => {
                if r.seq_no != p.head.seq_no {
                    return Err(format!(
                        "sample {effect} references sequence {} but sequence {} is open",
                        r.seq_no, p.head.seq_no
                    ));
                }
                if r.entries as usize != p.body.len() {
                    return Err(format!(
                        "sample {effect} references {} context fragments but the context has {}",
                        r.entries,
                        p.body.len()
                    ));
                }
                p.max_tokens = r.max_tokens;
            }
            Effect::Sample(recorded) => p.max_tokens = recorded.max_tokens,
            _ => unreachable!(),
        }
        Ok(p)
    }
}
