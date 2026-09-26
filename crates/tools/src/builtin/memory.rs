use crate::caps::{Key, Mem, DEFAULT_MEM_SCOPE};
use crate::{tool, Result};
use agent_proto::{Access, EffectClass, ResourceUri, ToolSpec};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// Save a fact to long-term memory under `key` (`scope/key`, scope defaults to
/// `project`). It is available in future sessions.
#[tool]
pub async fn remember(key: Mem<Key>, value: String) -> Result<String> {
    let prev = key.set(&value).await?;
    Ok(match prev {
        Some(p) => format!("Updated `{}` (previous value: {p})", key.raw()),
        None => format!("Remembered `{}`", key.raw()),
    })
}

/// `recall(query, scope?)`: search long-term memory. Declares a read of
/// `mem:<scope>/*`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Recall;

#[derive(Deserialize)]
struct RecallInput {
    query: String,
    #[serde(default)]
    scope: Option<String>,
}

fn parse(v: &Value) -> Result<RecallInput> {
    serde_json::from_value(v.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

fn scope_of(i: &RecallInput) -> Result<String> {
    let s = i
        .scope
        .clone()
        .unwrap_or_else(|| DEFAULT_MEM_SCOPE.to_string());
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(ToolError::InvalidInput(format!("invalid scope `{s}`")));
    }
    Ok(s)
}

#[async_trait]
impl Tool for Recall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".into(),
            description: "Search long-term memory for entries matching a query.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to look for" },
                    "scope": { "type": "string", "description": "Memory scope (default `project`)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            class: EffectClass::Pure,
            subagent: false,
        }
    }
    fn access(&self, input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>> {
        let i = parse(input)?;
        Ok(vec![Access::read(ResourceUri::mem(&format!(
            "{}/*",
            scope_of(&i)?
        )))])
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let i = parse(&input)?;
        let scope = scope_of(&i)?;
        crate::caps::check_granted(&ctx, &Access::read(ResourceUri::mem(&format!("{scope}/*"))))?;
        let mem = ctx
            .memory
            .as_ref()
            .ok_or_else(|| ToolError::Failed("no memory store configured".into()))?;
        let hits = mem
            .recall(&scope, &i.query)
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?;
        if hits.is_empty() {
            return Ok(ToolOutput::text("No matching memories"));
        }
        Ok(ToolOutput::text(
            hits.iter()
                .map(|(k, v)| format!("{scope}/{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

impl agent_runtime::ToolName for Recall {
    fn tool_name(&self) -> String {
        agent_runtime::Tool::spec(self).name
    }
}
