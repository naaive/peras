use crate::caps::{check_granted, Capability, File, Observations, Read};
use crate::Result;
use agent_proto::{Access, EffectClass, ResourceUri, ToolSpec};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `load_skill(name)`: returns the skill's `SKILL.md`. Looked up in the
/// project (`.agent/skills/<name>/SKILL.md` in the workspace), then in the
/// user's skills (`~/.agent/skills/<name>/SKILL.md`, `$HOME` at call time).
/// Pure; declares a read of the file it returns. See [`SkillLoader`] for a
/// loader over a compiled skill catalog (plugins included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LoadSkill;

/// `load_skill` over an explicit catalog (name -> `SKILL.md` path, e.g. the
/// profile's skills from every configuration layer and plugin), falling back
/// to the project and user skill directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillLoader {
    catalog: BTreeMap<String, PathBuf>,
    user_dir: Option<PathBuf>,
}

impl SkillLoader {
    /// The default lookup: project skills, then `$HOME/.agent/skills`.
    pub fn new() -> Self {
        SkillLoader {
            catalog: BTreeMap::new(),
            user_dir: std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".agent").join("skills")),
        }
    }

    /// Skills by name (an entry wins over the directory lookup).
    pub fn with_catalog(mut self, skills: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        self.catalog.extend(skills);
        self
    }

    /// User-level skill directory (default `$HOME/.agent/skills`).
    pub fn with_user_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.user_dir = dir;
        self
    }

    /// Where `name`'s `SKILL.md` is: absolute, or relative to the workspace.
    fn resolve(&self, name: &str, workspace: &Path) -> PathBuf {
        if let Some(p) = self.catalog.get(name) {
            return p.clone();
        }
        let project = PathBuf::from(format!(".agent/skills/{name}/SKILL.md"));
        if workspace.join(&project).is_file() {
            return project;
        }
        if let Some(u) = &self.user_dir {
            let f = u.join(name).join("SKILL.md");
            if f.is_file() {
                return f;
            }
        }
        project
    }
}

#[derive(Deserialize)]
struct Input {
    name: String,
}

fn skill_name(input: &Value) -> Result<String> {
    let i: Input = serde_json::from_value(input.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
    let ok = !i.name.is_empty()
        && i.name != "."
        && i.name != ".."
        && i.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        return Err(ToolError::InvalidInput(format!("invalid skill name `{}`", i.name)));
    }
    Ok(i.name)
}

/// The file to read: a workspace handle, or an absolute path outside it.
enum Target {
    Workspace(Box<Read<File>>),
    Outside(PathBuf),
}

fn target(loader: &SkillLoader, input: &Value, workspace: &Path) -> Result<Target> {
    let name = skill_name(input)?;
    let p = loader.resolve(&name, workspace);
    let rel = if p.is_absolute() { p.strip_prefix(workspace).ok().map(Path::to_path_buf) } else { Some(p.clone()) };
    Ok(match rel {
        Some(r) => Target::Workspace(Box::new(Read::new(r.display().to_string()))),
        None => Target::Outside(p),
    })
}

fn spec() -> ToolSpec {
    ToolSpec {
        name: "load_skill".into(),
        description: "Load the full instructions of a skill listed in the skills catalog.".into(),
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

fn access(loader: &SkillLoader, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>> {
    match target(loader, input, &ctx.workspace)? {
        Target::Workspace(h) => h.access(ctx),
        Target::Outside(p) => Ok(vec![Access::read(ResourceUri::fs(&p.display().to_string()))]),
    }
}

async fn call(loader: &SkillLoader, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
    let unknown = |e: ToolError| match e {
        ToolError::Failed(m) if m.contains("No such file") => ToolError::Failed("unknown skill".into()),
        other => other,
    };
    match target(loader, &input, &ctx.workspace)? {
        Target::Workspace(mut h) => {
            let obs = Observations::default();
            h.bind(&ctx, &obs)?;
            let text = h.text().await.map_err(unknown)?;
            let mut out = ToolOutput::text(text);
            out.observed = obs.take();
            Ok(out)
        }
        Target::Outside(p) => {
            check_granted(&ctx, &Access::read(ResourceUri::fs(&p.display().to_string())))?;
            let text = tokio::fs::read_to_string(&p).await.map_err(|e| unknown(ToolError::Failed(e.to_string())))?;
            Ok(ToolOutput::text(text))
        }
    }
}

#[async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        spec()
    }
    fn access(&self, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>> {
        access(&SkillLoader::new(), input, ctx)
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        call(&SkillLoader::new(), input, ctx).await
    }
}

#[async_trait]
impl Tool for SkillLoader {
    fn spec(&self) -> ToolSpec {
        spec()
    }
    fn access(&self, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>> {
        access(self, input, ctx)
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput> {
        call(self, input, ctx).await
    }
}

impl agent_runtime::ToolName for LoadSkill {
    fn tool_name(&self) -> String {
        agent_runtime::Tool::spec(self).name
    }
}
