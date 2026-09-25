//! The event envelope and trust annotations.

use crate::ids::{EventId, Seq, Timestamp};
use crate::render::Rendered;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who produced an event.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Origin {
    User,
    System,
    Kernel,
    Model,
    Hook(String),
    Tool(String),
    Plugin(String),
    /// Another session (e.g. a sub-agent or a notification from another session).
    Session(String),
}

/// How the content of an event must be treated.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trust {
    /// Typed by the user. Presented verbatim.
    User,
    /// Trusted guidance: framework reminders, trusted workspace instructions,
    /// hooks from trusted configuration, long-term memory. Presented through the
    /// instruction channel (system / system-reminder), never data-framed.
    Guidance,
    /// Untrusted data with its source label (web page, MCP result, untrusted
    /// workspace file, other session...). Data-framed with a fixed warning.
    Untrusted { source: String },
    /// Framework bookkeeping with no model-visible content.
    Internal,
}

impl Trust {
    pub fn is_untrusted(&self) -> bool {
        matches!(self, Trust::Untrusted { .. })
    }
    /// The more restrictive of two trust labels (used when content is merged,
    /// e.g. a hook rewriting a tool result keeps the original's untrusted label).
    pub fn weakest(a: &Trust, b: &Trust) -> Trust {
        match (a, b) {
            (Trust::Untrusted { .. }, _) => a.clone(),
            (_, Trust::Untrusted { .. }) => b.clone(),
            (Trust::User, _) | (_, Trust::User) => Trust::User,
            (Trust::Guidance, _) | (_, Trust::Guidance) => Trust::Guidance,
            _ => Trust::Internal,
        }
    }
}

/// Who sees an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Model,
    User,
    Both,
    /// Neither: pure bookkeeping (effect issued, verdict recorded...).
    None,
}

impl Audience {
    pub fn model_visible(self) -> bool {
        matches!(self, Audience::Model | Audience::Both)
    }
    pub fn user_visible(self) -> bool {
        matches!(self, Audience::User | Audience::Both)
    }
}

/// An appended event. Immutable once written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Envelope<E> {
    /// ULID.
    pub id: EventId,
    /// Edits, rewinds and forks are new events whose parent is an old node.
    pub parent: Option<EventId>,
    /// Monotonic per-session sequence number; also the subscription cursor.
    pub seq: Seq,
    /// Injected by the driver; the kernel's only time source.
    pub at: Timestamp,
    pub origin: Origin,
    pub trust: Trust,
    pub audience: Audience,
    /// Schema version of `body`; upgraded step by step on read, never migrated.
    pub schema: u16,
    /// Large content is referenced as blobs.
    pub body: E,
    /// Rendering of model-visible events, produced once at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered: Option<Rendered>,
}

/// Where a drafted event attaches in the event tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Parent {
    /// The head of the current branch (the usual case).
    #[default]
    Head,
    /// An explicit older node (edit, rewind, fork).
    Explicit(EventId),
}

/// An event produced by `decide`, before the driver assigns id/seq/at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Draft<E> {
    pub parent: Parent,
    pub origin: Origin,
    pub trust: Trust,
    pub audience: Audience,
    pub body: E,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered: Option<Rendered>,
}

impl<E> Draft<E> {
    pub fn internal(body: E) -> Self {
        Draft {
            parent: Parent::Head,
            origin: Origin::Kernel,
            trust: Trust::Internal,
            audience: Audience::None,
            body,
            rendered: None,
        }
    }
}
