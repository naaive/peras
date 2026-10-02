//! Context engineering on the product side: state snapshots and long-term
//! memory.
//!
//! - **Snapshots** ("what is true now"): the permission mode, the task list,
//!   the git state and the date are sent as state values before each turn.
//!   The kernel appends a snapshot only when a value changed, so the git
//!   state is rate-limited by `[[snapshots]]`. Old snapshots are superseded
//!   and are the first to go under pressure. The system prompt keeps only
//!   what never changes during a session, so its cache prefix stays stable.
//! - **Memory**: `user` scope in `~/.agent/memory`, `project` scope in
//!   `<workspace>/.agent/memory`. Memory is loaded into the Durable layer at
//!   session start; `remember` / `recall` are gated like any other write.

use crate::mode::PermissionMode;
use crate::tools::{TodoStatus, TodoStore};
use agent::proto::SessionId;
use agent::runtime::{MemoryStore, StoreError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub const MODE_KEY: &str = "permission_mode";
pub const TODOS_KEY: &str = "todos";
pub const GIT_KEY: &str = "git_status";
pub const DATE_KEY: &str = "date";

/// Minimum interval between two git snapshots (ms).
pub const GIT_MIN_INTERVAL_MS: u64 = 60_000;

pub fn mode_text(m: PermissionMode) -> String {
    match m {
        PermissionMode::Default => {
            "Permission mode: default. Reading is free; edits and commands may need the user's approval.".into()
        }
        PermissionMode::AcceptEdits => {
            "Permission mode: accept edits. File edits inside the workspace are approved automatically; commands may still need approval.".into()
        }
        PermissionMode::Plan => "Permission mode: plan. Only read and search: edits and commands are refused. Research the task, then present the plan with `exit_plan_mode` for approval.".into(),
        PermissionMode::BypassPermissions => {
            "Permission mode: bypass permissions. Calls are approved automatically, except those that could exfiltrate data, persist across sessions, change the agent's own configuration or run unanalyzable commands outside isolation.".into()
        }
    }
}

pub fn todos_text(todos: &TodoStore, session: &SessionId) -> String {
    let list = todos.get(session);
    if list.is_empty() || list.iter().all(|t| t.status == TodoStatus::Completed) {
        return String::new();
    }
    let mut out = String::from("Current task list (update it with todo_write):\n");
    for t in &list {
        let mark = match t.status {
            TodoStatus::Completed => "[x]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Pending => "[ ]",
        };
        out.push_str(&format!("{mark} {}\n", t.content));
    }
    out
}

/// Branch, short status and recent commits; empty outside a git repository.
pub fn git_text(dir: &Path) -> String {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        return String::new();
    }
    let branch = git(&["branch", "--show-current"]).filter(|b| !b.is_empty()).unwrap_or_else(|| "(detached)".into());
    let status = git(&["status", "--short"]).unwrap_or_default();
    let lines: Vec<&str> = status.lines().collect();
    let status = match lines.len() {
        0 => "(clean)".to_string(),
        n if n > 40 => format!("{}\n... ({} more)", lines[..40].join("\n"), n - 40),
        _ => status.clone(),
    };
    let log = git(&["log", "--oneline", "-5"]).unwrap_or_default();
    format!("Git branch: {branch}\nStatus:\n{status}\nRecent commits:\n{log}")
}

/// Sends the state values that changed since the last turn of a session.
#[derive(Clone, Default)]
pub struct StateSync {
    sent: Arc<Mutex<HashMap<(String, &'static str), String>>>,
}

impl StateSync {
    /// The current values of a session's state keys.
    pub fn values(dir: &Path, mode: PermissionMode, todos: &TodoStore, session: &SessionId) -> Vec<(&'static str, String)> {
        vec![
            (MODE_KEY, mode_text(mode)),
            (TODOS_KEY, todos_text(todos, session)),
            (GIT_KEY, git_text(dir)),
            (DATE_KEY, format!("Today's date: {}", crate::prompt::today())),
        ]
    }

    /// Values that differ from the last ones sent to `session` (and records
    /// them as sent).
    pub fn changed(&self, session: &SessionId, values: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
        let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        values
            .into_iter()
            .filter(|(k, v)| {
                let key = (session.to_string(), *k);
                let new = sent.get(&key) != Some(v) && !(v.is_empty() && !sent.contains_key(&key));
                if new {
                    sent.insert(key, v.clone());
                }
                new
            })
            .collect()
    }
}

/// Long-term memory with the `user` scope in the user's home and every
/// other scope (`project`) in the workspace.
pub struct ScopedMemory {
    user: Option<agent::adapters::FileMemoryStore>,
    project: agent::adapters::FileMemoryStore,
}

impl ScopedMemory {
    pub fn new(home: Option<&Path>, workspace: &Path) -> Result<ScopedMemory, StoreError> {
        let user = match home {
            Some(h) => Some(agent::adapters::FileMemoryStore::new(h.join(".agent").join("memory"))?),
            None => None,
        };
        let project = agent::adapters::FileMemoryStore::new(crate::data_dir(workspace).join("memory"))?;
        Ok(ScopedMemory { user, project })
    }

    fn store(&self, scope: &str) -> Result<&agent::adapters::FileMemoryStore, StoreError> {
        match scope {
            "user" => self.user.as_ref().ok_or_else(|| StoreError::NotFound("no user memory (no home directory)".into())),
            _ => Ok(&self.project),
        }
    }

    pub fn dirs(&self) -> Vec<PathBuf> {
        self.user.iter().chain(std::iter::once(&self.project)).map(|s| s.dir().to_path_buf()).collect()
    }
}

#[async_trait::async_trait]
impl MemoryStore for ScopedMemory {
    async fn load(&self, scope: &str) -> Result<Vec<(String, String)>, StoreError> {
        match self.store(scope) {
            Ok(s) => s.load(scope).await,
            Err(_) => Ok(vec![]),
        }
    }
    async fn recall(&self, scope: &str, query: &str) -> Result<Vec<(String, String)>, StoreError> {
        self.store(scope)?.recall(scope, query).await
    }
    async fn remember(&self, scope: &str, key: &str, value: &str) -> Result<Option<String>, StoreError> {
        self.store(scope)?.remember(scope, key, value).await
    }
    async fn forget(&self, scope: &str, key: &str) -> Result<Option<String>, StoreError> {
        self.store(scope)?.forget(scope, key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_changed_values_are_sent() {
        let s = StateSync::default();
        let id = SessionId::new("s");
        let first = s.changed(&id, vec![(MODE_KEY, "a".into()), (TODOS_KEY, String::new())]);
        assert_eq!(first, vec![(MODE_KEY, "a".to_string())], "an empty value is only sent to clear an earlier one");
        assert!(s.changed(&id, vec![(MODE_KEY, "a".into())]).is_empty());
        assert_eq!(s.changed(&id, vec![(MODE_KEY, "b".into())]).len(), 1);
        assert_eq!(s.changed(&SessionId::new("t"), vec![(MODE_KEY, "b".into())]).len(), 1, "per session");
        s.changed(&id, vec![(TODOS_KEY, "x".into())]);
        assert_eq!(s.changed(&id, vec![(TODOS_KEY, String::new())]), vec![(TODOS_KEY, String::new())], "clearing");
    }

    #[test]
    fn todos_snapshot_skips_finished_lists() {
        let t = TodoStore::default();
        let id = SessionId::new("s");
        assert_eq!(todos_text(&t, &id), "");
        t.set(&id, vec![crate::tools::Todo { content: "a".into(), status: TodoStatus::InProgress }]);
        assert!(todos_text(&t, &id).contains("[~] a"));
        t.set(&id, vec![crate::tools::Todo { content: "a".into(), status: TodoStatus::Completed }]);
        assert_eq!(todos_text(&t, &id), "");
    }

    #[tokio::test]
    async fn memory_scopes_live_apart() {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let m = ScopedMemory::new(Some(home.path()), ws.path()).unwrap();
        m.remember("user", "editor", "vim").await.unwrap();
        m.remember("project", "build", "cargo build").await.unwrap();
        assert_eq!(m.load("user").await.unwrap(), vec![("editor".into(), "vim".into())]);
        assert_eq!(m.load("project").await.unwrap(), vec![("build".into(), "cargo build".into())]);
        assert!(ws.path().join(".agent/memory").exists());
        let none = ScopedMemory::new(None, ws.path()).unwrap();
        assert!(none.load("user").await.unwrap().is_empty());
        assert!(none.remember("user", "k", "v").await.is_err());
    }
}
