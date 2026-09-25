//! `agent-profile`: configuration discovery, layered merge and compilation into
//! an immutable [`Profile`].
//!
//! Two steps:
//! 1. [`discover`] (IO) reads the layer files and extension files into [`Sources`].
//! 2. [`compile`] (pure) merges them into a [`Profile`] whose hash is
//!    deterministic: the same sources always give the same profile.
//!
//! Layers, highest priority first: Managed, Cli, LocalProject
//! (`.agent/settings.local.toml`), SharedProject (`.agent/settings.toml`), User
//! (`~/.agent/settings.toml`), Default.
//!
//! Merge rules:
//! - plain fields: the highest layer that sets a field wins;
//! - permission rules: merged across layers; deny rules are ordered first so
//!   any layer's deny wins;
//! - sensitive fields (`auto_answer`, `unattended.on_ask`,
//!   `security.egress_allow`, `security.trusted_sources`,
//!   `security.workspace_trusted`, `security.disposable_env`, `mcp.*.trusted`)
//!   are set only by Managed / Cli / User; project layers may only tighten, and
//!   loosening attempts are ignored with a [`Warning`];
//! - `locked = [..]` in the managed file freezes keys for all other layers;
//! - untrusted workspace: project hooks, MCP servers, commands, agents, system
//!   additions and non-deny permission rules are dropped (with warnings), and
//!   project instruction files / skills are marked untrusted.

pub mod compile;
pub mod frontmatter;
pub mod instructions;
pub mod profile;
pub mod settings;
pub mod sources;

pub use compile::{accepts_sensitive, compile, is_project, on_ask_strictness, stricter_on_ask, ConfigError};
pub use instructions::{apply_budget, InstructionFile, DEFAULT_INSTRUCTION_BUDGET};
pub use profile::*;
pub use settings::*;
pub use sources::{discover, find_project_root, DiscoverOptions, Scope, SourceFile, Sources};

/// Discover and compile in one go.
pub fn load(opts: &DiscoverOptions) -> Result<Profile, LoadError> {
    let s = discover(opts).map_err(|e| LoadError::Io(e.to_string()))?;
    compile(&s).map_err(LoadError::Config)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[cfg(test)]
mod tests;
