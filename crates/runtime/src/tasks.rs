//! Background task registry: long-running commands, async sub-agents, timers.
//! List, kill, timeout; output stored as a blob. Tasks may belong to a
//! session: when one finishes, the registry's notifier is told (the runtime
//! delivers it to the owning session as a notification).

use crate::ports::BlobStore;
use agent_proto::{BlobRef, SessionId, Trust};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub type TaskId = u64;

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
            Entry { name: name.clone(), owner: owner.clone(), trust: None, cancel: cancel.clone(), status: status.clone() },
        );
        let blobs = self.blobs.clone();
        let notifier = self.notifier.clone();
        tokio::spawn(async move {
            let timeout = timeout.unwrap_or(Duration::from_secs(365 * 24 * 3600));
            let r = tokio::select! {
                biased;
                _ = cancel.cancelled() => TaskStatus::Killed,
                r = tokio::time::timeout(timeout, fut) => match r {
                    Err(_) => { cancel.cancel(); TaskStatus::TimedOut }
                    Ok(Err(e)) => TaskStatus::Failed { error: e },
                    Ok(Ok(bytes)) if bytes.is_empty() => TaskStatus::Done { output: None },
                    Ok(Ok(bytes)) => match blobs.put(&bytes, Some("application/octet-stream")).await {
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
