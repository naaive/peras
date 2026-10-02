//! Tool registry: name -> tool, specs for the Static layer, and enrichment of
//! model tool-use blocks into [`ToolCall`]s (access declaration + class).

use crate::ports::{AccessCtx, Tool, ToolEnv};
use agent_proto::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// The registry a runtime dispatches with, replaced as a whole when the
/// configuration's tool set changes (hot reload). Calls already dispatched
/// keep the tool they resolved.
pub struct LiveTools(RwLock<Arc<ToolRegistry>>);

impl LiveTools {
    pub fn new(registry: ToolRegistry) -> Self {
        LiveTools(RwLock::new(Arc::new(registry)))
    }

    /// The current registry.
    pub fn load(&self) -> Arc<ToolRegistry> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the registry.
    pub fn store(&self, registry: ToolRegistry) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(registry);
    }
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    ctx: AccessCtx,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        ToolRegistry::new(PathBuf::from("/workspace"))
    }
}

impl ToolRegistry {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        ToolRegistry { tools: BTreeMap::new(), ctx: AccessCtx { workspace: workspace.into() } }
    }

    /// Register a tool under its spec name (replaces an existing one).
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.tools.insert(tool.spec().name, tool);
        self
    }

    /// Let every tool adapt to the execution environment ([`Tool::adapt`]),
    /// e.g. `Bash` classifies commands only once a sandbox is known to exist.
    pub fn adapt(&mut self, env: &ToolEnv) -> &mut Self {
        for tool in self.tools.values_mut() {
            if let Some(t) = tool.adapt(env) {
                *tool = t;
            }
        }
        self
    }

    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register(tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn workspace(&self) -> &std::path::Path {
        &self.ctx.workspace
    }

    pub fn access_ctx(&self) -> &AccessCtx {
        &self.ctx
    }

    /// Tool specs in name order (deterministic).
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Turn a model tool-use block into a [`ToolCall`]. Unknown tools get class
    /// `Opaque` and no access; tools whose `access()` fails get no access (the
    /// call then fails at execution time, with nothing granted).
    pub fn enrich(&self, id: CallId, name: &str, input: serde_json::Value) -> ToolCall {
        let (access, class, isolated) = match self.tools.get(name) {
            None => (vec![], EffectClass::Opaque, false),
            Some(t) => {
                let access = match t.access(&input, &self.ctx) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(tool = name, error = %e, "access declaration failed");
                        vec![]
                    }
                };
                (access, t.class(&input), t.isolated(&input))
            }
        };
        ToolCall { id, name: name.to_string(), input, access, class, isolated }
    }
}
