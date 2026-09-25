//! `agent-kernel`: the pure decision core.
//!
//! No IO, no clock reads, no user code, no hash-order iteration. `decide` turns
//! an input into events and effects; `evolve` folds an event into state. Both are
//! pure; replay only re-runs `evolve`.
//!
//! INTERFACE CONTRACT (used by `agent-runtime` and `agent-sim`; keep stable):
//! - [`Decider`], [`Decision`], [`Rejection`]
//! - [`Kernel`] implements `Decider<State = State>`
//! - [`render::render`]
//! - [`State`] is `Serialize + Deserialize` (JSON snapshots: load the snapshot,
//!   then fold the events after it)

use agent_proto::{Draft, EffectId, Effect, Envelope, Event, Input, Timestamp};

mod context;
mod decide;
pub mod gate;
pub mod render;
pub mod sched;
pub mod state;

pub use decide::{config_hash, start_session};
pub use state::{
    Phase, State, Subagent, Taint, OVERFLOW_KIND, PENDING_CONFIG_KIND, PENDING_SIGNAL_KIND, SIGNAL_DROPPED_KIND,
};

use agent_proto::{KernelConfig, Prompt, Question, Rendered, SeqHead};

/// Whether the session context is tainted (untrusted content entered it and no
/// `TaintCleared` followed). The runtime passes this on to sub-agents
/// (`SubagentSpawner::run_child(.., tainted_input)`); gate requests carry it in
/// `GateRequest::tainted`. Tool call inputs are never modified.
pub fn is_tainted(s: &State) -> bool {
    s.taint.tainted
}

/// Full taint projection (sources, labels, private-data flag).
pub fn taint(s: &State) -> &Taint {
    &s.taint
}

/// Destinations allowlisted for this session by a human ("allow this
/// destination for the session"), in addition to the configured egress allowlist.
pub fn allowed_destinations(s: &State) -> Vec<String> {
    s.destinations.iter().cloned().collect()
}

/// Sub-agents spawned by this session (from `SubagentStarted` /
/// `SubagentFinished`), ordered by spawning call id. Taint of a child's output is
/// carried by the trust of the spawning call's tool result.
pub fn subagents(s: &State) -> Vec<Subagent> {
    s.children.values().cloned().collect()
}

/// Whether `Event::Tombstone` erased this event (directly or, for a summary,
/// in cascade from one of its sources).
pub fn is_erased(s: &State, id: &agent_proto::EventId) -> bool {
    s.erased.contains(id)
}

/// Current execution phase.
pub fn phase(s: &State) -> Phase {
    state::phase(s)
}

/// The current model context (renderings in order), i.e. the next prompt body.
pub fn context(s: &State) -> Vec<Rendered> {
    s.context.iter().map(|e| (*e.rendered).clone()).collect()
}

/// Where one fragment of the current context came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSource {
    /// Seq of the originating event (for a summary: the first replaced seq).
    pub seq: agent_proto::Seq,
    /// The originating event (for a summary: the `Replaced` event).
    pub event: agent_proto::EventId,
    /// Event type name (`user_message`, `tool_resulted`, `state_snapshot`...),
    /// `<type>:trimmed` / `<type>:images_offloaded` for level-2/3 replacements,
    /// `replaced:summary` for a level-4 summary.
    pub kind: String,
    pub rendered: Rendered,
}

/// One entry per fragment of the current context, in prompt order
/// (`agent context explain`).
pub fn context_sources(s: &State) -> Vec<ContextSource> {
    s.context.iter().map(context::source_of).collect()
}

/// Head of the current request sequence.
pub fn current_head(s: &State) -> Option<&SeqHead> {
    s.head.as_ref()
}

/// The configuration currently in force.
pub fn config(s: &State) -> Option<&KernelConfig> {
    s.config.as_ref()
}

/// Questions awaiting an answer (answer with `Control::Answer`).
pub fn pending_questions(s: &State) -> Vec<Question> {
    s.questions.values().map(|q| q.question.clone()).collect()
}

/// The prompt the next `Sample` would carry (for request/journal consistency
/// assertions in debug builds).
pub fn current_prompt(s: &State) -> Option<Prompt> {
    let head = s.head.clone()?;
    let max_tokens = s.caps.as_ref().map(|c| c.max_output).unwrap_or(8_192);
    Some(Prompt { head, body: context(s), max_tokens })
}

/// Whether dispatch is paused (`Control::Pause`); effects issued meanwhile are
/// returned by the `Control::Resume` decision.
pub fn is_paused(s: &State) -> bool {
    s.paused
}

/// Current interrupt epoch (effect ids of older epochs are stale).
pub fn epoch(s: &State) -> u32 {
    s.epoch
}

/// Output of `decide`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decision {
    /// Drafted events; the driver assigns id/seq/at, appends them (log first),
    /// feeds each back through `evolve`, then dispatches `effects`.
    pub events: Vec<Draft<Event>>,
    /// Effects to dispatch. Each must also appear as an `Event::EffectIssued` in
    /// `events` (so `outstanding` can reconcile after a crash).
    pub effects: Vec<(EffectId, Effect)>,
}

/// An input the kernel refuses in the current state (the driver reports it and
/// keeps going; nothing is written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub reason: String,
}

impl Rejection {
    pub fn new(reason: impl Into<String>) -> Self {
        Rejection { reason: reason.into() }
    }
}

pub trait Decider {
    type State: Default;
    fn decide(s: &Self::State, at: Timestamp, input: Input) -> Result<Decision, Rejection>;
    fn evolve(s: &mut Self::State, ev: &Envelope<Event>);
    /// Issued but not settled effects: reconciled on recovery.
    fn outstanding(s: &Self::State) -> Vec<(EffectId, Effect)>;
}

/// The coding-agent kernel.
pub struct Kernel;

impl Decider for Kernel {
    type State = State;

    fn decide(s: &State, at: Timestamp, input: Input) -> Result<Decision, Rejection> {
        state::decide(s, at, input)
    }

    fn evolve(s: &mut State, ev: &Envelope<Event>) {
        state::evolve(s, ev)
    }

    fn outstanding(s: &State) -> Vec<(EffectId, Effect)> {
        state::outstanding(s)
    }
}
