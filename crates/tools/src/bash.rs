//! The `Bash` tool.
//!
//! `Bash` (a unit struct, usable as `.tools((read, edit, Bash))`) knows
//! nothing about the sandbox it will run in, so on its own every command is
//! `Opaque`, exclusive over the workspace. Registered through the SDK it is
//! adapted ([`Tool::adapt`]) to the probed sandbox: when an OS sandbox
//! enforces the compiled [`SandboxSpec`](agent_runtime::SandboxSpec), the
//! [`SemanticTable`] (extended by `[[shell.commands]]` from the profile)
//! classifies commands. `Bash::new(true)` returns such a [`BashTool`]
//! directly; only use it when an OS sandbox enforces the spec.
//!
//! With `"background": true` the command runs as a background task of the
//! session's task registry: the call returns at once with the task id; the
//! output (stdout and stderr, then the exit code) is stored as a blob and
//! read with `task_output`, the end is delivered to the session as a
//! notification, `task_kill` stops it (its whole process group) and its
//! timeout (default [`DEFAULT_BACKGROUND_TIMEOUT_MS`], at most
//! [`MAX_BACKGROUND_TIMEOUT_MS`]) ends it as timed out. Gating is unchanged:
//! the call is judged on its declared accesses before it starts, and while it
//! runs its declared writes count as the agent's (change attribution) and a
//! rewind stops it first. A background command is never run isolated
//! (design: "Commands in isolated execution cannot be turned into background
//! tasks"): its changes would land after the diff review, so an Opaque
//! background command is approved before it runs instead of after.

use crate::caps::{check_granted, compile_spec};
use crate::shell::{self, Rule, SemanticTable, ShellAnalysis};
use agent_proto::{Access, EffectClass, ToolContent, ToolSpec};
use agent_runtime::{staged_key, AccessCtx, Tool, ToolCtx, ToolEnv, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Background commands: default and maximum timeout.
pub const DEFAULT_BACKGROUND_TIMEOUT_MS: u64 = 1_800_000;
pub const MAX_BACKGROUND_TIMEOUT_MS: u64 = 7_200_000;
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
    /// Opaque commands run isolated (staged for review) instead of asking first.
    isolation: bool,
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
    #[serde(default)]
    background: bool,
}

fn parse_input(v: &Value) -> Result<BashInput, ToolError> {
    serde_json::from_value(v.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

impl BashTool {
    pub fn new(sandbox_available: bool) -> Self {
        BashTool {
            sandbox_available,
            isolation: false,
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
    /// Run Opaque commands isolated: on a copy of the workspace, offline, with
    /// their changes staged for review (needs a sandbox with isolation).
    pub fn with_isolation(mut self, isolation: bool) -> Self {
        self.isolation = isolation;
        self
    }
    pub fn isolation(&self) -> bool {
        self.isolation
    }

    /// This tool adapted to `env`: the semantic table applies only if the
    /// sandbox is really available (never more than this tool already
    /// assumed), and the configured rules extend the table.
    fn adapted(&self, env: &ToolEnv) -> BashTool {
        let mut table = (*self.table).clone();
        table.extend(env.shell_rules.iter().map(Rule::from_def));
        BashTool {
            sandbox_available: self.sandbox_available && env.sandbox.available,
            isolation: env.sandbox.available && env.sandbox.isolation,
            table: Arc::new(table),
        }
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
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_BACKGROUND_TIMEOUT_MS,
                                    "description": "Timeout in milliseconds (default 120000, at most 600000; in the background default 1800000, at most 7200000)" },
                    "description": { "type": "string", "description": "Short description of what the command does" },
                    "background": { "type": "boolean",
                                    "description": "Run as a background task (servers, watchers, long builds): returns the task id at once; you are notified when it ends, read its output with task_output and stop it with task_kill" }
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
        if inp.background {
            return background(inp, analysis, ctx);
        }
        let timeout_ms = inp
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let read_only = analysis.class == EffectClass::Pure;
        // Approved to run isolated: on a copy of the workspace, offline, the
        // changes staged until the kernel has them reviewed.
        let isolated = ctx.isolated;
        let mut spec = compile_spec(&ctx, timeout_ms, !read_only);
        if read_only {
            // Read-only commands: read-only, offline sandbox.
            spec.writable.clear();
        }
        if read_only || isolated {
            spec.network.clear();
        }
        spec.isolated = isolated;
        let argv = vec!["bash".to_string(), "-c".to_string(), inp.command.clone()];
        let cancel = ctx.cancel.child_token();
        let key = staged_key(&ctx.session, &ctx.call_id);
        let run = async {
            if isolated {
                ctx.sandbox
                    .run_staged(&key, &argv, &spec, cancel.clone())
                    .await
            } else {
                ctx.sandbox.run(&argv, &spec, cancel.clone()).await
            }
        };
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
        let mut text = output_text(&out);
        if out.timed_out {
            text.push_str(&format!("[timed out after {timeout_ms} ms]"));
            return Err(ToolError::Failed(text));
        }
        push_status(&mut text, &out);
        let staged = if isolated {
            out.overlay_changes.clone()
        } else {
            vec![]
        };
        if !staged.is_empty() {
            text.push_str(&format!(
                "\n[ran isolated; changed files staged for review: {}]",
                list(&staged)
            ));
        } else if !out.overlay_changes.is_empty() {
            text.push_str(&format!(
                "\n[changes outside the declared writes, not written back: {}]",
                list(&out.overlay_changes)
            ));
        }
        let mut output = spill(text, &ctx).await?;
        output.staged = staged;
        Ok(output)
    }
}

/// stdout and stderr combined, ending with a newline.
fn output_text(out: &agent_runtime::ExecOutput) -> String {
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
    text
}

fn push_status(text: &mut String, out: &agent_runtime::ExecOutput) {
    match out.status {
        Some(code) => text.push_str(&format!("[exit code {code}]")),
        None => text.push_str("[terminated by signal]"),
    }
}

/// Start the command as a background task owned by the session (see the
/// module docs). Never isolated: the task registry has no review step, so the
/// changes of a background command are not staged.
fn background(inp: BashInput, analysis: ShellAnalysis, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
    if ctx.isolated {
        // `isolated()` never asks for it; refuse rather than bypass the review.
        return Err(ToolError::Failed(
            "commands that run isolated cannot run in the background (their changes need review first)".into(),
        ));
    }
    let Some(tasks) = ctx.tasks.clone() else {
        return Err(ToolError::Failed("background tasks are not available here".into()));
    };
    let timeout_ms = inp
        .timeout_ms
        .unwrap_or(DEFAULT_BACKGROUND_TIMEOUT_MS)
        .clamp(1, MAX_BACKGROUND_TIMEOUT_MS);
    let read_only = analysis.class == EffectClass::Pure;
    // The registry enforces the timeout (status "timed out"); the sandbox's
    // own limit is only a backstop.
    let mut spec = compile_spec(&ctx, timeout_ms + 10_000, !read_only);
    if read_only {
        spec.writable.clear();
        spec.network.clear();
    }
    let trust = background_trust(&ctx, &analysis, &spec);
    let writes: Vec<Access> = analysis.accesses.iter().filter(|a| a.mode == agent_proto::AccessMode::Write).cloned().collect();
    let argv = vec!["bash".to_string(), "-c".to_string(), inp.command.clone()];
    let sandbox = ctx.sandbox.clone();
    let name = format!("bash: {}", one_line(&inp.command, 80));
    let id = tasks.spawn_for(Some(ctx.session.clone()), name, Some(std::time::Duration::from_millis(timeout_ms)), move |cancel| async move {
        // Cancelled (task_kill, timeout, rewind): the sandbox kills the
        // command's process group.
        let out = sandbox.run(&argv, &spec, cancel).await.map_err(|e| format!("failed to run command: {e}"))?;
        let mut text = output_text(&out);
        if out.timed_out {
            return Err(format!("{text}[timed out]"));
        }
        push_status(&mut text, &out);
        Ok(text.into_bytes())
    });
    if let Some(t) = trust {
        tasks.set_trust(id, t);
    }
    if !writes.is_empty() {
        tasks.set_writes(id, writes);
    }
    Ok(ToolOutput::text(format!(
        "Started as background task {id} (timeout {timeout_ms} ms). You will be notified when it ends; read its \
         output with task_output, stop it with task_kill."
    )))
}

/// Output of a background command reaches the model later, through
/// `task_output`, without this call's declared accesses: label it untrusted
/// when the command could read untrusted content (the network, MCP, files
/// outside the workspace). Content of an untrusted workspace is not
/// recognized here (the tool does not know the workspace's trust).
fn background_trust(ctx: &ToolCtx, analysis: &ShellAnalysis, spec: &agent_runtime::SandboxSpec) -> Option<agent_proto::Trust> {
    if let Some(net) = spec.network.first() {
        return Some(agent_proto::Trust::Untrusted { source: format!("net:{net}") });
    }
    let ws = ctx.workspace.to_string_lossy();
    let ws = ws.trim_end_matches('/');
    analysis.accesses.iter().find_map(|a| {
        let outside = match a.resource.scheme() {
            Some(agent_proto::Scheme::Net) | Some(agent_proto::Scheme::Mcp) => true,
            Some(agent_proto::Scheme::Fs) => {
                let p = a.resource.rest();
                !(p == ws || p.starts_with(&format!("{ws}/")))
            }
            _ => false,
        };
        outside.then(|| agent_proto::Trust::Untrusted { source: a.resource.as_str().to_string() })
    })
}

fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if out.len() < s.len() {
        out.push_str("...");
    }
    out
}

/// At most 20 paths, then "and N more".
fn list(paths: &[String]) -> String {
    let mut s = paths
        .iter()
        .take(20)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > 20 {
        s.push_str(&format!(" and {} more", paths.len() - 20));
    }
    s
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
    /// Opaque commands run isolated when the sandbox supports it ("execute
    /// isolated, then approve the diff" instead of asking first).
    /// Background commands are never isolated (see the module docs).
    fn isolated(&self, input: &Value) -> bool {
        let background = parse_input(input).is_ok_and(|i| i.background);
        self.isolation && !background && self.class(input) == EffectClass::Opaque
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        self.run(input, ctx).await
    }
    fn adapt(&self, env: &ToolEnv) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(self.adapted(env)))
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
    /// Classify commands with the semantic table once a sandbox is known
    /// to enforce the declarations.
    fn adapt(&self, env: &ToolEnv) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(BashTool::new(true).adapted(env)))
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
