//! Identifiers and scalar value types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Milliseconds since the Unix epoch. Injected by the driver; the kernel's only
/// source of time.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn millis(self) -> u64 {
        self.0
    }
    pub fn saturating_sub(self, other: Timestamp) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
    };
}

string_id!(
    /// Event id: a ULID assigned by the driver when the event is appended.
    EventId
);
string_id!(
    /// Session id. Sub-agent session ids are derived from the parent's call id.
    SessionId
);
string_id!(
    /// Tool-call id as produced by the model (or synthesised by the kernel).
    CallId
);
string_id!(
    /// Id of a pending question (approval request).
    QuestionId
);
string_id!(
    /// Id of a workspace checkpoint in the shadow snapshot store.
    CheckpointId
);
string_id!(
    /// Model identifier, e.g. `claude-sonnet-5`.
    ModelId
);

impl SessionId {
    /// Deterministically derive a child session id from the parent session and the
    /// call that spawned it, so recovery always finds the same child.
    pub fn child(&self, call: &CallId) -> SessionId {
        SessionId(format!("{}/{}", self.0, call.0))
    }
}

/// Monotonic per-session sequence number. Also the subscription cursor.
pub type Seq = u64;

/// Identifies an issued effect. `epoch` is the interrupt generation: every
/// interrupt increments it and late results from an older epoch are dropped.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema,
)]
pub struct EffectId {
    pub epoch: u32,
    pub n: u64,
}

impl fmt::Display for EffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "e{}.{}", self.epoch, self.n)
    }
}

/// Content-addressed reference to a blob (lowercase hex sha256 of its bytes).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct BlobRef {
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Lease generation held by a driver over a session. Appends carry it; a stale
/// generation is rejected by the journal store.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct LeaseGen(pub u64);
