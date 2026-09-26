//! A sub-agent is a tool: `Agent::new(..).tools((read,)).describe("Review the diff")`
//! can be passed to another agent's `.tools(..)`.
//!
//! The child runs in its own session whose id is derived from the parent
//! session and the call id, so recovery always finds the same child. Its output
//! is marked untrusted when the child's context was tainted.

use crate::agent::Agent;
use crate::run::{outcome_to_result, Run, Target};
use agent_proto::*;
use agent_runtime::*;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;

#[async_trait]
impl Tool for Agent {
    fn spec(&self) -> ToolSpec {
        let description = if self.cfg.description.is_empty() {
            format!("Delegate a self-contained task to the `{}` sub-agent.", self.cfg.name)
        } else {
            self.cfg.description.clone()
        };
        ToolSpec {
            name: self.cfg.name.clone(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": { "task": { "type": "string", "description": "A self-contained task description." } },
                "required": ["task"],
                "additionalProperties": false
            }),
            // The child's own gates govern its effects; to the parent it is an
            // opaque delegation.
            class: EffectClass::Opaque,
            subagent: true,
        }
    }

    fn access(&self, _input: &serde_json::Value, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::write(ResourceUri::fs(&format!("{}/**", ctx.workspace.display())))])
    }

    async fn call(&self, input: serde_json::Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let task = input
            .get("task")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing `task`".into()))?
            .to_string();
        let child = ctx.session.child(&ctx.call_id);
        let mut run = Run::new(self.clone(), Target::Open(child.clone()), task);
        let mut outcome = None;
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    run.control().interrupt();
                }
                u = run.next() => match u {
                    Some(crate::Update::Ask(a)) => a.deny("sub-agent asks cannot be answered from the parent"),
                    Some(crate::Update::Done(o)) => { outcome = Some(o); }
                    Some(_) => {}
                    None => break,
                }
            }
        }
        let outcome = outcome.ok_or_else(|| ToolError::Infra("sub-agent ended without outcome".into()))?;
        let tainted = match self.built().await {
            Ok(b) => b.rt.session(&child).map(|h| h.with_state(agent_kernel::is_tainted)).unwrap_or(false),
            Err(_) => false,
        };
        let text = outcome_to_result(&child, outcome).map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput {
            content: vec![ToolContent::Text { text }],
            trust: tainted.then(|| Trust::Untrusted { source: format!("subagent:{}", self.cfg.name) }),
            observed: vec![],
        })
    }
}
