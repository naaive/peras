//! `agent-sim`: deterministic simulation primitives.
//!
//! - [`VirtualClock`] / [`SeqIds`]: manual time and deterministic ids
//!   (implement `agent_runtime::Clock` / `IdGen`).
//! - [`Script`]: a scripted [`agent_runtime::ModelPort`] plus [`JsonEncoder`].
//! - [`Scheduler`]: seeded, controllable interleaving of pending completions.
//! - [`KernelSim`]: drives any [`agent_kernel::Decider`] without the runtime,
//!   with crash/recovery and fault injection ([`KernelSim::crash_at_effect`]).

pub mod clock;
pub mod kernel_sim;
pub mod sched;
pub mod script;

pub use clock::{SeqIds, VirtualClock};
pub use kernel_sim::{model_view, outcomes, KernelSim, Step, World};
pub use sched::{Rng, Scheduler};
pub use script::{collect_message, sample_blocking, JsonEncoder, Script, ToolName};
