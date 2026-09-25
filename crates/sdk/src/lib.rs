//! `agent`: the application-facing facade.
//!
//! ```no_run
//! use agent::prelude::*;
//!
//! # async fn demo() -> Result<(), agent::Error> {
//! let agent = Agent::new(Claude::default().retry(3))
//!     .tools((read, edit, Bash))
//!     .policy("agent.toml")
//!     .journal(Sqlite("runs.db"))
//!     .gate(|p: &Proposal| {
//!         if p.writes(".github/**") { Verdict::ask("CI config change") } else { Verdict::Allow }
//!     })
//!     .observe(|e: &ToolFailed| tracing::warn!(%e));
//!
//! let summary = agent.run("Summarize this repository").await?;
//!
//! let mut run = agent.run("Fix the failing tests");
//! run.control().steer("Don't touch the public API");
//! while let Some(u) = run.next().await {
//!     match u { Update::Text(t) => print!("{t}"), Update::Ask(ask) => ask.allow(), _ => {} }
//! }
//! # Ok(()) }
//! ```

mod agent;
mod error;
mod gate;
mod hooks;
mod observe;
mod run;
mod session;
mod subagent;
mod tool_set;

pub use crate::agent::{Agent, Memory, Sqlite};
pub use crate::error::Error;
pub use crate::gate::Proposal;
pub use crate::observe::{Observed, ToolFailed, ToolFinished, TurnFinished};
pub use crate::run::{Ask, Run, RunControl, Update};
pub use crate::session::Chat;
pub use crate::tool_set::{IntoTool, IntoTools};

pub use agent_adapters as adapters;
pub use agent_kernel as kernel;
pub use agent_profile as profile;
pub use agent_proto as proto;
pub use agent_runtime as runtime;
pub use agent_sim as sim;
pub use agent_tools as tools;

/// `#[agent::test]`: async test with a fresh temporary workspace.
pub use agent_tools::agent_test as test;
/// `#[agent::tool]`.
pub use agent_tools::tool;

pub mod prelude {
    pub use crate::{Agent, Ask, Chat, Error, Memory, Proposal, Run, Sqlite, ToolFailed, ToolFinished, TurnFinished, Update};
    pub use agent_adapters::{Claude, Container, ModelPortExt, OpenAiCompat};
    pub use agent_proto::{Answer, OnAsk, RestoreReport, TurnOutcome, Verdict};
    pub use agent_sim::Script;
    pub use agent_tools::builtin::*;
    pub use agent_tools::caps::{Cmd, Dir, Exec, File, Get, Key, Mem, Name, Net, Read, Secret, Url, Write};
    pub use agent_tools::{tool, Bash, LoadSkill, Recall};
    pub use futures::StreamExt;
    pub use serde_json::json;
    /// Tool body result type (`Result<T, ToolError>`).
    pub type Result<T, E = agent_runtime::ToolError> = std::result::Result<T, E>;
}
