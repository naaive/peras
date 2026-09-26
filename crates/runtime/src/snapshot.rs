//! [`StateCodec`] helpers.

use crate::ports::StateCodec;
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;

/// JSON codec for any serde state. The bytes carry a format tag
/// (`{"v": <version>, "state": ...}`); a snapshot written with another
/// version fails to decode, so the runtime falls back to a full fold.
pub struct JsonCodec<S> {
    version: u32,
    _s: PhantomData<fn() -> S>,
}

impl<S> JsonCodec<S> {
    /// `version`: bump whenever the state's shape changes.
    pub fn new(version: u32) -> Self {
        JsonCodec { version, _s: PhantomData }
    }
}

impl<S> Default for JsonCodec<S> {
    fn default() -> Self {
        Self::new(1)
    }
}

impl<S: Serialize + DeserializeOwned> StateCodec<S> for JsonCodec<S> {
    fn encode(&self, state: &S) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&serde_json::json!({ "v": self.version, "state": state })).map_err(|e| e.to_string())
    }

    fn decode(&self, bytes: &[u8]) -> Result<S, String> {
        let mut v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let got = v.get("v").and_then(|v| v.as_u64());
        if got != Some(self.version as u64) {
            return Err(format!("snapshot version {got:?}, expected {}", self.version));
        }
        serde_json::from_value(v["state"].take()).map_err(|e| e.to_string())
    }
}
