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
    /// List the slash commands the server expands (`/name args`).
    ListCommands,
    Ping,
}

/// A slash command offered by the server (from the profile's command table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

/// Split `/name args` into `("name", "args")`. Text that does not start with
/// `/` followed by a command name (letters, digits, `-`, `_`, `:`, `.`) is
/// not a slash command.
pub fn parse_slash(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    ok.then(|| (name, rest[end..].trim()))
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
    /// Reply to `ListCommands`.
    Commands { commands: Vec<CommandInfo> },
    /// A question that is not in this session's event stream: one a sub-agent
    /// asked, forwarded to its parent's clients (answer it with `Answer` on
    /// this session, compare-and-swap like any other).
    Question { session: SessionId, question: crate::verdict::Question },
    Pong,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::parse_slash;

    #[test]
    fn slash_parsing() {
        assert_eq!(parse_slash("/review src/lib.rs  "), Some(("review", "src/lib.rs")));
        assert_eq!(parse_slash("  /help"), Some(("help", "")));
        assert_eq!(parse_slash("/"), None);
        assert_eq!(parse_slash("/usr/bin is broken"), None);
        assert_eq!(parse_slash("hello /x"), None);
    }
}

/// Exit codes of the headless client.
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    /// Suspended on an approval: resume later with `--resume`.
    pub const SUSPENDED: i32 = 20;
    pub const INTERRUPTED: i32 = 130;
}
