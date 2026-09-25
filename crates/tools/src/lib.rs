//! `agent-tools`: capability handles, the `#[tool]` built-ins, the `Bash` tool with
//! its shell analysis and semantic table, and a minimal MCP client.
//!
//! ```ignore
//! use agent_tools::prelude::*;
//!
//! /// Replace the unique occurrence of `old` with `new`.
//! #[tool]
//! pub async fn edit(file: Write<File>, old: String, new: String) -> Result<()> {
//!     file.replace_once(&old, &new).await
//! }
//! ```

// Lets `#[tool]` refer to this crate as `::agent_tools` from inside it (and from
// its own tests), just like from any dependent crate.
extern crate self as agent_tools;

pub mod bash;
pub mod builtin;
pub mod caps;
mod fsafe;
pub mod mcp;
pub mod shell;
pub mod testing;

#[doc(hidden)]
pub mod __private;

pub use agent_macros::{agent_test, tool};
pub use bash::{Bash, BashTool};
pub use builtin::{edit, glob, grep, read, remember, web_fetch, write, LoadSkill, Recall};
pub use caps::{
    Capability, Cmd, Dir, Exec, File, Get, Key, Mem, Name, Net, Observations, Read, Secret, Url,
    Write,
};
pub use mcp::{McpClient, McpError, McpTool};
pub use schemars;
pub use shell::{SemanticTable, ShellAnalysis};

/// Error type of tool bodies.
pub use agent_runtime::ToolError as Error;

/// `Result` with [`ToolError`](agent_runtime::ToolError) as the default error.
pub type Result<T, E = agent_runtime::ToolError> = std::result::Result<T, E>;

/// Everything needed to write tools.
pub mod prelude {
    pub use crate::caps::{
        Capability, Cmd, Dir, Exec, File, Get, Key, Mem, Name, Net, Read, Secret, Url, Write,
    };
    pub use crate::{tool, Bash, Result};
    pub use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
}
