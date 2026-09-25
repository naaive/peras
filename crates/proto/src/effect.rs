//! Effects (what the kernel asks the outside world to do) and kernel inputs.

use crate::envelope::Trust;
use crate::ids::{CallId, CheckpointId, EffectId, EventId, Seq};
use crate::model::{AssistantMessage, ModelError, SeqHead};
use crate::render::Rendered;
use crate::resource::Access;
use crate::signal::{Control, Signal};
use crate::tool::{ToolCall, ToolResult};
use crate::verdict::{ApprovalLevel, HookPoint, Proposal, Question, Responder, Ring, Verdict};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Everything that can enter the kernel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    /// Enters the context.
    Signal(Signal),
    /// Changes execution only.
    Control(Control),
    /// While sampling, one tool-call block is complete (enriched with access).
    Streamed(EffectId, ToolCall),
    /// Result of an effect, including actual token usage.
    Completed(EffectId, EffectResult),
}

/// A request to sample the model: the sequence head plus the append-only body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Prompt {
    pub head: SeqHead,
    pub body: Vec<Rendered>,
    pub max_tokens: u32,
}

/// Journaled form of [`Effect::Sample`] (only inside `Event::EffectIssued`).
///
/// The request is a pure function of the sequence head, the stored renderings
/// and the encoder version, so the journal records only where to find it: the
/// open sequence (`seq_no`) and the length of the model context when the effect
/// was issued. The full [`Prompt`] is rebuilt from the fold (the head of
/// sequence `seq_no` plus the first `entries` context fragments), which keeps the
/// journal linear in the session length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SampleRef {
    /// Sequence number of the head the request was built on.
    pub seq_no: u32,
    /// Number of context fragments in the request body (a prefix of the
    /// context at issue time: the whole context).
    pub entries: u32,
    pub max_tokens: u32,
}

/// Journaled form of [`Effect::Compact`] (only inside `Event::EffectIssued`):
/// the body is the first `entries` context fragments followed by one user
/// fragment carrying `instruction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompactRef {
    pub seq_no: u32,
    /// Context fragments replayed before the instruction (the whole context on
    /// the pressure path, the earliest segment on the overflow path).
    pub entries: u32,
    /// Text of the appended user fragment (`Rendered::text(Role::User, ..)`).
    pub instruction: String,
    pub max_tokens: u32,
    /// Seq range (inclusive) of the events being replaced.
    pub range: (Seq, Seq),
    pub overflow: bool,
}

/// A set of tool calls that may run concurrently (no resource conflicts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Batch {
    pub calls: Vec<ToolCall>,
    /// Accesses granted per call (the runtime hands only these handles out and
    /// compiles them into the sandbox profile).
    pub grants: Vec<(CallId, Vec<Access>)>,
}

/// What a gate request is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateSubject {
    SessionStart,
    UserSubmit { text: String },
    PreSample,
    Tool { call: ToolCall },
    PostTool { call: ToolCall, result: ToolResult },
    PreCompact,
    Stop { final_text: String },
}

/// Rings 4 (hooks) and 5 (human) run in the runtime: the kernel issues this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GateRequest {
    pub point: HookPoint,
    pub ring: Ring,
    pub subject: GateSubject,
    /// Set when ring 5 is asked: the question and its framework-assigned level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    pub level: ApprovalLevel,
    /// Taint status at the time of asking (hook authors / UIs show it).
    #[serde(default)]
    pub tainted: bool,
}

/// A summarisation job (pressure relief level 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompactJob {
    /// Pressure path: the current request replayed verbatim + one instruction
    /// appended at the end. Overflow path: only the earliest segment.
    pub prompt: Prompt,
    /// Seq range (inclusive) of the events being replaced.
    pub range: (Seq, Seq),
    pub overflow: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointScope {
    /// Declared writes about to happen (originals saved before executing).
    pub declared_writes: Vec<Access>,
    /// Safe-point checkpoint (full scan / watcher delta) vs pre-batch.
    pub safe_point: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RestorePlan {
    /// Target event of the rewind.
    pub to: EventId,
    /// Checkpoint whose agent-attributed changes are undone back to.
    pub checkpoint: Option<CheckpointId>,
}

/// The final outcome of a turn / run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Model finished and the Stop hook allowed it. Carries the final text.
    Done { text: String },
    Interrupted,
    /// Waiting on an approval nobody can answer (unattended): resume later.
    Suspended { question: Option<Question> },
    Failed { error: String },
    BudgetExhausted { what: String },
}

/// Kinds of effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    Sample(Prompt),
    Execute(Batch),
    Gate(GateRequest),
    Compact(CompactJob),
    Checkpoint(CheckpointScope),
    Restore(RestorePlan),
    Finish(TurnOutcome),
    /// Journal-only form of `Sample` (see [`SampleRef`]). Never dispatched: the
    /// kernel hands out the rebuilt `Sample`.
    SampleRef(SampleRef),
    /// Journal-only form of `Compact` (see [`CompactRef`]). Never dispatched.
    CompactRef(CompactRef),
}

impl Effect {
    /// Short kind name for logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Sample(_) | Effect::SampleRef(_) => "sample",
            Effect::Execute(_) => "execute",
            Effect::Gate(_) => "gate",
            Effect::Compact(_) | Effect::CompactRef(_) => "compact",
            Effect::Checkpoint(_) => "checkpoint",
            Effect::Restore(_) => "restore",
            Effect::Finish(_) => "finish",
        }
    }

    /// Whether this is a journal-only reference form (`SampleRef` /
    /// `CompactRef`) that must be expanded before dispatch.
    pub fn is_journal_ref(&self) -> bool {
        matches!(self, Effect::SampleRef(_) | Effect::CompactRef(_))
    }

    /// The journaled form of a dispatchable effect, given the number of context
    /// fragments its prompt replays: `Sample` / `Compact` become references,
    /// everything else is journaled as is. `None` when a compaction prompt does
    /// not end with its instruction fragment.
    pub fn journaled(&self) -> Option<Effect> {
        Some(match self {
            Effect::Sample(p) => Effect::SampleRef(SampleRef {
                seq_no: p.head.seq_no,
                entries: p.body.len() as u32,
                max_tokens: p.max_tokens,
            }),
            Effect::Compact(job) => {
                let (last, prefix) = job.prompt.body.split_last()?;
                let instruction = match last.blocks.as_slice() {
                    [crate::render::RBlock::Text { text }] if last.role == crate::render::Role::User => text.clone(),
                    _ => return None,
                };
                Effect::CompactRef(CompactRef {
                    seq_no: job.prompt.head.seq_no,
                    entries: prefix.len() as u32,
                    instruction,
                    max_tokens: job.prompt.max_tokens,
                    range: job.range,
                    overflow: job.overflow,
                })
            }
            other => other.clone(),
        })
    }
}

/// Info returned by a checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointInfo {
    pub id: CheckpointId,
    /// Files changed since the previous checkpoint, by attribution.
    pub agent_changes: Vec<String>,
    pub external_changes: Vec<String>,
}

/// Report of a workspace rewind.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    /// Current content differs from the agent's last write: left alone.
    pub conflicts: Vec<String>,
    /// Ignored paths that were changed but are not restored.
    pub unrestored_ignored: Vec<String>,
    /// Irreversible operations after the target (listed, not undone).
    pub irreversible: Vec<String>,
    /// git ref changes (HEAD / branches before→after).
    pub git_refs: Vec<String>,
}

/// Result of an effect, fed back as `Input::Completed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum EffectResult {
    Sampled(AssistantMessage),
    SampleFailed(ModelError),
    /// Results of an `Execute` batch (one per call, same order).
    Executed(Vec<ToolResult>),
    /// A ring-4/5 verdict and who gave it.
    Gated {
        verdict: Verdict,
        responder: Responder,
        /// The human chose "allow this destination for the rest of the session".
        #[serde(default)]
        remember: bool,
    },
    Compacted { summary: String, trust: Trust },
    CompactFailed(ModelError),
    Checkpointed(CheckpointInfo),
    Restored(RestoreReport),
    /// Infrastructure error (not a tool failure: those are results).
    Failed { error: String },
}

/// Convenience used by gates: the proposal a rewrite produced.
pub type Rewrite = Proposal;
