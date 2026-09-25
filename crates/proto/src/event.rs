//! Journal events: facts that happened. Immutable once written.

use crate::config::KernelConfig;
use crate::effect::{CheckpointInfo, Effect, RestorePlan, RestoreReport, TurnOutcome};
use crate::ids::{CallId, EffectId, EventId, ModelId, QuestionId, Seq, SessionId};
use crate::model::{AssistantMessage, SeqHead};
use crate::signal::Attachment;
use crate::tool::{ToolCall, ToolResult};
use crate::verdict::{Answer, HookPoint, Question, Responder, Ring, Verdict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Current schema version of [`Event`]. Bump when a variant changes shape and add
/// an upgrade step in [`crate::upgrade`].
pub const EVENT_SCHEMA: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnCause {
    User,
    Queued,
    Wake,
    /// Stop hook asked to continue / undelivered steer.
    Continuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementKind {
    /// Level 2: old tool results keep head+tail; supersedable snapshots dropped.
    Trim,
    /// Level 3: old images replaced by attachment paths.
    ImageOffload,
    /// Level 4: earliest segment replaced by a summary.
    Summary,
    /// Model switch: history re-rendered for the new model's render profile.
    Rerender,
}

/// The only way to shorten the context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Replacement {
    pub kind: ReplacementKind,
    /// Inclusive seq range of replaced events.
    pub range: (Seq, Seq),
    /// Events whose content was replaced.
    pub sources: Vec<EventId>,
    /// Untrusted source labels carried over from replaced content (gates use them).
    pub untrusted_sources: Vec<String>,
    /// The replacement renderings (stored in the envelope's `rendered` is not
    /// enough since a replacement may produce several fragments).
    pub content: Vec<crate::render::Rendered>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ---- session / sequence ----
    SessionStarted {
        session: SessionId,
        /// Hash of the compiled profile.
        profile_hash: String,
        config: KernelConfig,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<SessionId>,
    },
    /// A request sequence opened (head is fixed from here on).
    SequenceOpened { head: SeqHead },
    ConfigChanged { profile_hash: String, config: KernelConfig },
    ModelSwitched { from: ModelId, to: ModelId, reason: String },

    // ---- turn lifecycle ----
    TurnStarted { cause: TurnCause },
    UserMessage { text: String, #[serde(default)] attachments: Vec<Attachment> },
    /// A steer / notification delivered at a safe point.
    Injected { source: String, text: String },
    AssistantReplied { message: AssistantMessage, effect: EffectId },
    ToolResulted { call: ToolCall, result: ToolResult },
    TurnEnded { outcome: TurnOutcome },

    // ---- effects / verdicts ----
    /// Written before the effect is dispatched (log first, then act).
    EffectIssued { id: EffectId, effect: Effect },
    /// An issued effect finished (its content is in the specific event).
    EffectSettled { id: EffectId },
    VerdictRecorded {
        subject: GateRef,
        point: HookPoint,
        ring: Ring,
        verdict: Verdict,
        responder: Responder,
    },
    QuestionAsked { question: Question, subject: GateRef },
    QuestionAnswered { question: QuestionId, answer: Answer, responder: Responder },

    // ---- control ----
    Interrupted { hard: bool, epoch: u32 },
    Paused,
    Resumed,
    Suspended { reason: String },

    // ---- context management ----
    Replaced(Replacement),
    /// A state snapshot ("what is true now") appended when it changed.
    StateSnapshot { key: String, text: String },
    /// "Previous snapshots of this key are void".
    SnapshotCleared { key: String },
    /// Instructions file / skill directory injected on first access.
    InstructionsInjected { path: String, text: String },
    MemoryLoaded { text: String },

    // ---- security ----
    /// Taint introduced (projection helper; taint is still derived from trust).
    TaintCleared,
    DestinationAllowed { destination: String },

    // ---- checkpoints ----
    CheckpointTaken { info: CheckpointInfo },
    RewindPlanned { plan: RestorePlan },
    RewindCompleted { report: RestoreReport, to: EventId },

    // ---- sub-agents ----
    SubagentStarted { call: CallId, child: SessionId },
    SubagentFinished { call: CallId, child: SessionId, outcome: TurnOutcome },

    // ---- extensibility ----
    /// Plugin events. `ignorable` ones may be skipped by readers that do not
    /// know `kind`; unknown non-ignorable events refuse the session load.
    Plugin { kind: String, ignorable: bool, data: serde_json::Value },
    /// Tombstone: keeps tree structure, erases the body of `target`; summaries and
    /// memories derived from it are cascaded.
    Tombstone { target: EventId },
}

/// What a verdict / question refers to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum GateRef {
    Call(CallId),
    Turn(u64),
    Session,
}

impl Event {
    pub fn type_name(&self) -> &'static str {
        match self {
            Event::SessionStarted { .. } => "session_started",
            Event::SequenceOpened { .. } => "sequence_opened",
            Event::ConfigChanged { .. } => "config_changed",
            Event::ModelSwitched { .. } => "model_switched",
            Event::TurnStarted { .. } => "turn_started",
            Event::UserMessage { .. } => "user_message",
            Event::Injected { .. } => "injected",
            Event::AssistantReplied { .. } => "assistant_replied",
            Event::ToolResulted { .. } => "tool_resulted",
            Event::TurnEnded { .. } => "turn_ended",
            Event::EffectIssued { .. } => "effect_issued",
            Event::EffectSettled { .. } => "effect_settled",
            Event::VerdictRecorded { .. } => "verdict_recorded",
            Event::QuestionAsked { .. } => "question_asked",
            Event::QuestionAnswered { .. } => "question_answered",
            Event::Interrupted { .. } => "interrupted",
            Event::Paused => "paused",
            Event::Resumed => "resumed",
            Event::Suspended { .. } => "suspended",
            Event::Replaced(_) => "replaced",
            Event::StateSnapshot { .. } => "state_snapshot",
            Event::SnapshotCleared { .. } => "snapshot_cleared",
            Event::InstructionsInjected { .. } => "instructions_injected",
            Event::MemoryLoaded { .. } => "memory_loaded",
            Event::TaintCleared => "taint_cleared",
            Event::DestinationAllowed { .. } => "destination_allowed",
            Event::CheckpointTaken { .. } => "checkpoint_taken",
            Event::RewindPlanned { .. } => "rewind_planned",
            Event::RewindCompleted { .. } => "rewind_completed",
            Event::SubagentStarted { .. } => "subagent_started",
            Event::SubagentFinished { .. } => "subagent_finished",
            Event::Plugin { .. } => "plugin",
            Event::Tombstone { .. } => "tombstone",
        }
    }
}
