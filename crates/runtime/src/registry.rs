//! Tool registry: name -> tool, specs for the Static layer, and enrichment of
//! model tool-use blocks into [`ToolCall`]s (access declaration + class).

use crate::ports::{AccessCtx, Tool};
use agent_proto::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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
        let (access, class) = match self.tools.get(name) {
            None => (vec![], EffectClass::Opaque),
            Some(t) => {
                let access = match t.access(&input, &self.ctx) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(tool = name, error = %e, "access declaration failed");
                        vec![]
                    }
                };
                (access, t.class(&input))
            }
        };
        ToolCall { id, name: name.to_string(), input, access, class }
    }
}
