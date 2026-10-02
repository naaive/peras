//! The coding tools added to the framework's built-ins: `edit` with
//! `replace_all`, `multi_edit`, `ls`, `todo_write` and `exit_plan_mode`.

use agent::proto::{Access, EffectClass, SessionId, ToolSpec};
use agent::runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use agent::tools::caps::{Dir, File, Read, Write};
use agent::tools::{tool, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Apply one replacement to `text`. Fails when `old` is missing, or occurs
/// more than once without `replace_all`.
pub fn replace(text: &str, old: &str, new: &str, replace_all: bool) -> std::result::Result<(String, usize), String> {
    if old.is_empty() {
        return Err("`old` must not be empty (use `write` to create a file)".into());
    }
    if old == new {
        return Err("`old` and `new` are identical: nothing to change".into());
    }
    match text.matches(old).count() {
        0 => Err("`old` was not found in the file; read the file and copy the text exactly, whitespace included".into()),
        1 => Ok((text.replacen(old, new, 1), 1)),
        n if replace_all => Ok((text.replace(old, new), n)),
        n => Err(format!(
            "`old` occurs {n} times; include more surrounding context to make it unique, or set `replace_all` to change every occurrence"
        )),
    }
}

/// Replace text in a file. `old` must match the file exactly (whitespace
/// included) and be unique, unless `replace_all` is true (then every
/// occurrence is replaced, e.g. to rename a variable). Read the file first.
#[tool]
pub async fn edit(file: Write<File>, old: String, new: String, replace_all: Option<bool>) -> Result<String> {
    let text = file.text().await?;
    let (out, n) = replace(&text, &old, &new, replace_all.unwrap_or(false)).map_err(ToolError::Failed)?;
    file.write_text(&out).await?;
    Ok(format!("Edited {} ({n} replacement{})", file.raw(), if n == 1 { "" } else { "s" }))
}

/// One replacement of a `multi_edit` call.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EditOp {
    /// Exact text to replace.
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Apply several replacements to one file, in order, atomically: each edit
/// applies to the result of the previous one, and if any fails the file is
/// left unchanged. Prefer this over several `edit` calls on the same file.
#[tool]
pub async fn multi_edit(file: Write<File>, edits: Vec<EditOp>) -> Result<String> {
    if edits.is_empty() {
        return Err(ToolError::InvalidInput("`edits` is empty".into()));
    }
    let mut text = file.text().await?;
    let mut total = 0;
    for (i, e) in edits.iter().enumerate() {
        let (out, n) = replace(&text, &e.old, &e.new, e.replace_all)
            .map_err(|m| ToolError::Failed(format!("edit {} of {}: {m} (no change was written)", i + 1, edits.len())))?;
        text = out;
        total += n;
    }
    file.write_text(&text).await?;
    Ok(format!("Applied {} edits to {} ({total} replacements)", edits.len(), file.raw()))
}

/// List the entries of a directory (not recursive); directories end with `/`.
/// Use `glob` to find files recursively.
#[tool]
pub async fn ls(dir: Read<Dir>) -> Result<String> {
    let entries = dir.list().await?;
    if entries.is_empty() {
        return Ok("(empty directory)".into());
    }
    let mut out: Vec<String> = entries
        .into_iter()
        .filter(|e| e.name != ".git")
        .map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name })
        .collect();
    out.sort_by_key(|e| (!e.ends_with('/'), e.clone()));
    Ok(out.join("\n"))
}

// ------------------------------------------------------------------ todos

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Todo {
    /// Imperative description ("Run the tests").
    pub content: String,
    pub status: TodoStatus,
}

/// The todo lists of the sessions of this process (also recoverable from
/// the journal: the last `todo_write` call of a session holds its list).
#[derive(Clone, Default)]
pub struct TodoStore(Arc<Mutex<HashMap<SessionId, Vec<Todo>>>>);

impl TodoStore {
    pub fn get(&self, s: &SessionId) -> Vec<Todo> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(s).cloned().unwrap_or_default()
    }

    pub fn set(&self, s: &SessionId, todos: Vec<Todo>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(s.clone(), todos);
    }

    /// Parse the `todos` input of a `todo_write` call.
    pub fn parse(input: &Value) -> Option<Vec<Todo>> {
        serde_json::from_value(input.get("todos")?.clone()).ok()
    }
}

/// `todo_write(todos)`: replace the session's task list.
pub struct TodoWrite(pub TodoStore);

#[async_trait]
impl Tool for TodoWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".into(),
            description: "Create or update the task list of this session (the full list each time). Use it for work with 3 or more steps or when the user gives several tasks: break the work down, keep exactly one task `in_progress`, and mark tasks `completed` as soon as they are done. The user sees the list.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Imperative description of the task" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
            class: EffectClass::Pure,
            subagent: false,
        }
    }

    fn access(&self, _input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        Ok(vec![])
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let todos = TodoStore::parse(&input).ok_or_else(|| ToolError::InvalidInput("`todos` must be a list of {content, status}".into()))?;
        let in_progress = todos.iter().filter(|t| t.status == TodoStatus::InProgress).count();
        self.0.set(&ctx.session, todos.clone());
        let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();
        let mut msg = format!("Task list updated: {done}/{} completed.", todos.len());
        if in_progress > 1 {
            msg.push_str(" Note: keep only one task in progress at a time.");
        }
        Ok(ToolOutput::text(msg))
    }
}

// ------------------------------------------------------------------ plan mode

/// `exit_plan_mode(plan)`: present the plan for approval and leave plan mode.
pub struct ExitPlanMode(pub crate::mode::Permissions);

#[async_trait]
impl Tool for ExitPlanMode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan_mode".into(),
            description: "In plan mode, when the research is done: present the implementation plan (Markdown, concise) to the user for approval. If approved, plan mode ends and you implement the plan; if not, keep planning with the user's feedback. Only for tasks that need code changes, not for research questions.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "plan": { "type": "string", "description": "The plan, in Markdown" } },
                "required": ["plan"],
                "additionalProperties": false
            }),
            class: EffectClass::Pure,
            subagent: false,
        }
    }

    fn access(&self, _input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        Ok(vec![])
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput> {
        use crate::mode::PermissionMode;
        if self.0.mode() != PermissionMode::Plan {
            return Ok(ToolOutput::text("Not in plan mode: go ahead."));
        }
        self.0.set_mode(PermissionMode::Default);
        Ok(ToolOutput::text("The user approved the plan. Plan mode has ended: start implementing it, tracking progress with todo_write."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements() {
        assert_eq!(replace("a b a", "b", "c", false).unwrap(), ("a c a".into(), 1));
        assert!(replace("a b a", "a", "c", false).unwrap_err().contains("2 times"));
        assert_eq!(replace("a b a", "a", "c", true).unwrap(), ("c b c".into(), 2));
        assert!(replace("abc", "x", "y", false).unwrap_err().contains("not found"));
        assert!(replace("abc", "", "y", false).is_err());
        assert!(replace("abc", "a", "a", false).is_err());
    }

    #[test]
    fn todo_input_parses() {
        let v = json!({"todos": [{"content": "Run tests", "status": "in_progress"}]});
        let t = TodoStore::parse(&v).unwrap();
        assert_eq!(t[0].status, TodoStatus::InProgress);
        assert!(TodoStore::parse(&json!({"todos": [{"content": "x", "status": "bogus"}]})).is_none());
    }
}
