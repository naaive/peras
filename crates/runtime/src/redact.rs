//! Secret redaction: secrets reach tools only as handles
//! (`Secret<Name>` through the [`SecretSource`] port) and never enter the
//! context. Every value the port hands out is registered with a
//! [`Redactor`]; the driver redacts each input before the kernel sees it (so
//! nothing derived from it, in the journal, the model context, verdicts or
//! traces built from events, carries the value), and blobs are redacted
//! before they are stored ([`RedactingBlobs`]).
//!
//! The process-wide [`Redactor::global`] is the default, so values exposed
//! in one session (e.g. a sub-agent's) are redacted from every session of the
//! process. Values shorter than [`MIN_SECRET_LEN`] bytes are not redacted
//! (they would mangle ordinary text); encoded forms (base64, URL-encoded)
//! are not recognised.

use crate::ports::{BlobStore, SecretSource, StoreError};
use agent_proto::BlobRef;
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use std::borrow::Cow;
use std::sync::{Arc, OnceLock, RwLock};

/// Shorter values are not redacted.
pub const MIN_SECRET_LEN: usize = 4;

/// Known secret values and their names, longest value first.
#[derive(Debug, Default)]
pub struct Redactor {
    values: RwLock<Vec<(String, String)>>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide redactor (the runtime's default).
    pub fn global() -> Arc<Redactor> {
        static GLOBAL: OnceLock<Arc<Redactor>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Redactor::new())).clone()
    }

    /// Remember a secret value (from now on it is redacted everywhere).
    pub fn register(&self, name: &str, value: &str) {
        if value.len() < MIN_SECRET_LEN {
            return;
        }
        let mut v = self.values.write().unwrap_or_else(|e| e.into_inner());
        if v.iter().any(|(x, _)| x == value) {
            return;
        }
        v.push((value.to_string(), name.to_string()));
        // Longest first, so a secret containing another is replaced whole.
        v.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    }

    pub fn is_empty(&self) -> bool {
        self.values.read().map(|v| v.is_empty()).unwrap_or(true)
    }

    fn marker(name: &str) -> String {
        format!("[REDACTED:{name}]")
    }

    pub fn redact_str<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let values = self.values.read().unwrap_or_else(|e| e.into_inner());
        if !values.iter().any(|(v, _)| s.contains(v.as_str())) {
            return Cow::Borrowed(s);
        }
        let mut out = s.to_string();
        for (v, name) in values.iter() {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), &Self::marker(name));
            }
        }
        Cow::Owned(out)
    }

    pub fn redact_bytes<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        let values = self.values.read().unwrap_or_else(|e| e.into_inner());
        let mut cur: Cow<'a, [u8]> = Cow::Borrowed(b);
        for (v, name) in values.iter() {
            let needle = v.as_bytes();
            if !contains(&cur, needle) {
                continue;
            }
            let marker = Self::marker(name);
            let mut out = Vec::with_capacity(cur.len());
            let mut i = 0;
            while i < cur.len() {
                if cur[i..].starts_with(needle) {
                    out.extend_from_slice(marker.as_bytes());
                    i += needle.len();
                } else {
                    out.push(cur[i]);
                    i += 1;
                }
            }
            cur = Cow::Owned(out);
        }
        cur
    }

    /// Redact every string in a JSON value; true if anything changed.
    pub fn redact_json(&self, v: &mut serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => match self.redact_str(s) {
                Cow::Owned(r) => {
                    *s = r;
                    true
                }
                Cow::Borrowed(_) => false,
            },
            serde_json::Value::Array(a) => {
                // Every element, no short-circuit.
                let mut changed = false;
                for x in a {
                    changed |= self.redact_json(x);
                }
                changed
            }
            serde_json::Value::Object(m) => {
                let mut changed = false;
                let keys: Vec<String> = m.keys().cloned().collect();
                for k in keys {
                    let r = self.redact_str(&k).into_owned();
                    if r != k {
                        if let Some(x) = m.remove(&k) {
                            m.insert(r, x);
                        }
                        changed = true;
                    }
                }
                for x in m.values_mut() {
                    changed |= self.redact_json(x);
                }
                changed
            }
            _ => false,
        }
    }

    /// Redact every string of a serializable value (through its JSON form;
    /// returned unchanged when nothing matches or it does not round-trip).
    pub fn redact<T: Serialize + DeserializeOwned>(&self, t: T) -> T {
        if self.is_empty() {
            return t;
        }
        let Ok(mut v) = serde_json::to_value(&t) else { return t };
        if !self.redact_json(&mut v) {
            return t;
        }
        serde_json::from_value(v).unwrap_or(t)
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// A [`SecretSource`] that registers every value it hands out.
pub struct RedactingSecrets {
    inner: Arc<dyn SecretSource>,
    redactor: Arc<Redactor>,
}

impl RedactingSecrets {
    pub fn new(inner: Arc<dyn SecretSource>, redactor: Arc<Redactor>) -> Self {
        RedactingSecrets { inner, redactor }
    }
}

impl SecretSource for RedactingSecrets {
    fn get(&self, name: &str) -> Option<String> {
        let v = self.inner.get(name)?;
        self.redactor.register(name, &v);
        Some(v)
    }
}

/// A [`BlobStore`] that redacts known secret values before storing.
pub struct RedactingBlobs {
    inner: Arc<dyn BlobStore>,
    redactor: Arc<Redactor>,
}

impl RedactingBlobs {
    pub fn new(inner: Arc<dyn BlobStore>, redactor: Arc<Redactor>) -> Self {
        RedactingBlobs { inner, redactor }
    }
}

#[async_trait]
impl BlobStore for RedactingBlobs {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError> {
        let bytes = self.redactor.redact_bytes(bytes);
        self.inner.put(&bytes, media_type).await
    }
    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
        self.inner.get(blob).await
    }
    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError> {
        self.inner.gc(reachable).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_strings_bytes_and_json() {
        let r = Redactor::new();
        assert!(r.is_empty());
        r.register("SHORT", "abc"); // too short
        r.register("TOKEN", "s3cr3t-token");
        r.register("TOKEN_PREFIX", "s3cr3t");
        assert_eq!(r.redact_str("x s3cr3t-token y s3cr3t"), "x [REDACTED:TOKEN] y [REDACTED:TOKEN_PREFIX]");
        assert!(matches!(r.redact_str("nothing abc"), Cow::Borrowed(_)));
        assert_eq!(&*r.redact_bytes(b"\x00s3cr3t-token\xff"), b"\x00[REDACTED:TOKEN]\xff");
        let mut v = serde_json::json!({"a": ["s3cr3t-token"], "s3cr3t": 1, "n": 2});
        assert!(r.redact_json(&mut v));
        assert_eq!(v, serde_json::json!({"a": ["[REDACTED:TOKEN]"], "[REDACTED:TOKEN_PREFIX]": 1, "n": 2}));
        let input = agent_proto::Input::Signal(agent_proto::Signal::Steer { text: "use s3cr3t-token".into() });
        let out = r.redact(input);
        assert_eq!(
            out,
            agent_proto::Input::Signal(agent_proto::Signal::Steer { text: "use [REDACTED:TOKEN]".into() })
        );
    }

    #[test]
    fn secrets_handed_out_are_registered() {
        struct One;
        impl SecretSource for One {
            fn get(&self, name: &str) -> Option<String> {
                (name == "K").then(|| "value-of-k".to_string())
            }
        }
        let r = Arc::new(Redactor::new());
        let s = RedactingSecrets::new(Arc::new(One), r.clone());
        assert_eq!(r.redact_str("value-of-k"), "value-of-k");
        assert_eq!(s.get("K").as_deref(), Some("value-of-k"));
        assert_eq!(s.get("missing"), None);
        assert_eq!(r.redact_str("value-of-k"), "[REDACTED:K]");
    }
}
