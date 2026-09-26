//! The `Bash` tool.
//!
//! `Bash` (a unit struct, usable as `.tools((read, edit, Bash))`) assumes no
//! sandbox is available: every command is `Opaque`, exclusive over the
//! workspace. `Bash::new(true)` returns a [`BashTool`] that applies the
//! [`SemanticTable`] to classify commands; only use it when an OS sandbox
//! enforces the compiled [`SandboxSpec`](agent_runtime::SandboxSpec).

use crate::caps::{check_granted, compile_spec};
use crate::shell::{self, SemanticTable, ShellAnalysis};
use agent_proto::{Access, EffectClass, ToolContent, ToolSpec};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Outputs longer than this are spilled to a blob.
pub const MAX_INLINE_OUTPUT: usize = 30_000;

/// Runs shell commands; without a sandbox every command is Opaque.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Bash;

impl Bash {
    /// A configurable bash tool. With `sandbox_available == false` the semantic
    /// table never lowers the class (always Opaque).
    #[allow(clippy::new_ret_no_self)]
    pub fn new(sandbox_available: bool) -> BashTool {
        BashTool::new(sandbox_available)
    }
}

/// Configurable bash tool (see [`Bash`]).
#[derive(Debug, Clone)]
pub struct BashTool {
    sandbox_available: bool,
    table: Arc<SemanticTable>,
}

impl Default for BashTool {
    fn default() -> Self {
        BashTool::new(false)
    }
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
}

fn parse_input(v: &Value) -> Result<BashInput, ToolError> {
    serde_json::from_value(v.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

impl BashTool {
    pub fn new(sandbox_available: bool) -> Self {
        BashTool {
            sandbox_available,
            table: Arc::new(SemanticTable::defaults()),
        }
    }
    pub fn with_table(mut self, table: SemanticTable) -> Self {
        self.table = Arc::new(table);
        self
    }
    pub fn table(&self) -> &SemanticTable {
        &self.table
    }
    pub fn sandbox_available(&self) -> bool {
        self.sandbox_available
    }

    /// The analysis used for access/class: the semantic table when a sandbox is
    /// available, otherwise Opaque (still split into `cmd:` resources).
    pub fn analyze(&self, command: &str, workspace: &Path) -> ShellAnalysis {
        if self.sandbox_available {
            self.table.analyze(command, workspace)
        } else {
            let parsed = shell::parse(command);
            let lines: Vec<String> = if parsed.opaque.is_some() {
                vec![]
            } else {
                parsed.commands.iter().map(|c| c.line()).collect()
            };
            ShellAnalysis::opaque(command, &lines, workspace, "no sandbox available")
        }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command with `bash -c` in the workspace root. Output is stdout and \
                          stderr combined, followed by the exit code. Commands are sandboxed according to \
                          their declared accesses."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line to run" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS,
                                    "description": "Timeout in milliseconds (default 120000)" },
                    "description": { "type": "string", "description": "Short description of what the command does" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            class: EffectClass::Opaque,
            subagent: false,
        }
    }

    async fn run(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let inp = parse_input(&input)?;
        let analysis = self.analyze(&inp.command, &ctx.workspace);
        for a in &analysis.accesses {
            check_granted(&ctx, a)?;
        }
        let timeout_ms = inp
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let read_only = analysis.class == EffectClass::Pure;
        let mut spec = compile_spec(&ctx, timeout_ms, !read_only);
        if read_only {
            // Read-only commands: read-only, offline sandbox.
            spec.writable.clear();
            spec.network.clear();
        }
        let argv = vec!["bash".to_string(), "-c".to_string(), inp.command.clone()];
        let cancel = ctx.cancel.child_token();
        let run = ctx.sandbox.run(&argv, &spec, cancel.clone());
        let grace = std::time::Duration::from_millis(timeout_ms + 5_000);
        let out = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => { cancel.cancel(); return Err(ToolError::Cancelled) }
            r = tokio::time::timeout(grace, run) => match r {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return Err(ToolError::Failed(format!("failed to run command: {e}"))),
                Err(_) => { cancel.cancel(); return Err(ToolError::Failed(format!("command timed out after {timeout_ms} ms"))) }
            },
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if out.timed_out {
            text.push_str(&format!("[timed out after {timeout_ms} ms]"));
            return Err(ToolError::Failed(text));
        }
        match out.status {
            Some(code) => text.push_str(&format!("[exit code {code}]")),
            None => text.push_str("[terminated by signal]"),
        }
        if !out.overlay_changes.is_empty() {
            text.push_str(&format!(
                "\n[changed files: {}]",
                out.overlay_changes.join(", ")
            ));
        }
        spill(text, &ctx).await
    }
}

/// Long outputs go to a blob; the model sees head and tail.
pub(crate) async fn spill(text: String, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
    if text.len() <= MAX_INLINE_OUTPUT {
        return Ok(ToolOutput::text(text));
    }
    let blob = ctx
        .blobs
        .put(text.as_bytes(), Some("text/plain"))
        .await
        .map_err(|e| ToolError::Infra(e.to_string()))?;
    let half = MAX_INLINE_OUTPUT / 2;
    let mut head_end = half;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - half;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let preview = format!(
        "{}\n... [{} bytes omitted; full output in blob {}] ...\n{}",
        &text[..head_end],
        tail_start - head_end,
        blob.sha256,
        &text[tail_start..]
    );
    Ok(ToolOutput {
        content: vec![ToolContent::Blob { blob, preview }],
        ..Default::default()
    })
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        Self::spec()
    }
    fn access(&self, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        let inp = parse_input(input)?;
        Ok(self.analyze(&inp.command, &ctx.workspace).accesses)
    }
    fn class(&self, input: &Value) -> EffectClass {
        if !self.sandbox_available {
            return EffectClass::Opaque;
        }
        match parse_input(input) {
            // The class does not depend on the workspace path.
            Ok(inp) => {
                self.table
                    .analyze(&inp.command, Path::new("/workspace"))
                    .class
            }
            Err(_) => EffectClass::Opaque,
        }
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        self.run(input, ctx).await
    }
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        BashTool::spec()
    }
    fn access(&self, input: &Value, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        BashTool::default().access(input, ctx)
    }
    fn class(&self, _input: &Value) -> EffectClass {
        EffectClass::Opaque
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        BashTool::default().run(input, ctx).await
    }
}

impl agent_runtime::ToolName for Bash {
    fn tool_name(&self) -> String {
        agent_runtime::Tool::spec(self).name
    }
}

impl agent_runtime::ToolName for BashTool {
    fn tool_name(&self) -> String {
        agent_runtime::Tool::spec(self).name
    }
}
