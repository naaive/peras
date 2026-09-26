use crate::caps::{Capability, File, Observations, Read};
use crate::Result;
use agent_proto::{Access, EffectClass, ToolSpec};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// `load_skill(name)`: returns `.agent/skills/<name>/SKILL.md` from the
/// workspace. Pure; declares a read of that file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LoadSkill;

#[derive(Deserialize)]
struct Input {
    name: String,
}

fn handle(input: &Value) -> Result<Read<File>> {
    let i: Input = serde_json::from_value(input.clone())
        .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
    let ok = !i.name.is_empty()
        && i.name != "."
        && i.name != ".."
        && i.name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        return Err(ToolError::InvalidInput(format!(
            "invalid skill name `{}`",
            i.name
        )));
    }
    Ok(Read::new(format!(".agent/skills/{}/SKILL.md", i.name)))
}

#[async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "load_skill".into(),
            description: "Load the full instructions of a skill listed in the skills directory."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Skill name" } },
                "required": ["name"],
                "additionalProperties": false
            }),
            class: EffectClass::Pure,
            subagent: false,
        }
    }
    fn access(&self, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>> {
        handle(input)?.access(ctx)
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        let mut h = handle(&input)?;
        let obs = Observations::default();
        h.bind(&ctx, &obs)?;
        let text = h.text().await.map_err(|e| match e {
            ToolError::Failed(m) if m.contains("No such file") => {
                ToolError::Failed("unknown skill".into())
            }
            other => other,
        })?;
        let mut out = ToolOutput::text(text);
        out.observed = obs.take();
        Ok(out)
    }
}

impl agent_runtime::ToolName for LoadSkill {
    fn tool_name(&self) -> String {
        agent_runtime::Tool::spec(self).name
    }
}
