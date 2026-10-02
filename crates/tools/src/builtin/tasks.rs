//! Background task tools over the runtime's task registry: `task_list`,
//! `task_output`, `task_kill`. A session sees only the tasks it started
//! (background sub-agents, long-running work); a task's end is also delivered
//! to it as a notification.

use crate::Result;
use agent_proto::{Access, EffectClass, ToolSpec};
use agent_runtime::{AccessCtx, TaskRegistry, TaskStatus, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

fn registry(ctx: &ToolCtx) -> Result<&Arc<TaskRegistry>> {
    ctx.tasks.as_ref().ok_or_else(|| ToolError::Failed("background tasks are not available here".into()))
}

fn task_id(input: &Value) -> Result<u64> {
    input.get("id").and_then(Value::as_u64).ok_or_else(|| ToolError::InvalidInput("missing `id`".into()))
}

fn id_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "properties": { "id": { "type": "integer", "description": what } },
        "required": ["id"],
        "additionalProperties": false
    })
}

/// `task_list()`: the session's background tasks and their state.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskList;

#[async_trait]
impl Tool for TaskList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_list".into(),
            description: "List the background tasks started in this session and their state.".into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            class: EffectClass::Pure,
            subagent: false,
        }
    }
    fn access(&self, _input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        Ok(vec![])
    }
    async fn call(&self, _input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let tasks = registry(&ctx)?.list_for(&ctx.session);
        if tasks.is_empty() {
            return Ok(ToolOutput::text("No background tasks."));
        }
        Ok(ToolOutput::text(tasks.iter().map(agent_runtime::tasks::describe).collect::<Vec<_>>().join("\n")))
    }
}

/// `task_output(id)`: the stored output of a finished task.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskOutput;

#[async_trait]
impl Tool for TaskOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_output".into(),
            description: "Read the output of a finished background task.".into(),
            input_schema: id_schema("Task id (from task_list or the completion notice)"),
            class: EffectClass::Pure,
            subagent: false,
        }
    }
    fn access(&self, input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        task_id(input)?;
        Ok(vec![])
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let id = task_id(&input)?;
        let reg = registry(&ctx)?;
        let info = reg
            .get(id)
            .filter(|t| t.owner.as_ref() == Some(&ctx.session))
            .ok_or_else(|| ToolError::Failed(format!("no task {id} in this session")))?;
        match &info.status {
            TaskStatus::Done { output: Some(blob) } => {
                let bytes = reg.blobs().get(blob).await.map_err(|e| ToolError::Infra(e.to_string()))?;
                let mut out = ToolOutput::text(String::from_utf8_lossy(&bytes).into_owned());
                out.trust = reg.trust(id);
                Ok(out)
            }
            _ => Ok(ToolOutput::text(agent_runtime::tasks::describe(&info))),
        }
    }
}

/// `task_kill(id)`: stop a running background task.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskKill;

#[async_trait]
impl Tool for TaskKill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_kill".into(),
            description: "Stop a running background task started in this session.".into(),
            input_schema: id_schema("Task id"),
            class: EffectClass::LocalWrite,
            subagent: false,
        }
    }
    fn access(&self, input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        task_id(input)?;
        Ok(vec![])
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let id = task_id(&input)?;
        let reg = registry(&ctx)?;
        let owned = reg.get(id).is_some_and(|t| t.owner.as_ref() == Some(&ctx.session));
        if owned && reg.kill(id) {
            Ok(ToolOutput::text(format!("Task {id} stopped.")))
        } else {
            Err(ToolError::Failed(format!("no running task {id} in this session")))
        }
    }
}
