//! Resource URIs, access declarations and side-effect classes.
//!
//! Resource URI schemes:
//! `fs:///repo/src/**`, `net:api.github.com:443`, `cmd:cargo test*`,
//! `mcp:github/create_issue`, `secret:GITHUB_TOKEN`, `mem:project/conventions`,
//! `git:refs`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    Fs,
    Net,
    Cmd,
    Mcp,
    Secret,
    Mem,
    Git,
}

impl Scheme {
    pub fn prefix(self) -> &'static str {
        match self {
            Scheme::Fs => "fs://",
            Scheme::Net => "net:",
            Scheme::Cmd => "cmd:",
            Scheme::Mcp => "mcp:",
            Scheme::Secret => "secret:",
            Scheme::Mem => "mem:",
            Scheme::Git => "git:",
        }
    }
}

/// A resource URI. For `fs`, the path is absolute (`fs:///abs/path`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ResourceUri(pub String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid resource uri: {0}")]
pub struct UriError(pub String);

impl ResourceUri {
    pub fn parse(s: &str) -> Result<Self, UriError> {
        let u = ResourceUri(s.to_string());
        u.scheme().ok_or_else(|| UriError(s.to_string()))?;
        if s.starts_with("fs://") && !s["fs://".len()..].starts_with('/') {
            return Err(UriError(s.to_string()));
        }
        Ok(u)
    }
    pub fn fs(abs_path: &str) -> Self {
        debug_assert!(abs_path.starts_with('/'));
        ResourceUri(format!("fs://{abs_path}"))
    }
    pub fn net(host: &str, port: u16) -> Self {
        ResourceUri(format!("net:{host}:{port}"))
    }
    pub fn cmd(command_line: &str) -> Self {
        ResourceUri(format!("cmd:{command_line}"))
    }
    pub fn mcp(server: &str, tool: &str) -> Self {
        ResourceUri(format!("mcp:{server}/{tool}"))
    }
    pub fn secret(name: &str) -> Self {
        ResourceUri(format!("secret:{name}"))
    }
    pub fn mem(key: &str) -> Self {
        ResourceUri(format!("mem:{key}"))
    }
    pub fn git(what: &str) -> Self {
        ResourceUri(format!("git:{what}"))
    }
    pub fn scheme(&self) -> Option<Scheme> {
        [
            Scheme::Fs,
            Scheme::Net,
            Scheme::Cmd,
            Scheme::Mcp,
            Scheme::Secret,
            Scheme::Mem,
            Scheme::Git,
        ]
        .into_iter()
        .find(|s| self.0.starts_with(s.prefix()))
    }
    /// The part after the scheme prefix (for `fs`, the absolute path).
    pub fn rest(&self) -> &str {
        match self.scheme() {
            Some(s) => &self.0[s.prefix().len()..],
            None => &self.0,
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourceUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    Read,
    Write,
}

/// One declared access of a tool call. For `fs` the uri may be a glob; `Opaque`
/// calls declare `fs:///<workspace>/**` Write (exclusive over the workspace).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Access {
    pub resource: ResourceUri,
    pub mode: AccessMode,
    /// Content hash observed when read (stale-write detection) or the hash of the
    /// definition file that defines a command (`npm run`, `make`...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl Access {
    pub fn read(resource: ResourceUri) -> Self {
        Access { resource, mode: AccessMode::Read, content_hash: None }
    }
    pub fn write(resource: ResourceUri) -> Self {
        Access { resource, mode: AccessMode::Write, content_hash: None }
    }
}

/// Side-effect class of a tool call; decides crash recovery and gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Read file, search, GET. Re-run after crash.
    Pure,
    /// Edit file, write memory. Restore originals then re-run.
    LocalWrite,
    /// POST request. Ask the user after crash.
    Network,
    /// Send mail, `git push`. Ask; rewind only lists it.
    Irreversible,
    /// Unanalysable shell command. Ask; exclusive over the workspace.
    Opaque,
}

impl EffectClass {
    pub fn is_read_only(self) -> bool {
        matches!(self, EffectClass::Pure)
    }
}
