//! In-process gates on proposed tool calls (ring 4).

use agent_proto::*;
use agent_runtime::Hook;
use async_trait::async_trait;
use globset::Glob;
use std::path::PathBuf;
use std::sync::Arc;

/// A proposed tool call as seen by an in-process gate.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub call: ToolCall,
    /// Session context is tainted (untrusted content entered it).
    pub tainted: bool,
    workspace: PathBuf,
}

impl Proposal {
    pub fn new(call: ToolCall, tainted: bool, workspace: PathBuf) -> Self {
        Proposal { call, tainted, workspace }
    }
    pub fn tool(&self) -> &str {
        &self.call.name
    }
    pub fn input(&self) -> &serde_json::Value {
        &self.call.input
    }
    pub fn class(&self) -> EffectClass {
        self.call.class
    }
    pub fn accesses(&self) -> &[Access] {
        &self.call.access
    }

    /// Does the call write a resource matching `pattern`? Patterns are resource
    /// URI globs (`fs:///repo/.github/**`, `net:*`) or workspace-relative paths
    /// (`.github/**`).
    pub fn writes(&self, pattern: &str) -> bool {
        self.touches(pattern, Some(AccessMode::Write))
    }

    pub fn reads(&self, pattern: &str) -> bool {
        self.touches(pattern, Some(AccessMode::Read))
    }

    pub fn touches(&self, pattern: &str, mode: Option<AccessMode>) -> bool {
        let pat = self.normalize(pattern);
        let Ok(glob) = Glob::new(&pat) else { return false };
        let m = glob.compile_matcher();
        self.call.access.iter().filter(|a| mode.is_none_or(|md| a.mode == md)).any(|a| {
            let r = a.resource.as_str();
            if m.is_match(r) {
                return true;
            }
            // A declared glob (e.g. an Opaque command's `fs:///ws/**`) covers the
            // pattern when the pattern's literal prefix lies beneath it.
            if let Some(base) = r.strip_suffix("/**") {
                let lit: String = pat.chars().take_while(|c| !matches!(c, '*' | '?' | '[' | '{')).collect();
                return lit.starts_with(base);
            }
            false
        })
    }

    fn normalize(&self, pattern: &str) -> String {
        normalize_pattern(pattern, &self.workspace.display().to_string())
    }
}

/// Resource URI globs pass through; absolute paths become `fs://` URIs;
/// relative paths are resolved against the workspace root.
pub(crate) fn normalize_pattern(pattern: &str, workspace: &str) -> String {
    let schemes = ["fs://", "net:", "cmd:", "mcp:", "secret:", "mem:", "git:"];
    if schemes.iter().any(|s| pattern.starts_with(s)) {
        pattern.to_string()
    } else if pattern.starts_with('/') {
        format!("fs://{pattern}")
    } else {
        format!("fs://{}/{}", workspace.trim_end_matches('/'), pattern.trim_start_matches("./"))
    }
}

pub(crate) struct CodeGate {
    name: String,
    f: Arc<dyn Fn(&Proposal) -> Verdict + Send + Sync>,
    workspace: PathBuf,
}

impl CodeGate {
    pub fn new(name: String, f: Arc<dyn Fn(&Proposal) -> Verdict + Send + Sync>, workspace: PathBuf) -> Self {
        CodeGate { name, f, workspace }
    }
}

#[async_trait]
impl Hook for CodeGate {
    fn name(&self) -> &str {
        &self.name
    }
    fn points(&self) -> Vec<HookPoint> {
        vec![HookPoint::PreTool]
    }
    async fn run(&self, req: &GateRequest) -> Result<Verdict, String> {
        match &req.subject {
            GateSubject::Tool { call } => {
                let p = Proposal::new(call.clone(), req.tainted, self.workspace.clone());
                let f = self.f.clone();
                // User code must not take the driver down.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&p)))
                    .map_err(|_| format!("gate `{}` panicked", self.name))
            }
            _ => Ok(Verdict::Allow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(access: Vec<Access>) -> Proposal {
        Proposal::new(
            ToolCall { id: "c".into(), name: "edit".into(), input: serde_json::json!({}), access, class: EffectClass::LocalWrite },
            false,
            PathBuf::from("/repo"),
        )
    }

    #[test]
    fn relative_and_uri_patterns() {
        let pr = p(vec![Access::write(ResourceUri::fs("/repo/.github/workflows/ci.yml"))]);
        assert!(pr.writes(".github/**"));
        assert!(pr.writes("fs:///repo/.github/**"));
        assert!(!pr.reads(".github/**"));
        assert!(!pr.writes("src/**"));
    }

    #[test]
    fn opaque_workspace_glob_covers_everything_beneath() {
        let pr = p(vec![Access::write(ResourceUri::fs("/repo/**"))]);
        assert!(pr.writes(".github/**"));
        assert!(!pr.writes("/etc/**"));
    }
}
