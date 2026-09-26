//! In-memory / trivial port implementations for tests, the simulator and
//! quick embedding: journal, blobs, clocks, id generators, secrets, sandboxes,
//! a no-op checkpointer and a model port that is always unavailable.

use crate::ports::*;
use agent_proto::*;
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- journal

#[derive(Default)]
struct MemLog {
    lease: LeaseGen,
    events: Vec<Envelope<Event>>,
    snapshot: Option<(Seq, Vec<u8>)>,
}

/// In-memory [`JournalStore`] with lease-generation fencing and optimistic
/// seq checks. Envelopes are round-tripped through JSON (and the read-time
/// upgrader) so serialization bugs surface in tests.
#[derive(Default)]
pub struct MemJournal {
    logs: Mutex<BTreeMap<SessionId, MemLog>>,
    /// Round-trip envelopes through JSON on append (default true).
    json_roundtrip: bool,
}

impl MemJournal {
    pub fn new() -> Self {
        MemJournal { logs: Mutex::default(), json_roundtrip: true }
    }
    /// Skip the JSON round-trip (faster; for benchmarks).
    pub fn without_roundtrip() -> Self {
        MemJournal { logs: Mutex::default(), json_roundtrip: false }
    }
    /// Current lease generation of a session (tests).
    pub fn lease(&self, session: &SessionId) -> LeaseGen {
        self.logs.lock().unwrap().get(session).map(|l| l.lease).unwrap_or_default()
    }
    /// Number of stored events (tests).
    pub fn len(&self, session: &SessionId) -> usize {
        self.logs.lock().unwrap().get(session).map(|l| l.events.len()).unwrap_or(0)
    }
    pub fn is_empty(&self, session: &SessionId) -> bool {
        self.len(session) == 0
    }
    /// Synchronous snapshot of all stored events (tests).
    pub fn events(&self, session: &SessionId) -> Vec<Envelope<Event>> {
        self.logs.lock().unwrap().get(session).map(|l| l.events.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl JournalStore for MemJournal {
    async fn acquire_lease(&self, session: &SessionId) -> Result<LeaseGen, StoreError> {
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(session.clone()).or_default();
        log.lease = LeaseGen(log.lease.0 + 1);
        Ok(log.lease)
    }

    async fn append(
        &self,
        session: &SessionId,
        lease: LeaseGen,
        expected_next: Seq,
        events: &[Envelope<Event>],
    ) -> Result<(), StoreError> {
        let mut prepared = Vec::with_capacity(events.len());
        for (i, e) in events.iter().enumerate() {
            if e.seq != expected_next + i as u64 {
                return Err(StoreError::Io(format!(
                    "non-consecutive seq {} at offset {i} (expected {})",
                    e.seq,
                    expected_next + i as u64
                )));
            }
            if self.json_roundtrip {
                let v = serde_json::to_value(e).map_err(|x| StoreError::Io(x.to_string()))?;
                match upgrade::read_envelope(v).map_err(|x| StoreError::Upgrade(x.to_string()))? {
                    Some(back) => prepared.push(back),
                    None => prepared.push(e.clone()),
                }
            } else {
                prepared.push(e.clone());
            }
        }
        let mut logs = self.logs.lock().unwrap();
        let log = logs.entry(session.clone()).or_default();
        if lease != log.lease {
            return Err(StoreError::StaleLease { held: lease, current: log.lease });
        }
        let found = log.events.len() as u64;
        if expected_next != found {
            return Err(StoreError::SeqConflict { expected: expected_next, found });
        }
        log.events.extend(prepared);
        Ok(())
    }

    async fn load(&self, session: &SessionId, from: Seq) -> Result<Vec<Envelope<Event>>, StoreError> {
        let logs = self.logs.lock().unwrap();
        Ok(logs
            .get(session)
            .map(|l| l.events.iter().skip(from as usize).cloned().collect())
            .unwrap_or_default())
    }

    async fn next_seq(&self, session: &SessionId) -> Result<Seq, StoreError> {
        Ok(self.len(session) as u64)
    }

    async fn save_snapshot(&self, session: &SessionId, seq: Seq, state: Vec<u8>) -> Result<(), StoreError> {
        let mut logs = self.logs.lock().unwrap();
        logs.entry(session.clone()).or_default().snapshot = Some((seq, state));
        Ok(())
    }

    async fn load_snapshot(&self, session: &SessionId) -> Result<Option<(Seq, Vec<u8>)>, StoreError> {
        Ok(self.logs.lock().unwrap().get(session).and_then(|l| l.snapshot.clone()))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        Ok(self
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| !l.events.is_empty())
            .map(|(k, _)| k.clone())
            .collect())
    }
}

// ---------------------------------------------------------------- blobs

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// In-memory content-addressed [`BlobStore`] (sha256).
#[derive(Default)]
pub struct MemBlobStore {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl BlobStore for MemBlobStore {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError> {
        let sha = sha256_hex(bytes);
        self.blobs.lock().unwrap().entry(sha.clone()).or_insert_with(|| bytes.to_vec());
        Ok(BlobRef { sha256: sha, size: bytes.len() as u64, media_type: media_type.map(str::to_string) })
    }
    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
        self.blobs
            .lock()
            .unwrap()
            .get(&blob.sha256)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(blob.sha256.clone()))
    }
    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError> {
        let keep: std::collections::BTreeSet<&str> = reachable.iter().map(|b| b.sha256.as_str()).collect();
        let mut blobs = self.blobs.lock().unwrap();
        let before = blobs.len();
        blobs.retain(|k, _| keep.contains(k.as_str()));
        Ok(before - blobs.len())
    }
}

// ---------------------------------------------------------------- clock / ids

/// Wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        Timestamp(ms)
    }
}

/// A clock that only moves when told to (tests / simulation).
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        ManualClock(AtomicU64::new(start_ms))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

/// Monotonic ULID generator: the time part comes from the injected timestamp,
/// the random part increments within the same millisecond.
pub struct UlidGen {
    last: Mutex<(u64, u128)>,
}

impl Default for UlidGen {
    fn default() -> Self {
        UlidGen { last: Mutex::new((0, 0)) }
    }
}

impl UlidGen {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGen for UlidGen {
    fn event_id(&self, at: Timestamp) -> EventId {
        let mut last = self.last.lock().unwrap();
        let ms = at.0.max(last.0);
        let random = if ms == last.0 && last.1 != 0 {
            last.1.wrapping_add(1) & ((1u128 << 80) - 1)
        } else {
            ulid::Ulid::new().random() >> 1 // leave headroom for increments
        };
        *last = (ms, random.max(1));
        EventId(ulid::Ulid::from_parts(ms, last.1).to_string())
    }
}

/// Deterministic ids for tests: `ev-000000000001`, `ev-000000000002`, ...
#[derive(Default)]
pub struct SeqIdGen(AtomicU64);

impl SeqIdGen {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGen for SeqIdGen {
    fn event_id(&self, _at: Timestamp) -> EventId {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        EventId(format!("ev-{n:012}"))
    }
}

// ---------------------------------------------------------------- secrets

/// Secrets from environment variables, optionally with a prefix
/// (`EnvSecrets::with_prefix("AGENT_SECRET_")` maps `GITHUB_TOKEN` to
/// `$AGENT_SECRET_GITHUB_TOKEN`).
#[derive(Debug, Default, Clone)]
pub struct EnvSecrets {
    prefix: String,
}

impl EnvSecrets {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        EnvSecrets { prefix: prefix.into() }
    }
}

impl SecretSource for EnvSecrets {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(format!("{}{name}", self.prefix)).ok()
    }
}

/// Fixed secret map (tests).
#[derive(Debug, Default, Clone)]
pub struct MapSecrets(pub BTreeMap<String, String>);

impl SecretSource for MapSecrets {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

// ---------------------------------------------------------------- sandboxes

/// Runs nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSandbox;

#[async_trait]
impl SandboxPort for NullSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport { implementation: "null".into(), available: false, ..Default::default() }
    }
    async fn run(&self, argv: &[String], _spec: &SandboxSpec, _c: CancellationToken) -> Result<ExecOutput, String> {
        Err(format!("no sandbox configured; refusing to run {argv:?}"))
    }
}

/// Runs argv directly with `tokio::process`, WITHOUT isolation. Reports
/// `available = false` so the kernel treats every command as Opaque.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectSandbox;

#[async_trait]
impl SandboxPort for DirectSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport {
            implementation: "none".into(),
            available: false,
            egress_proxy: false,
            isolation: false,
            notes: vec!["direct execution without isolation".into()],
        }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        let (prog, args) = argv.split_first().ok_or("empty argv")?;
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args).kill_on_drop(true);
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        let child = cmd.output();
        let timeout = if spec.timeout_ms == 0 { Duration::from_secs(24 * 3600) } else { Duration::from_millis(spec.timeout_ms) };
        tokio::select! {
            _ = cancel.cancelled() => Err("cancelled".into()),
            r = tokio::time::timeout(timeout, child) => match r {
                Err(_) => Ok(ExecOutput { timed_out: true, ..Default::default() }),
                Ok(Err(e)) => Err(e.to_string()),
                Ok(Ok(out)) => Ok(ExecOutput {
                    status: out.status.code(),
                    stdout: out.stdout,
                    stderr: out.stderr,
                    timed_out: false,
                    overlay_changes: vec![],
                }),
            }
        }
    }
}

// ---------------------------------------------------------------- checkpointer

/// Checkpointer that records nothing (no workspace).
#[derive(Debug, Default)]
pub struct NullCheckpointer(AtomicU64);

#[async_trait]
impl Checkpointer for NullCheckpointer {
    async fn checkpoint(&self, _scope: &CheckpointScope) -> Result<CheckpointInfo, String> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(CheckpointInfo { id: CheckpointId(format!("null-{n}")), agent_changes: vec![], external_changes: vec![] })
    }
    async fn save_originals(&self, _writes: &[Access]) -> Result<(), String> {
        Ok(())
    }
    async fn restore(&self, _plan: &RestorePlan) -> Result<RestoreReport, String> {
        Ok(RestoreReport::default())
    }
}

// ---------------------------------------------------------------- model

struct NullEncoder;
impl Encoder for NullEncoder {
    fn version(&self) -> u32 {
        0
    }
    fn encode(&self, _head: &SeqHead, _body: &[Rendered], max_tokens: u32) -> Request {
        Request { encoder_version: 0, body: serde_json::Value::Null, max_tokens }
    }
}

/// Default model port when none is configured: every request fails with
/// `ModelError::Unavailable`.
pub struct NoModel {
    caps: ModelCaps,
}

impl Default for NoModel {
    fn default() -> Self {
        NoModel { caps: ModelCaps { model: ModelId::new("none"), ..ModelCaps::default() } }
    }
}

impl ModelPort for NoModel {
    fn caps(&self) -> &ModelCaps {
        &self.caps
    }
    fn encoder(&self) -> &dyn Encoder {
        &NullEncoder
    }
    fn stream(&self, _req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        stream::once(async { Err(ModelError::Unavailable { message: "no model configured".into() }) }).boxed()
    }
}
