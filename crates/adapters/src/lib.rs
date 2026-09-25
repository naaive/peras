//! `agent-adapters`: concrete implementations of the runtime ports.
//!
//! - [`model`]: model ports (Anthropic, OpenAI-compatible), frozen encoders,
//!   SSE mapping and composable layers (`retry`, `rate_limit`, `meter`).
//! - [`store`]: SQLite journal + blob stores, filesystem blob store.
//! - [`memory`]: file-backed long-term memory.
//! - [`sandbox`]: platform probe, bubblewrap / seatbelt / direct / container.

pub mod memory;
pub mod model;
pub mod sandbox;
pub mod store;


pub use model::{
    AnthropicEncoderV1, Claude, Meter, Metered, ModelPortExt, OpenAiCompat, OpenAiEncoderV1, Price, Quota, RateLimited,
    Retry, RetryPolicy,
};


pub use memory::FileMemoryStore;
pub use sandbox::{detect, probe, BwrapSandbox, Container, DirectExec, SeatbeltSandbox};
pub use store::{FsBlobStore, Sqlite, SqliteBlobStore, SqliteJournal};
