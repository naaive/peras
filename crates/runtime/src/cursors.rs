//! [`ObserverCursors`] implementations: in-memory and a JSON file.

use crate::ports::{ObserverCursors, StoreError};
use agent_proto::{Seq, SessionId};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type Table = BTreeMap<String, BTreeMap<String, Seq>>;

/// In-memory cursors: observers skip replay when a session is closed and
/// resumed within the same process.
#[derive(Debug, Default)]
pub struct MemCursors {
    table: Mutex<Table>,
}

impl MemCursors {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ObserverCursors for MemCursors {
    async fn load(&self, session: &SessionId, observer: &str) -> Result<Option<Seq>, StoreError> {
        Ok(self.table.lock().unwrap().get(&session.0).and_then(|m| m.get(observer)).copied())
    }
    async fn save(&self, session: &SessionId, observer: &str, next: Seq) -> Result<(), StoreError> {
        self.table.lock().unwrap().entry(session.0.clone()).or_default().insert(observer.to_string(), next);
        Ok(())
    }
}

/// Cursors persisted as one JSON document
/// (`{"<session>": {"<observer>": <next seq>}}`), rewritten atomically
/// (temp file + rename) on every change. Cursors only move forward.
#[derive(Debug)]
pub struct FileCursors {
    path: PathBuf,
    table: Mutex<Table>,
}

fn io<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Io(e.to_string())
}

impl FileCursors {
    /// Opens (or starts) the cursor file at `path`. An unreadable / corrupt
    /// file is an error; a missing one starts empty.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let table = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| StoreError::Io(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Table::new(),
            Err(e) => return Err(io(e)),
        };
        Ok(FileCursors { path, table: Mutex::new(table) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, table: &Table) -> Result<(), StoreError> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(table).map_err(io)?).map_err(io)?;
        std::fs::rename(&tmp, &self.path).map_err(io)
    }
}

#[async_trait]
impl ObserverCursors for FileCursors {
    async fn load(&self, session: &SessionId, observer: &str) -> Result<Option<Seq>, StoreError> {
        Ok(self.table.lock().unwrap().get(&session.0).and_then(|m| m.get(observer)).copied())
    }
    async fn save(&self, session: &SessionId, observer: &str, next: Seq) -> Result<(), StoreError> {
        let mut table = self.table.lock().unwrap();
        let slot = table.entry(session.0.clone()).or_default().entry(observer.to_string()).or_insert(0);
        if *slot >= next && next != 0 {
            return Ok(());
        }
        *slot = next;
        self.write(&table)
    }
}
