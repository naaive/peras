//! Service protocol messages (stdio / WebSocket / in-process channel).

use crate::envelope::Envelope;
use crate::event::Event;
use crate::ids::{EffectId, QuestionId, Seq, SessionId};
use crate::signal::{Control, Signal};
use crate::verdict::Answer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Transient stream: token deltas, progress, heartbeats. Never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "pulse", rename_all = "snake_case")]
pub enum Pulse {
    TextDelta { effect: EffectId, text: String },
    ThinkingDelta { effect: EffectId, text: String },
    ToolProgress { call: String, message: String },
    Heartbeat { seq: Seq },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Signal(Signal),
    Control(Control),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { versions: Vec<u32>, client: String },
    /// Subscribe to the event stream from `from_seq` (replay, then live).
    Subscribe { session: SessionId, from_seq: Seq, pulses: bool },
    /// Commands carry an idempotency key and may be retried safely.
    Command { session: SessionId, key: String, command: Command },
    /// Approvals are compare-and-swap: first answer wins.
    Answer { session: SessionId, key: String, question: QuestionId, answer: Answer },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { version: u32 },
    Event { session: SessionId, event: Box<Envelope<Event>> },
    Pulse { session: SessionId, pulse: Pulse },
    /// Acknowledges a command (duplicate keys get the original ack).
    Ack { key: String, accepted: bool, #[serde(default)] error: Option<String> },
    /// A question was answered by someone else: close the dialog.
    QuestionClosed { session: SessionId, question: QuestionId },
    Pong,
    Error { message: String },
}

/// Exit codes of the headless client.
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    /// Suspended on an approval: resume later with `--resume`.
    pub const SUSPENDED: i32 = 20;
    pub const INTERRUPTED: i32 = 130;
}
