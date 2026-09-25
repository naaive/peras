//! Background task registry (simple version): long-running commands, async
//! sub-agents, timers. List, kill, timeout; output stored as a blob.

use crate::ports::BlobStore;
use agent_proto::BlobRef;
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
}

struct Entry {
    name: String,
    cancel: CancellationToken,
    status: watch::Sender<TaskStatus>,
}

pub struct TaskRegistry {
    blobs: Arc<dyn BlobStore>,
    tasks: Arc<Mutex<BTreeMap<TaskId, Entry>>>,
    next: Mutex<TaskId>,
}

impl TaskRegistry {
    pub fn new(blobs: Arc<dyn BlobStore>) -> Self {
        TaskRegistry { blobs, tasks: Arc::default(), next: Mutex::new(1) }
    }

    /// Spawn a task. `f` gets a cancellation token and returns its output
    /// bytes (stored as a blob) or an error.
    pub fn spawn<F, Fut>(&self, name: impl Into<String>, timeout: Option<Duration>, f: F) -> TaskId
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
        self.tasks
            .lock()
            .unwrap()
            .insert(id, Entry { name: name.into(), cancel: cancel.clone(), status: status.clone() });
        let blobs = self.blobs.clone();
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
            status.send_replace(r);
        });
        id
    }

    pub fn list(&self) -> Vec<TaskInfo> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .map(|(id, e)| TaskInfo { id: *id, name: e.name.clone(), status: e.status.borrow().clone() })
            .collect()
    }

    pub fn get(&self, id: TaskId) -> Option<TaskInfo> {
        self.tasks
            .lock()
            .unwrap()
            .get(&id)
            .map(|e| TaskInfo { id, name: e.name.clone(), status: e.status.borrow().clone() })
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
