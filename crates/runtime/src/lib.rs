//! `agent-runtime`: the driver loop and the ports it dispatches effects through.
//!
//! - [`driver`] — [`Runtime`], [`SessionHandle`] — one actor per session, log
//!   first then act, hard-interrupt contract, crash recovery, subscriptions.
//! - [`dispatch`] — effect execution (sample assembly, tool batches with blob
//!   spill, gates, checkpoints, compaction).
//! - [`gate`] — default ring 4/5 executor [`GateChain`], hooks, auto rules and the
//!   compare-and-swap [`AskBoard`].
//! - [`registry`] — [`ToolRegistry`].
//! - [`shadow`] — [`ShadowCheckpointer`] (content-addressed shadow snapshots).
//! - [`tasks`] — background [`TaskRegistry`].
//! - [`mem`] — in-memory / null port implementations for tests and simulation.

pub mod assemble;
pub mod dispatch;
pub mod driver;
pub mod gate;
pub mod mem;
pub mod ports;
pub mod registry;
pub mod shadow;
pub mod tasks;

pub use assemble::Assembler;
pub use dispatch::{Env, RuntimeOptions};
pub use driver::{Applied, DriverError, Runtime, RuntimeBuilder, SessionHandle};
pub use gate::{AnswerError, AskBoard, AutoRule, FnHook, FnRule, GateChain, Hook};
pub use mem::*;
pub use ports::*;
pub use registry::ToolRegistry;
pub use shadow::ShadowCheckpointer;
pub use tasks::{TaskId, TaskInfo, TaskRegistry, TaskStatus};
