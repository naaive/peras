//! File-backed long-term memory: one JSON file (`{key: value}`) per scope.

use agent_runtime::{MemoryStore, StoreError};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

pub struct FileMemoryStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

fn io<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Io(e.to_string())
}

impl FileMemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io)?;
        Ok(FileMemoryStore { dir, lock: Mutex::new(()) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Readable, collision-free file name for a scope.
    fn file(&self, scope: &str) -> PathBuf {
        let safe: String = scope
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .take(64)
            .collect();
        let h = hex::encode(&Sha256::digest(scope.as_bytes())[..6]);
        self.dir.join(format!("{safe}-{h}.json"))
    }

    async fn read(&self, scope: &str) -> Result<BTreeMap<String, String>, StoreError> {
        match tokio::fs::read(self.file(scope)).await {
            Ok(b) => serde_json::from_slice(&b).map_err(io),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(io(e)),
        }
    }

    async fn write(&self, scope: &str, m: &BTreeMap<String, String>) -> Result<(), StoreError> {
        let path = self.file(scope);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(m).map_err(io)?;
        tokio::fs::write(&tmp, data).await.map_err(io)?;
        tokio::fs::rename(&tmp, &path).await.map_err(io)
    }
}

fn terms(q: &str) -> Vec<String> {
    q.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect()
}

/// Score: key hits count double; entries with no hit are dropped.
fn score(terms: &[String], k: &str, v: &str) -> usize {
    let (k, v) = (k.to_lowercase(), v.to_lowercase());
    terms.iter().map(|t| if k.contains(t.as_str()) { 2 } else { 0 } + usize::from(v.contains(t.as_str()))).sum()
}

#[async_trait]
impl MemoryStore for FileMemoryStore {
    async fn load(&self, scope: &str) -> Result<Vec<(String, String)>, StoreError> {
        let _g = self.lock.lock().await;
        Ok(self.read(scope).await?.into_iter().collect())
    }

    async fn recall(&self, scope: &str, query: &str) -> Result<Vec<(String, String)>, StoreError> {
        let _g = self.lock.lock().await;
        let m = self.read(scope).await?;
        let ts = terms(query);
        if ts.is_empty() {
            return Ok(m.into_iter().collect());
        }
        let mut hits: Vec<(usize, String, String)> =
            m.into_iter().map(|(k, v)| (score(&ts, &k, &v), k, v)).filter(|(s, _, _)| *s > 0).collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(hits.into_iter().map(|(_, k, v)| (k, v)).collect())
    }

    async fn remember(&self, scope: &str, key: &str, value: &str) -> Result<Option<String>, StoreError> {
        let _g = self.lock.lock().await;
        let mut m = self.read(scope).await?;
        let prev = m.insert(key.to_string(), value.to_string());
        self.write(scope, &m).await?;
        Ok(prev)
    }

    async fn forget(&self, scope: &str, key: &str) -> Result<Option<String>, StoreError> {
        let _g = self.lock.lock().await;
        let mut m = self.read(scope).await?;
        let prev = m.remove(key);
        if prev.is_some() {
            self.write(scope, &m).await?;
        }
        Ok(prev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_recall_forget() {
        let d = tempfile::tempdir().unwrap();
        let s = FileMemoryStore::new(d.path()).unwrap();
        assert_eq!(s.remember("user/a", "editor", "prefers vim").await.unwrap(), None);
        assert_eq!(s.remember("user/a", "editor", "prefers Helix").await.unwrap().as_deref(), Some("prefers vim"));
        s.remember("user/a", "tests", "run cargo test with helix feature").await.unwrap();
        s.remember("user/a", "style", "tabs").await.unwrap();
        let r = s.recall("user/a", "HELIX editor").await.unwrap();
        assert_eq!(r.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["editor", "tests"]);
        assert_eq!(s.load("user/b").await.unwrap(), vec![]);
        assert_eq!(s.forget("user/a", "style").await.unwrap().as_deref(), Some("tabs"));
        // Persisted across instances.
        let s2 = FileMemoryStore::new(d.path()).unwrap();
        assert_eq!(s2.load("user/a").await.unwrap().len(), 2);
    }
}
