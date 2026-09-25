//! Port traits: every piece of IO sits behind one of these.
//!
//! INTERFACE CONTRACT: adapters (`agent-adapters`), tools (`agent-tools`), the
//! simulator (`agent-sim`) and the SDK implement / consume these. Keep them stable;
//! additive changes only.

use agent_proto::*;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- model

/// A wire request produced by an encoder. Pure function of
/// `(SeqHead, [Rendered], encoder version)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub encoder_version: u32,
    /// Vendor JSON body.
    pub body: serde_json::Value,
    pub max_tokens: u32,
}

/// Encodes a sequence head and renderings into a wire request. Versioned and
/// frozen after release: a new version is only used for new sequences.
pub trait Encoder: Send + Sync {
    fn version(&self) -> u32;
    fn encode(&self, head: &SeqHead, body: &[Rendered], max_tokens: u32) -> Request;
}

/// Streaming deltas from a model port.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Thinking(String),
    ThinkingSignature(String),
    /// A tool-use block started.
    ToolUseStart { id: CallId, name: String },
    /// Partial JSON of the current tool-use input.
    ToolUseInput(String),
    /// The current tool-use block is complete.
    ToolUseEnd,
    Opaque { vendor: String, data: serde_json::Value },
    Usage(Usage),
    Stop(StopReason),
}

pub trait ModelPort: Send + Sync {
    fn caps(&self) -> &ModelCaps;
    fn encoder(&self) -> &dyn Encoder;
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>>;
}

// ---------------------------------------------------------------- storage

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Optimistic concurrency: expected seq did not match the stored head.
    #[error("seq conflict: expected {expected}, found {found}")]
    SeqConflict { expected: Seq, found: Seq },
    /// Lease generation is stale: another driver owns the session now.
    #[error("stale lease {held:?}, current {current:?}")]
    StaleLease { held: LeaseGen, current: LeaseGen },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("upgrade: {0}")]
    Upgrade(String),
    #[error("io: {0}")]
    Io(String),
}

/// Append-only journal storage.
#[async_trait]
pub trait JournalStore: Send + Sync {
    /// Take (or steal) the session lease; returns the new generation.
    async fn acquire_lease(&self, session: &SessionId) -> Result<LeaseGen, StoreError>;
    /// Append envelopes whose seqs start at `expected_next`. Fails with
    /// `SeqConflict` / `StaleLease` without writing anything.
    async fn append(
        &self,
        session: &SessionId,
        lease: LeaseGen,
        expected_next: Seq,
        events: &[Envelope<Event>],
    ) -> Result<(), StoreError>;
    /// Events with `seq >= from`, in order (upgraded on read).
    async fn load(&self, session: &SessionId, from: Seq) -> Result<Vec<Envelope<Event>>, StoreError>;
    /// Next seq to be written (0 for a new session).
    async fn next_seq(&self, session: &SessionId) -> Result<Seq, StoreError>;
    /// State snapshots: an optimisation, may be deleted at any time.
    async fn save_snapshot(&self, session: &SessionId, seq: Seq, state: Vec<u8>) -> Result<(), StoreError>;
    async fn load_snapshot(&self, session: &SessionId) -> Result<Option<(Seq, Vec<u8>)>, StoreError>;
    async fn list_sessions(&self) -> Result<Vec<SessionId>, StoreError>;
}

/// Content-addressed blob storage (write blob before the event referencing it).
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError>;
    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError>;
    /// Delete blobs not in `reachable`; returns how many were removed.
    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError>;
}

/// Long-term memory store. Every read and write is also journaled; replay never
/// touches this port.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Loaded into the Durable layer at session start.
    async fn load(&self, scope: &str) -> Result<Vec<(String, String)>, StoreError>;
    async fn recall(&self, scope: &str, query: &str) -> Result<Vec<(String, String)>, StoreError>;
    /// Returns the previous value (saved for rewind).
    async fn remember(&self, scope: &str, key: &str, value: &str) -> Result<Option<String>, StoreError>;
    async fn forget(&self, scope: &str, key: &str) -> Result<Option<String>, StoreError>;
}

// ---------------------------------------------------------------- sandbox

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxReport {
    /// Implementation chosen (`bubblewrap`, `landlock`, `seatbelt`, `none`).
    pub implementation: String,
    pub available: bool,
    /// Network can be restricted to the egress proxy (else: offline only).
    pub egress_proxy: bool,
    /// Overlay isolation available (Linux only).
    pub isolation: bool,
    pub notes: Vec<String>,
}

/// Sandbox profile compiled from the granted accesses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxSpec {
    pub cwd: PathBuf,
    pub readable: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    /// Reachable `host:port`s (through the egress proxy); empty = offline.
    pub network: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeout_ms: u64,
    /// Run in an overlay and return the change list instead of writing through.
    pub isolated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// For isolated runs: files the command changed (workspace-relative).
    pub overlay_changes: Vec<String>,
}

#[async_trait]
pub trait SandboxPort: Send + Sync {
    fn report(&self) -> SandboxReport;
    async fn run(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String>;
}

// ---------------------------------------------------------------- tools

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    /// Tool failure: returned to the model as an error result.
    #[error("{0}")]
    Failed(String),
    /// Input did not match the schema.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The tool tried to touch a resource it did not declare / was not granted.
    #[error("access not granted: {0}")]
    NotGranted(String),
    /// Read-hash mismatch: the file changed since it was read.
    #[error("stale: {0} changed since it was read")]
    Stale(String),
    #[error("cancelled")]
    Cancelled,
    /// Infrastructure error: propagated upward, not handed to the model.
    #[error("infrastructure: {0}")]
    Infra(String),
}

/// Output of a tool call before the runtime turns it into a `ToolResult`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolOutput {
    pub content: Vec<ToolContent>,
    /// `None` = derive from accesses (untrusted if any read was untrusted).
    pub trust: Option<Trust>,
    /// Content hashes observed while reading.
    pub observed: Vec<Access>,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput { content: vec![ToolContent::Text { text: s.into() }], ..Default::default() }
    }
}

/// Static context for computing a call's access declaration.
#[derive(Debug, Clone)]
pub struct AccessCtx {
    pub workspace: PathBuf,
}

/// Everything a running tool may use. Handles given to tools only reach the
/// granted accesses.
#[derive(Clone)]
pub struct ToolCtx {
    pub call_id: CallId,
    pub session: SessionId,
    pub workspace: PathBuf,
    pub grants: Vec<Access>,
    pub cancel: CancellationToken,
    pub blobs: Arc<dyn BlobStore>,
    pub sandbox: Arc<dyn SandboxPort>,
    pub memory: Option<Arc<dyn MemoryStore>>,
    /// Secrets are injected as handles, never enter the context.
    pub secrets: Arc<dyn SecretSource>,
    /// Progress messages go out as pulses.
    pub progress: Arc<dyn Fn(String) + Send + Sync>,
    /// For sub-agent tools: spawns a child session.
    pub subagents: Option<Arc<dyn SubagentSpawner>>,
}

pub trait SecretSource: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

/// A tool. Most tools are generated by `#[tool]`; tools whose access is only
/// known at runtime (bash, MCP) implement this by hand.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Declared accesses for this input. The same declaration drives scheduling,
    /// authorisation, sandboxing, rewind and taint.
    fn access(&self, input: &serde_json::Value, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError>;
    /// Side-effect class for this input (defaults to the spec's class).
    fn class(&self, _input: &serde_json::Value) -> EffectClass {
        self.spec().class
    }
    async fn call(&self, input: serde_json::Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Spawns sub-agent sessions (implemented by the runtime / sdk).
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    /// Run a child session to completion and return its final outcome and whether
    /// its output is tainted.
    async fn run_child(
        &self,
        child: SessionId,
        agent: &str,
        task: String,
        tainted_input: bool,
    ) -> Result<(TurnOutcome, bool), ToolError>;
}

// ---------------------------------------------------------------- gates

/// Per-dispatch context handed to gate executors by the driver (ADDITIVE).
#[derive(Clone)]
pub struct GateCtx {
    pub session: SessionId,
    /// The session's pending-question board (clients answer through it,
    /// compare-and-swap: first answer wins).
    pub asks: Arc<crate::gate::AskBoard>,
    /// Cancelled on hard interrupt.
    pub cancel: CancellationToken,
}

/// Hook / auto-rule / human answer executors (rings 4 and 5).
#[async_trait]
pub trait GateExecutor: Send + Sync {
    async fn evaluate(&self, req: &GateRequest) -> (Verdict, Responder);
    /// Called by the driver. Defaults to [`GateExecutor::evaluate`]; executors
    /// that need the session's question board (e.g. [`crate::gate::GateChain`])
    /// override it. (ADDITIVE)
    async fn evaluate_in(&self, req: &GateRequest, _ctx: &GateCtx) -> (Verdict, Responder) {
        self.evaluate(req).await
    }
    /// Called by the driver. Defaults to [`GateExecutor::evaluate_in`] with
    /// `remember: false`; executors that surface human answers override it to
    /// carry `Answer::Allow { remember }`. (ADDITIVE)
    async fn evaluate_outcome(&self, req: &GateRequest, ctx: &GateCtx) -> GateOutcome {
        let (verdict, responder) = self.evaluate_in(req, ctx).await;
        GateOutcome::new(verdict, responder)
    }
}

/// An observer: event-stream subscriber with its own cursor. At-least-once
/// delivery; the event id is the idempotency key. Never affects execution.
#[async_trait]
pub trait Observer: Send + Sync {
    fn name(&self) -> &str;
    async fn on_event(&self, session: &SessionId, ev: &Envelope<Event>) -> Result<(), String>;
}

// ---------------------------------------------------------------- checkpoints

/// Shadow snapshot store, separate from the user's `.git`.
#[async_trait]
pub trait Checkpointer: Send + Sync {
    async fn checkpoint(&self, scope: &CheckpointScope) -> Result<CheckpointInfo, String>;
    /// Save originals of declared writes before a batch executes.
    async fn save_originals(&self, writes: &[Access]) -> Result<(), String>;
    /// Undo agent-attributed changes back to `plan.checkpoint`. Idempotent:
    /// re-running the same plan after a crash converges.
    async fn restore(&self, plan: &RestorePlan) -> Result<RestoreReport, String>;
    /// Crash recovery of a `LocalWrite` call: put back the originals saved by
    /// [`Checkpointer::save_originals`] for these writes before re-running.
    /// Default: no-op (best effort re-run). (ADDITIVE)
    async fn restore_originals(&self, _writes: &[Access]) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------- clock / ids

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub trait IdGen: Send + Sync {
    fn event_id(&self, at: Timestamp) -> EventId;
}

// ---------------------------------------------------------------- names

/// Anything that names a tool: `&str`, `String`, and the unit structs generated
/// by `#[tool]` (so scripted models can say `.call(edit, json!(..))`).
pub trait ToolName {
    fn tool_name(&self) -> String;
}

impl ToolName for str {
    fn tool_name(&self) -> String {
        self.to_string()
    }
}

impl ToolName for String {
    fn tool_name(&self) -> String {
        self.clone()
    }
}

impl<T: ToolName + ?Sized> ToolName for &T {
    fn tool_name(&self) -> String {
        (**self).tool_name()
    }
}

// ---------------------------------------------------------------- gate outcome (ADDITIVE)

/// Full result of a gate evaluation: the verdict, who gave it, and whether the
/// human chose "allow this destination for the rest of the session"
/// (`Answer::Allow { remember: true }`). Carried into
/// `EffectResult::Gated { remember }`.
#[derive(Debug, Clone, PartialEq)]
pub struct GateOutcome {
    pub verdict: Verdict,
    pub responder: Responder,
    pub remember: bool,
}

impl GateOutcome {
    pub fn new(verdict: Verdict, responder: Responder) -> Self {
        GateOutcome { verdict, responder, remember: false }
    }
}

// ---------------------------------------------------------------- state snapshots (ADDITIVE)

/// Serialization contract for a decider's state, supplied to the runtime
/// builder when the state can be snapshotted. Snapshots are only a cache:
/// a snapshot that fails to decode is ignored and the journal is folded from
/// the start instead.
pub trait StateCodec<S>: Send + Sync {
    fn encode(&self, state: &S) -> Result<Vec<u8>, String>;
    fn decode(&self, bytes: &[u8]) -> Result<S, String>;
}

// ---------------------------------------------------------------- request verification (ADDITIVE)

/// Rebuilds, from the journal alone, the prompt the kernel sent with a
/// `Sample` effect. Used by the debug request-consistency check
/// (`RuntimeOptions::verify_requests`): the rebuilt and the actual prompt are
/// both encoded with the model's encoder and must be byte-identical.
pub trait PromptRebuilder: Send + Sync {
    /// `events` is the session's journal up to and including the
    /// `EffectIssued` event of `effect`.
    fn rebuild(&self, events: &[Envelope<Event>], effect: EffectId) -> Result<Prompt, String>;
}

// ---------------------------------------------------------------- observer cursors (ADDITIVE)

/// Persisted per-(session, observer) delivery cursors, so observers resume
/// where they stopped instead of replaying the whole journal after a resume.
/// A cursor is the next seq to deliver (everything below it was delivered).
#[async_trait]
pub trait ObserverCursors: Send + Sync {
    async fn load(&self, session: &SessionId, observer: &str) -> Result<Option<Seq>, StoreError>;
    async fn save(&self, session: &SessionId, observer: &str, next: Seq) -> Result<(), StoreError>;
}
