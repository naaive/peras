//! SQLite journal (WAL, fsync on commit) and blob store sharing one database.

use super::blob_ref;
use agent_proto::upgrade::read_envelope;
use agent_proto::*;
use agent_runtime::{BlobStore, JournalStore, StoreError};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

type Db = Arc<Mutex<Connection>>;

fn io<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Io(e.to_string())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    session TEXT NOT NULL,
    seq     INTEGER NOT NULL,
    json    TEXT NOT NULL,
    PRIMARY KEY (session, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS leases (
    session TEXT PRIMARY KEY,
    gen     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshots (
    session TEXT PRIMARY KEY,
    seq     INTEGER NOT NULL,
    state   BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS blobs (
    sha256     TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    media_type TEXT,
    bytes      BLOB NOT NULL
);
";

fn init(conn: &Connection) -> Result<(), StoreError> {
    // WAL + synchronous=FULL: every committed append is fsynced.
    conn.pragma_update(None, "journal_mode", "WAL").map_err(io)?;
    conn.pragma_update(None, "synchronous", "FULL").map_err(io)?;
    conn.pragma_update(None, "foreign_keys", "ON").map_err(io)?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(io)?;
    conn.execute_batch(SCHEMA).map_err(io)
}

async fn blocking<R: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
) -> Result<R, StoreError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        let mut c = db.lock().map_err(|_| StoreError::Io("db mutex poisoned".into()))?;
        f(&mut c)
    })
    .await
    .map_err(io)?
}

/// One SQLite database holding a journal and blobs.
#[derive(Clone)]
pub struct Sqlite {
    pub journal: Arc<SqliteJournal>,
    pub blobs: Arc<SqliteBlobStore>,
}

impl Sqlite {
    pub fn open(path: impl AsRef<Path>) -> Result<Sqlite, StoreError> {
        Self::from_conn(Connection::open(path).map_err(io)?)
    }
    pub fn in_memory() -> Result<Sqlite, StoreError> {
        Self::from_conn(Connection::open_in_memory().map_err(io)?)
    }
    fn from_conn(c: Connection) -> Result<Sqlite, StoreError> {
        init(&c)?;
        let db: Db = Arc::new(Mutex::new(c));
        Ok(Sqlite { journal: Arc::new(SqliteJournal { db: db.clone() }), blobs: Arc::new(SqliteBlobStore { db }) })
    }
}

/// Append-only journal in SQLite.
pub struct SqliteJournal {
    db: Db,
}

impl SqliteJournal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let s = Sqlite::open(path)?;
        Ok(SqliteJournal { db: s.journal.db.clone() })
    }
}

fn current_gen(tx: &Connection, session: &str) -> Result<LeaseGen, StoreError> {
    let g: Option<i64> = tx
        .query_row("SELECT gen FROM leases WHERE session = ?1", [session], |r| r.get(0))
        .optional()
        .map_err(io)?;
    Ok(LeaseGen(g.unwrap_or(0) as u64))
}

fn next_seq_of(c: &Connection, session: &str) -> Result<Seq, StoreError> {
    let m: Option<i64> =
        c.query_row("SELECT MAX(seq) FROM events WHERE session = ?1", [session], |r| r.get(0)).map_err(io)?;
    Ok(m.map(|m| m as u64 + 1).unwrap_or(0))
}

#[async_trait]
impl JournalStore for SqliteJournal {
    async fn acquire_lease(&self, session: &SessionId) -> Result<LeaseGen, StoreError> {
        let s = session.0.clone();
        blocking(&self.db, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(io)?;
            let g = current_gen(&tx, &s)?.0 + 1;
            tx.execute(
                "INSERT INTO leases(session, gen) VALUES (?1, ?2) ON CONFLICT(session) DO UPDATE SET gen = excluded.gen",
                params![s, g as i64],
            )
            .map_err(io)?;
            tx.commit().map_err(io)?;
            Ok(LeaseGen(g))
        })
        .await
    }

    async fn append(
        &self,
        session: &SessionId,
        lease: LeaseGen,
        expected_next: Seq,
        events: &[Envelope<Event>],
    ) -> Result<(), StoreError> {
        let s = session.0.clone();
        let rows: Vec<(u64, String)> = events
            .iter()
            .map(|e| serde_json::to_string(e).map(|j| (e.seq, j)))
            .collect::<Result<_, _>>()
            .map_err(io)?;
        blocking(&self.db, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(io)?;
            let current = current_gen(&tx, &s)?;
            if current != lease {
                return Err(StoreError::StaleLease { held: lease, current });
            }
            let found = next_seq_of(&tx, &s)?;
            if found != expected_next {
                return Err(StoreError::SeqConflict { expected: expected_next, found });
            }
            {
                let mut st = tx.prepare_cached("INSERT INTO events(session, seq, json) VALUES (?1, ?2, ?3)").map_err(io)?;
                for (i, (seq, json)) in rows.iter().enumerate() {
                    let want = expected_next + i as u64;
                    if *seq != want {
                        return Err(StoreError::SeqConflict { expected: want, found: *seq });
                    }
                    st.execute(params![s, *seq as i64, json]).map_err(io)?;
                }
            }
            tx.commit().map_err(io)
        })
        .await
    }

    async fn load(&self, session: &SessionId, from: Seq) -> Result<Vec<Envelope<Event>>, StoreError> {
        let s = session.0.clone();
        let raws: Vec<String> = blocking(&self.db, move |c| {
            let mut st = c.prepare_cached("SELECT json FROM events WHERE session = ?1 AND seq >= ?2 ORDER BY seq").map_err(io)?;
            let rows = st.query_map(params![s, from as i64], |r| r.get::<_, String>(0)).map_err(io)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(io)
        })
        .await?;
        let mut out = Vec::with_capacity(raws.len());
        for raw in raws {
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| StoreError::Upgrade(e.to_string()))?;
            if let Some(e) = read_envelope(v).map_err(|e| StoreError::Upgrade(e.to_string()))? {
                out.push(e);
            }
        }
        Ok(out)
    }

    async fn next_seq(&self, session: &SessionId) -> Result<Seq, StoreError> {
        let s = session.0.clone();
        blocking(&self.db, move |c| next_seq_of(c, &s)).await
    }

    async fn save_snapshot(&self, session: &SessionId, seq: Seq, state: Vec<u8>) -> Result<(), StoreError> {
        let s = session.0.clone();
        blocking(&self.db, move |c| {
            c.execute(
                "INSERT INTO snapshots(session, seq, state) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session) DO UPDATE SET seq = excluded.seq, state = excluded.state",
                params![s, seq as i64, state],
            )
            .map_err(io)?;
            Ok(())
        })
        .await
    }

    async fn load_snapshot(&self, session: &SessionId) -> Result<Option<(Seq, Vec<u8>)>, StoreError> {
        let s = session.0.clone();
        blocking(&self.db, move |c| {
            c.query_row("SELECT seq, state FROM snapshots WHERE session = ?1", [s], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
            })
            .optional()
            .map_err(io)
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        blocking(&self.db, move |c| {
            let mut st = c
                .prepare("SELECT DISTINCT session FROM events UNION SELECT session FROM leases ORDER BY 1")
                .map_err(io)?;
            let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(io)?;
            rows.map(|r| r.map(SessionId)).collect::<Result<Vec<_>, _>>().map_err(io)
        })
        .await
    }
}

/// Content-addressed blobs in the same SQLite database.
pub struct SqliteBlobStore {
    db: Db,
}

#[async_trait]
impl BlobStore for SqliteBlobStore {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError> {
        let r = blob_ref(bytes, media_type);
        let (sha, size, mt, data) = (r.sha256.clone(), r.size, r.media_type.clone(), bytes.to_vec());
        blocking(&self.db, move |c| {
            c.execute(
                "INSERT OR IGNORE INTO blobs(sha256, size, media_type, bytes) VALUES (?1, ?2, ?3, ?4)",
                params![sha, size as i64, mt, data],
            )
            .map_err(io)?;
            Ok(())
        })
        .await?;
        Ok(r)
    }

    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
        let sha = blob.sha256.clone();
        blocking(&self.db, move |c| {
            c.query_row("SELECT bytes FROM blobs WHERE sha256 = ?1", [&sha], |r| r.get::<_, Vec<u8>>(0))
                .optional()
                .map_err(io)?
                .ok_or_else(|| StoreError::NotFound(format!("blob {sha}")))
        })
        .await
    }

    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError> {
        let keep: BTreeSet<String> = reachable.iter().map(|b| b.sha256.clone()).collect();
        blocking(&self.db, move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(io)?;
            let all: Vec<String> = {
                let mut st = tx.prepare("SELECT sha256 FROM blobs").map_err(io)?;
                let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(io)?;
                rows.collect::<Result<_, _>>().map_err(io)?
            };
            let mut n = 0;
            for sha in all.iter().filter(|s| !keep.contains(*s)) {
                n += tx.execute("DELETE FROM blobs WHERE sha256 = ?1", [sha]).map_err(io)?;
            }
            tx.commit().map_err(io)?;
            Ok(n)
        })
        .await
    }
}
