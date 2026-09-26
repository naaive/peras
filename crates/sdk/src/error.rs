use agent_proto::{Question, SessionId};
use agent_runtime::DriverError;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// Configuration problem (reported at first run, or by `Agent::check`).
    #[error("config: {0}")]
    Config(String),
    #[error(transparent)]
    Driver(#[from] DriverError),
    /// The turn failed (model/infrastructure error).
    #[error("failed: {0}")]
    Failed(String),
    #[error("interrupted")]
    Interrupted,
    /// Waiting on an approval nobody could answer; resume the session later.
    #[error("suspended (session {session})")]
    Suspended { session: SessionId, question: Option<Box<Question>> },
    #[error("budget exhausted: {0}")]
    Budget(String),
    /// `.json::<T>()` could not parse the final answer.
    #[error("json: {0}")]
    Json(String),
    /// The run ended without a turn outcome (session closed).
    #[error("run ended unexpectedly")]
    Ended,
}
