//! Test doubles for running tools outside the runtime, and the support behind
//! `#[agent_test]`.
//!
//! [`LocalSandbox`] runs commands directly with `tokio::process` (no isolation;
//! it reports `available: false`). Use it only in tests.

use agent_proto::{Access, BlobRef, CallId, SessionId};
use agent_runtime::{
    AccessCtx, BlobStore, ExecOutput, MemoryStore, SandboxPort, SandboxReport, SandboxSpec,
    SecretSource, StoreError, Tool, ToolCtx, ToolError, ToolOutput,
};
use agent_sim::VirtualClock;
use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// What an `#[agent_test]` provides: a temporary workspace, a framework data
/// directory next to it (shadow snapshots, locks, cursors), and a virtual
/// clock. Agents built inside the test default to all three.
#[derive(Debug, Clone)]
pub struct TestScope {
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    /// The time the runtime stamps on events. It starts at [`TEST_EPOCH_MS`]
    /// and only moves when the test advances it.
    pub clock: VirtualClock,
}

/// Start of the virtual clock (2026-01-01T00:00:00Z).
pub const TEST_EPOCH_MS: u64 = 1_767_225_600_000;

thread_local! {
    // The test body runs on a current-thread runtime: every task it spawns
    // (e.g. a run building its agent) runs on this thread.
    static SCOPE: RefCell<Option<TestScope>> = const { RefCell::new(None) };
}

/// The scope of the current `#[agent_test]`, `None` outside one.
pub fn scope() -> Option<TestScope> {
    SCOPE.with(|s| s.borrow().clone())
}

fn current() -> TestScope {
    scope().expect("agent_tools::testing called outside #[agent_test]")
}

/// The temporary workspace of the current `#[agent_test]`.
///
/// # Panics
/// Outside an `#[agent_test]`.
pub fn workspace() -> PathBuf {
    current().workspace
}

/// The virtual clock of the current `#[agent_test]` (advance it to move
/// the time agents see: turn budgets, snapshot intervals).
///
/// # Panics
/// Outside an `#[agent_test]`.
pub fn clock() -> VirtualClock {
    current().clock
}

/// Runs an `#[agent_test]` body: current-thread runtime, fresh temp
/// workspace and data directory, virtual clock.
pub fn run_test<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let dir = tempfile::tempdir().expect("create temp workspace");
    let root = dir.path().canonicalize().expect("canonicalize temp dir");
    let scope = TestScope {
        workspace: root.join("ws"),
        data_dir: root.join("data"),
        clock: VirtualClock::new(TEST_EPOCH_MS),
    };
    std::fs::create_dir_all(&scope.workspace).expect("create temp workspace");
    std::fs::create_dir_all(&scope.data_dir).expect("create temp data dir");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCOPE.with(|s| s.borrow_mut().take());
        }
    }
    SCOPE.with(|s| *s.borrow_mut() = Some(scope));
    let reset = Reset;
    let r = rt.block_on(f());
    drop(rt);
    drop(reset);
    drop(dir);
    r
}

/// In-memory content-addressed blobs.
#[derive(Default)]
pub struct MemBlobs(Mutex<BTreeMap<String, Vec<u8>>>);

#[async_trait]
impl BlobStore for MemBlobs {
    async fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<BlobRef, StoreError> {
        let sha256 = crate::caps::sha256_hex(bytes);
        self.0
            .lock()
            .unwrap()
            .insert(sha256.clone(), bytes.to_vec());
        Ok(BlobRef {
            sha256,
            size: bytes.len() as u64,
            media_type: media_type.map(str::to_string),
        })
    }
    async fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
        self.0
            .lock()
            .unwrap()
            .get(&blob.sha256)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(blob.sha256.clone()))
    }
    async fn gc(&self, reachable: &[BlobRef]) -> Result<usize, StoreError> {
        let mut m = self.0.lock().unwrap();
        let before = m.len();
        m.retain(|k, _| reachable.iter().any(|r| &r.sha256 == k));
        Ok(before - m.len())
    }
}

/// Secrets from a map.
#[derive(Default)]
pub struct MapSecrets(pub BTreeMap<String, String>);

impl SecretSource for MapSecrets {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// In-memory memory store; `recall` is a case-insensitive substring match on key
/// or value.
#[derive(Default)]
pub struct MemMemory(Mutex<BTreeMap<(String, String), String>>);

#[async_trait]
impl MemoryStore for MemMemory {
    async fn load(&self, scope: &str) -> Result<Vec<(String, String)>, StoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|((s, _), _)| s == scope)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect())
    }
    async fn recall(&self, scope: &str, query: &str) -> Result<Vec<(String, String)>, StoreError> {
        let q = query.to_lowercase();
        Ok(self
            .load(scope)
            .await?
            .into_iter()
            .filter(|(k, v)| k.to_lowercase().contains(&q) || v.to_lowercase().contains(&q))
            .collect())
    }
    async fn remember(
        &self,
        scope: &str,
        key: &str,
        value: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .insert((scope.into(), key.into()), value.into()))
    }
    async fn forget(&self, scope: &str, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .remove(&(scope.to_string(), key.to_string())))
    }
}

/// Runs commands directly (no isolation). Records the last spec it was given.
#[derive(Default)]
pub struct LocalSandbox {
    pub last_spec: Mutex<Option<SandboxSpec>>,
}

#[async_trait]
impl SandboxPort for LocalSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport {
            implementation: "none".into(),
            available: false,
            notes: vec!["test double: runs commands directly".into()],
            ..Default::default()
        }
    }
    async fn run(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        *self.last_spec.lock().unwrap() = Some(spec.clone());
        let (prog, args) = argv.split_first().ok_or("empty argv")?;
        let mut cmd = tokio::process::Command::new(prog);
        cmd.args(args)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(|e| format!("spawn {prog}: {e}"))?;
        let timeout = std::time::Duration::from_millis(if spec.timeout_ms == 0 {
            120_000
        } else {
            spec.timeout_ms
        });
        let wait = child.wait_with_output();
        tokio::select! {
            _ = cancel.cancelled() => Err("cancelled".into()),
            r = tokio::time::timeout(timeout, wait) => match r {
                Ok(Ok(o)) => Ok(ExecOutput { status: o.status.code(), stdout: o.stdout, stderr: o.stderr, timed_out: false, overlay_changes: vec![] }),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Ok(ExecOutput { timed_out: true, ..Default::default() }),
            },
        }
    }
}

/// A `ToolCtx` for tests with the given grants, in-memory blobs / memory, the
/// given secrets and a [`LocalSandbox`].
pub fn ctx(workspace: impl AsRef<Path>, grants: Vec<Access>) -> ToolCtx {
    ToolCtx {
        isolated: false,
        call_id: CallId::new("test-call"),
        session: SessionId::new("test-session"),
        workspace: workspace.as_ref().to_path_buf(),
        grants,
        cancel: CancellationToken::new(),
        blobs: Arc::new(MemBlobs::default()),
        sandbox: Arc::new(LocalSandbox::default()),
        memory: Some(Arc::new(MemMemory::default())),
        secrets: Arc::new(MapSecrets::default()),
        progress: Arc::new(|_| {}),
        subagents: None,
        tasks: None,
    }
}

/// Declare the tool's accesses for `input` and call it with exactly those grants.
pub async fn call_granted(
    tool: &dyn Tool,
    input: serde_json::Value,
    workspace: impl AsRef<Path>,
) -> Result<ToolOutput, ToolError> {
    let ws = workspace.as_ref();
    let grants = tool.access(
        &input,
        &AccessCtx {
            workspace: ws.to_path_buf(),
        },
    )?;
    tool.call(input, ctx(ws, grants)).await
}

/// Concatenated text content of an output (blob previews included).
pub fn text_of(out: &ToolOutput) -> String {
    out.content
        .iter()
        .map(|c| match c {
            agent_proto::ToolContent::Text { text } => text.clone(),
            agent_proto::ToolContent::Blob { preview, .. } => preview.clone(),
            agent_proto::ToolContent::Json { value } => value.to_string(),
            agent_proto::ToolContent::Image { blob } => format!("[image {}]", blob.sha256),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
