//! Built-in tools. Each lives in its own module so that a tool's unit struct
//! never shadows a parameter name of another tool (e.g. `grep`'s `glob`).

mod fs_tools;
mod glob_tool;
mod grep_tool;
mod memory;
mod skill;
mod web;

pub use fs_tools::{edit, read, write};
pub use glob_tool::glob;
pub use grep_tool::grep;
pub use memory::{remember, Recall};
pub use skill::LoadSkill;
pub use web::web_fetch;
