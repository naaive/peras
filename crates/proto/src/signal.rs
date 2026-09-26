//! Inputs from outside: signals (enter the context) and controls (change execution).

use crate::ids::{EventId, ModelId, QuestionId};
use crate::verdict::Answer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    /// `@file` path, relative to the workspace.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Signals enter the model context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Signal {
    /// A user message that starts a turn when idle (or queues if busy).
    Submit {
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    /// Steer: delivered at the next safe point without interrupting; the turn may
    /// not end before it is delivered.
    Steer { text: String },
    /// Queue: starts a new turn once back to Idle.
    Queue { text: String },
    /// Notification from system/task/hook: delivered at the next safe point as a
    /// source-annotated data block; same-`key` notifications merge in the mailbox.
    Notify {
        source: String,
        key: String,
        text: String,
        #[serde(default)]
        untrusted: bool,
    },
    /// Wake: when idle, start a new turn; consumes continuation budget.
    Wake { source: String, reason: String },
    /// Silent: update state only; appended as a snapshot at the next safe point
    /// when it changed. An empty value clears the key.
    Silent { key: String, value: String },
}

/// Controls change execution only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    /// Stop after the current tool finishes.
    SoftInterrupt,
    /// Cancel immediately.
    HardInterrupt,
    Pause,
    Resume,
    /// Answer a pending question.
    Answer {
        question: QuestionId,
        answer: Answer,
        /// Who answered (user name / "code").
        responder: String,
    },
    /// Go back to the given event and produce a workspace rewind plan.
    Rewind { to: EventId },
    /// Switch model: opens a new request sequence.
    SwitchModel { model: ModelId },
    /// User-reviewed explicit taint clearance (itself an event).
    ClearTaint,
    /// Configuration was recompiled; applied at the next idle.
    Reconfigure { config: Box<crate::config::KernelConfig> },
}
