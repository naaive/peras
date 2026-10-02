//! Background task registry: long-running commands, async sub-agents, timers.
//! List, kill, timeout; output stored as a blob. Tasks may belong to a
//! session: when one finishes, the registry's notifier is told (the runtime
//! delivers it to the owning session as a notification). A task that declared
//! writes ([`TaskRegistry::set_writes`]) changes the workspace while it runs:
//! the checkpointer counts changes there as the agent's, and a rewind stops
//! the task first. Killing or timing out a task cancels its token and gives
//! it a short grace to stop (e.g. kill its process group) before dropping it.

use crate::ports::BlobStore;
use agent_proto::{Access, BlobRef, SessionId, Trust};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub type TaskId = u64;

/// How long a killed or timed-out task may take to stop after its token is
/// cancelled before it is dropped.
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Done { output: Option<BlobRef> },
    Failed { error: String },
    Killed,
    TimedOut,
}

impl TaskStatus {
    pub fn is_finished(&self) -> bool {
        !matches!(self, TaskStatus::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    pub id: TaskId,
    pub name: String,
    pub status: TaskStatus,
    /// The session that started the task, if any.
    pub owner: Option<SessionId>,
}

/// Called once per finished task that has an owner.
pub type TaskNotifier = Arc<dyn Fn(&TaskInfo) + Send + Sync>;

struct Entry {
    name: String,
    owner: Option<SessionId>,
    /// Trust of the output (e.g. a sub-agent whose context was tainted).
    trust: Option<Trust>,
    /// Declared writes of the task (its changes are the agent's while it
    /// runs; a rewind stops it first).
    writes: Vec<Access>,
    cancel: CancellationToken,
    status: watch::Sender<TaskStatus>,
}

impl Entry {
    fn info(&self, id: TaskId) -> TaskInfo {
        TaskInfo { id, name: self.name.clone(), status: self.status.borrow().clone(), owner: self.owner.clone() }
    }
}

/// One-line description of a task's state (used in notifications and by the
/// task tools).
pub fn describe(t: &TaskInfo) -> String {
    let status = match &t.status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Done { output: Some(b) } => format!("done (output: {} bytes, read it with task_output)", b.size),
        TaskStatus::Done { output: None } => "done (no output)".to_string(),
        TaskStatus::Failed { error } => format!("failed: {error}"),
        TaskStatus::Killed => "killed".to_string(),
        TaskStatus::TimedOut => "timed out".to_string(),
    };
    format!("Background task {} ({}): {status}", t.id, t.name)
}

pub struct TaskRegistry {
    blobs: Arc<dyn BlobStore>,
    tasks: Arc<Mutex<BTreeMap<TaskId, Entry>>>,
    next: Mutex<TaskId>,
    notifier: Arc<Mutex<Option<TaskNotifier>>>,
}

impl TaskRegistry {
    pub fn new(blobs: Arc<dyn BlobStore>) -> Self {
        TaskRegistry { blobs, tasks: Arc::default(), next: Mutex::new(1), notifier: Arc::default() }
    }

    /// Install the callback told about finished tasks that have an owner.
    pub fn set_notifier(&self, n: TaskNotifier) {
        *self.notifier.lock().unwrap() = Some(n);
    }

    /// The blob store task outputs are written to.
    pub fn blobs(&self) -> &Arc<dyn BlobStore> {
        &self.blobs
    }

    /// Spawn a task. `f` gets a cancellation token and returns its output
    /// bytes (stored as a blob) or an error.
    pub fn spawn<F, Fut>(&self, name: impl Into<String>, timeout: Option<Duration>, f: F) -> TaskId
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>> + Send + 'static,
    {
        self.spawn_for(None, name, timeout, f)
    }

    /// Spawn a task owned by `owner` (listed for it, its end notified to it).
    pub fn spawn_for<F, Fut>(&self, owner: Option<SessionId>, name: impl Into<String>, timeout: Option<Duration>, f: F) -> TaskId
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, String>> + Send + 'static,
    {
        let id = {
            let mut n = self.next.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };
        let cancel = CancellationToken::new();
        let (status, _) = watch::channel(TaskStatus::Running);
        let fut = f(cancel.clone());
        let name = name.into();
        self.tasks.lock().unwrap().insert(
            id,
            Entry {
                name: name.clone(),
                owner: owner.clone(),
                trust: None,
                writes: vec![],
                cancel: cancel.clone(),
                status: status.clone(),
            },
        );
        let blobs = self.blobs.clone();
        let notifier = self.notifier.clone();
        tokio::spawn(async move {
            let timeout = timeout.unwrap_or(Duration::from_secs(365 * 24 * 3600));
            let mut fut = std::pin::pin!(fut);
            let deadline = tokio::time::sleep(timeout);
            let r = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Let the task observe its token (e.g. kill a process
                    // group) before it is dropped.
                    let _ = tokio::time::timeout(STOP_GRACE, &mut fut).await;
                    TaskStatus::Killed
                }
                _ = deadline => {
                    cancel.cancel();
                    let _ = tokio::time::timeout(STOP_GRACE, &mut fut).await;
                    TaskStatus::TimedOut
                }
                r = &mut fut => match r {
                    Err(e) => TaskStatus::Failed { error: e },
                    Ok(bytes) if bytes.is_empty() => TaskStatus::Done { output: None },
                    Ok(bytes) => match blobs.put(&bytes, Some("application/octet-stream")).await {
                        Ok(b) => TaskStatus::Done { output: Some(b) },
                        Err(e) => TaskStatus::Failed { error: format!("storing output: {e}") },
                    },
                }
            };
            status.send_replace(r.clone());
            if owner.is_some() {
                let n = notifier.lock().unwrap().clone();
                if let Some(n) = n {
                    n(&TaskInfo { id, name, status: r, owner });
                }
            }
        });
        id
    }

    pub fn list(&self) -> Vec<TaskInfo> {
        self.tasks.lock().unwrap().iter().map(|(id, e)| e.info(*id)).collect()
    }

    /// Tasks started by `owner`.
    pub fn list_for(&self, owner: &SessionId) -> Vec<TaskInfo> {
        self.list().into_iter().filter(|t| t.owner.as_ref() == Some(owner)).collect()
    }

    pub fn get(&self, id: TaskId) -> Option<TaskInfo> {
        self.tasks.lock().unwrap().get(&id).map(|e| e.info(id))
    }

    /// Label a task's output (set before the task finishes; read by whoever
    /// hands the output to a model).
    pub fn set_trust(&self, id: TaskId, trust: Trust) {
        if let Some(e) = self.tasks.lock().unwrap().get_mut(&id) {
            e.trust = Some(trust);
        }
    }

    /// Record the task's declared writes: while it runs they are the
    /// agent's changes ([`TaskRegistry::write_scopes`]), and a rewind stops
    /// it first ([`TaskRegistry::kill_writers`]).
    pub fn set_writes(&self, id: TaskId, writes: Vec<Access>) {
        if let Some(e) = self.tasks.lock().unwrap().get_mut(&id) {
            e.writes = writes;
        }
    }

    /// Declared writes of the running tasks (for change attribution: the
    /// checkpointer counts changes there as the agent's).
    pub fn write_scopes(&self) -> Vec<Access> {
        let tasks = self.tasks.lock().unwrap();
        let mut out: Vec<Access> = vec![];
        for e in tasks.values().filter(|e| !e.status.borrow().is_finished()) {
            for w in &e.writes {
                if !out.contains(w) {
                    out.push(w.clone());
                }
            }
        }
        out
    }

    /// Stop the running tasks of `owner` that declared writes (before a
    /// rewind restores the workspace). Returns their ids.
    pub fn kill_writers(&self, owner: &SessionId) -> Vec<TaskId> {
        let tasks = self.tasks.lock().unwrap();
        let mut out = vec![];
        for (id, e) in tasks.iter() {
            if e.owner.as_ref() == Some(owner) && !e.writes.is_empty() && !e.status.borrow().is_finished() {
                e.cancel.cancel();
                out.push(*id);
            }
        }
        out
    }

    /// The label set with [`TaskRegistry::set_trust`].
    pub fn trust(&self, id: TaskId) -> Option<Trust> {
        self.tasks.lock().unwrap().get(&id).and_then(|e| e.trust.clone())
    }

    /// Kill a running task. Returns false if unknown or already finished.
    pub fn kill(&self, id: TaskId) -> bool {
        let tasks = self.tasks.lock().unwrap();
        match tasks.get(&id) {
            Some(e) if !e.status.borrow().is_finished() => {
                e.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Wait for a task to finish.
    pub async fn wait(&self, id: TaskId) -> Option<TaskStatus> {
        let mut rx = self.tasks.lock().unwrap().get(&id)?.status.subscribe();
        let s = rx.wait_for(|s| s.is_finished()).await.ok()?.clone();
        Some(s)
    }

    /// Forget finished tasks.
    pub fn prune(&self) {
        self.tasks.lock().unwrap().retain(|_, e| !e.status.borrow().is_finished());
    }
}
