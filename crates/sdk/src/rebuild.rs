//! Kernel-backed prompt rebuilder for the debug request-consistency check:
//! fold the journal up to (not including) the `EffectIssued` of a sample and
//! re-derive the prompt from the projected context.

use agent_kernel::{Decider, Kernel, State};
use agent_proto::*;
use agent_runtime::PromptRebuilder;

pub(crate) struct KernelRebuilder;

impl PromptRebuilder for KernelRebuilder {
    fn rebuild(&self, events: &[Envelope<Event>], effect: EffectId) -> Result<Prompt, String> {
        let mut s = State::default();
        let mut issued = None;
        for ev in events {
            if let Event::EffectIssued { id, effect: Effect::Sample(p) } = &ev.body {
                if *id == effect {
                    issued = Some(p.max_tokens);
                    break;
                }
            }
            Kernel::evolve(&mut s, ev);
        }
        let max_tokens = issued.ok_or_else(|| format!("sample {effect} not found in journal"))?;
        let mut p = agent_kernel::current_prompt(&s).ok_or("no open request sequence")?;
        p.max_tokens = max_tokens;
        Ok(p)
    }
}
