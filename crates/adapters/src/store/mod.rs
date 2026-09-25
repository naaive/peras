//! Journal and blob stores.

pub mod fs;
pub mod sqlite;

pub use fs::FsBlobStore;
pub use sqlite::{Sqlite, SqliteBlobStore, SqliteJournal};

use agent_proto::BlobRef;
use sha2::{Digest, Sha256};

/// Content address of `bytes`.
pub fn blob_ref(bytes: &[u8], media_type: Option<&str>) -> BlobRef {
    BlobRef {
        sha256: hex::encode(Sha256::digest(bytes)),
        size: bytes.len() as u64,
        media_type: media_type.map(str::to_string),
    }
}

pub(crate) fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
