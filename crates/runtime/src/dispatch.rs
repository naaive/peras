//! Effect execution: sampling, tool batches, gates, checkpoints, compaction.
//! Each function runs inside a spawned task and returns the `EffectResult`
//! the driver feeds back as `Input::Completed`.

use crate::assemble::Assembler;
use crate::gate::AskBoard;
use crate::ports::*;
use crate::registry::ToolRegistry;
use agent_proto::*;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Runtime tunables.
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    pub workspace: PathBuf,
    /// Text tool outputs above this many bytes are spilled to the blob store.
    pub inline_limit_bytes: usize,
    /// Bytes of head and of tail kept in a spilled output's preview.
    pub preview_bytes: usize,
    /// Pulse broadcast capacity (lossy).
    pub pulse_capacity: usize,
    /// Recent envelopes kept in memory for live subscribers.
    pub recent_capacity: usize,
    /// Idempotency keys remembered per session.
    pub dedupe_capacity: usize,
    /// Observers re-receive the whole journal when a session is resumed
    /// (at-least-once without a persisted cursor).
    pub observer_replay_on_resume: bool,
    /// Sessions whose crash-recovered Network/Irreversible/Opaque calls should
    /// be re-asked rather than reported. Currently informational: see
    /// `recover_batch` (we always report, so the model re-issues through the
    /// normal, journaled gate path).
    pub interactive: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        RuntimeOptions {
            workspace: PathBuf::from("/workspace"),
            inline_limit_bytes: 32 * 1024,
            preview_bytes: 2 * 1024,
            pulse_capacity: 1024,
            recent_capacity: 1024,
            dedupe_capacity: 256,
            observer_replay_on_resume: true,
            interactive: true,
        }
    }
}

/// The services effects are dispatched through.
pub struct Env {
    pub journal: Arc<dyn JournalStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub model: Arc<dyn ModelPort>,
    pub tools: Arc<ToolRegistry>,
    pub gates: Arc<dyn GateExecutor>,
    pub checkpointer: Arc<dyn Checkpointer>,
    pub sandbox: Arc<dyn SandboxPort>,
    pub secrets: Arc<dyn SecretSource>,
    pub memory: Option<Arc<dyn MemoryStore>>,
    pub subagents: Option<Arc<dyn SubagentSpawner>>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub observers: Vec<Arc<dyn Observer>>,
    pub options: RuntimeOptions,
}

/// Where effect tasks send their outputs.
pub trait EffectSink: Send + Sync + 'static {
    fn streamed(&self, id: EffectId, call: ToolCall);
}

// ---------------------------------------------------------------- sample

/// Sample the model, assembling into `asm` (shared with the driver so a hard
/// interrupt can take the partial message). `None` when cancelled.
pub async fn sample(
    env: &Env,
    id: EffectId,
    prompt: &Prompt,
    asm: Arc<Mutex<Assembler>>,
    cancel: &CancellationToken,
    pulses: &broadcast::Sender<Pulse>,
    sink: Option<&dyn EffectSink>,
) -> Option<Result<AssistantMessage, ModelError>> {
    let model = env.model.clone();
    let req = model.encoder().encode(&prompt.head, &prompt.body, prompt.max_tokens);
    let mut stream = model.stream(req);
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            d = stream.next() => d,
        };
        match next {
            None => break,
            Some(Err(e)) => return Some(Err(e)),
            Some(Ok(delta)) => {
                let pushed = asm.lock().unwrap().push(delta, &env.tools);
                if let Some(t) = pushed.pulse_text {
                    let _ = pulses.send(Pulse::TextDelta { effect: id, text: t });
                }
                if let Some(t) = pushed.pulse_thinking {
                    let _ = pulses.send(Pulse::ThinkingDelta { effect: id, text: t });
                }
                if let (Some(call), Some(sink)) = (pushed.call, sink) {
                    sink.streamed(id, call);
                }
            }
        }
    }
    if cancel.is_cancelled() {
        return None;
    }
    let msg = asm.lock().unwrap().finish();
    Some(Ok(msg))
}

pub async fn compact(
    env: &Env,
    id: EffectId,
    job: &CompactJob,
    cancel: &CancellationToken,
) -> Option<EffectResult> {
    let asm = Arc::new(Mutex::new(Assembler::new()));
    // No streamed tool calls / pulses for summaries: use a dummy channel.
    let (quiet, _) = broadcast::channel(1);
    match sample(env, id, &job.prompt, asm, cancel, &quiet, None).await? {
        Ok(msg) => Some(EffectResult::Compacted { summary: msg.text(), trust: Trust::Internal }),
        Err(e) => Some(EffectResult::CompactFailed(e)),
    }
}

// ---------------------------------------------------------------- execute

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Head + tail preview of a long text.
pub fn preview(text: &str, bytes: usize) -> String {
    if text.len() <= bytes * 2 {
        return text.to_string();
    }
    let head = &text[..floor_char(text, bytes)];
    let tail = &text[ceil_char(text, text.len() - bytes)..];
    let omitted = text.len() - head.len() - tail.len();
    format!("{head}\n[... {omitted} bytes omitted ...]\n{tail}")
}

/// Spill text contents above the inline limit to the blob store.
pub async fn spill(env: &Env, content: Vec<ToolContent>) -> Result<Vec<ToolContent>, StoreError> {
    let mut out = Vec::with_capacity(content.len());
    for c in content {
        match c {
            ToolContent::Text { text } if text.len() > env.options.inline_limit_bytes => {
                let blob = env.blobs.put(text.as_bytes(), Some("text/plain; charset=utf-8")).await?;
                out.push(ToolContent::Blob { blob, preview: preview(&text, env.options.preview_bytes) });
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Trust of a result: the tool's own label, else untrusted when any granted
/// access reaches the network or an MCP server, else internal.
pub fn derive_trust(tool: &str, declared: Option<Trust>, grants: &[Access]) -> Trust {
    if let Some(t) = declared {
        return t;
    }
    if grants.iter().any(|a| matches!(a.resource.scheme(), Some(Scheme::Net) | Some(Scheme::Mcp))) {
        Trust::Untrusted { source: tool.to_string() }
    } else {
        Trust::Internal
    }
}

/// Run one call with only its grants. `Err` = infrastructure error.
pub async fn run_call(
    env: &Env,
    session: &SessionId,
    call: &ToolCall,
    grants: Vec<Access>,
    cancel: CancellationToken,
    pulses: &broadcast::Sender<Pulse>,
) -> Result<ToolResult, String> {
    let Some(tool) = env.tools.get(&call.name).cloned() else {
        return Ok(ToolResult::text(call.id.clone(), format!("Unknown tool `{}`.", call.name), true));
    };
    let pulses_tx = pulses.clone();
    let call_name = call.id.0.clone();
    let progress: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |m: String| {
        let _ = pulses_tx.send(Pulse::ToolProgress { call: call_name.clone(), message: m });
    });
    let ctx = ToolCtx {
        call_id: call.id.clone(),
        session: session.clone(),
        workspace: env.options.workspace.clone(),
        grants: grants.clone(),
        cancel,
        blobs: env.blobs.clone(),
        sandbox: env.sandbox.clone(),
        memory: env.memory.clone(),
        secrets: env.secrets.clone(),
        progress,
        subagents: env.subagents.clone(),
    };
    match tool.call(call.input.clone(), ctx).await {
        Ok(out) => {
            let content = spill(env, out.content).await.map_err(|e| e.to_string())?;
            Ok(ToolResult {
                call_id: call.id.clone(),
                content,
                is_error: false,
                trust: derive_trust(&call.name, out.trust, &grants),
                observed: out.observed,
            })
        }
        Err(ToolError::Infra(e)) => Err(e),
        Err(e) => {
            let mut r = ToolResult::text(call.id.clone(), e.to_string(), true);
            r.trust = derive_trust(&call.name, None, &grants);
            Ok(r)
        }
    }
}

fn grants_for(batch: &Batch, call: &CallId) -> Vec<Access> {
    batch.grants.iter().find(|(c, _)| c == call).map(|(_, g)| g.clone()).unwrap_or_default()
}

/// Run a batch concurrently; results in call order.
pub async fn execute(
    env: &Env,
    session: &SessionId,
    batch: &Batch,
    cancel: &CancellationToken,
    pulses: &broadcast::Sender<Pulse>,
) -> Option<EffectResult> {
    let futs = batch.calls.iter().map(|c| run_call(env, session, c, grants_for(batch, &c.id), cancel.child_token(), pulses));
    let results = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        r = futures::future::join_all(futs) => r,
    };
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        match r {
            Ok(r) => out.push(r),
            Err(e) => return Some(EffectResult::Failed { error: e }),
        }
    }
    Some(EffectResult::Executed(out))
}

/// Crash-recovery text for calls that are not re-run.
pub const NOT_RERUN: &str = "Interrupted by a crash; not re-run because it may have had external side effects. Please confirm before retrying.";

/// Crash recovery of an outstanding batch, per side-effect class:
/// Pure -> re-run; LocalWrite -> restore originals (best effort) then re-run;
/// Network / Irreversible / Opaque -> not run, error result [`NOT_RERUN`].
///
/// Choice: we never ask through the `Asker` here even in interactive sessions,
/// because such an ask would not be journaled as a `QuestionAsked` event. The
/// error result reaches the model, which re-issues the call through the
/// kernel's normal (journaled, gated) path if it still wants it.
pub async fn recover_batch(
    env: &Env,
    session: &SessionId,
    batch: &Batch,
    cancel: &CancellationToken,
    pulses: &broadcast::Sender<Pulse>,
) -> Option<EffectResult> {
    let mut out = Vec::with_capacity(batch.calls.len());
    // Sequential: recovery is rare and ordering keeps restore-then-rerun simple.
    for call in &batch.calls {
        if cancel.is_cancelled() {
            return None;
        }
        let grants = grants_for(batch, &call.id);
        let r = match call.class {
            EffectClass::Pure => run_call(env, session, call, grants, cancel.child_token(), pulses).await,
            EffectClass::LocalWrite => {
                let writes: Vec<Access> = grants.iter().filter(|a| a.mode == AccessMode::Write).cloned().collect();
                if let Err(e) = env.checkpointer.restore_originals(&writes).await {
                    tracing::warn!(call = %call.id, error = %e, "restore originals failed; re-running anyway");
                }
                run_call(env, session, call, grants, cancel.child_token(), pulses).await
            }
            EffectClass::Network | EffectClass::Irreversible | EffectClass::Opaque => {
                Ok(ToolResult::text(call.id.clone(), NOT_RERUN, true))
            }
        };
        match r {
            Ok(r) => out.push(r),
            Err(e) => return Some(EffectResult::Failed { error: e }),
        }
    }
    Some(EffectResult::Executed(out))
}

// ---------------------------------------------------------------- gate / checkpoint

pub async fn gate(env: &Env, req: &GateRequest, ctx: GateCtx) -> Option<EffectResult> {
    let cancel = ctx.cancel.clone();
    let (verdict, responder) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return None,
        r = env.gates.evaluate_in(req, &ctx) => r,
    };
    Some(EffectResult::Gated { verdict, responder, remember: false })
}

pub async fn checkpoint(env: &Env, scope: &CheckpointScope) -> EffectResult {
    if !scope.declared_writes.is_empty() {
        if let Err(e) = env.checkpointer.save_originals(&scope.declared_writes).await {
            return EffectResult::Failed { error: format!("save originals: {e}") };
        }
    }
    match env.checkpointer.checkpoint(scope).await {
        Ok(info) => EffectResult::Checkpointed(info),
        Err(e) => EffectResult::Failed { error: format!("checkpoint: {e}") },
    }
}

pub async fn restore(env: &Env, plan: &RestorePlan) -> EffectResult {
    match env.checkpointer.restore(plan).await {
        Ok(r) => EffectResult::Restored(r),
        Err(e) => EffectResult::Failed { error: format!("restore: {e}") },
    }
}

/// Everything a spawned effect task needs about its session.
#[derive(Clone)]
pub struct EffectCtx {
    pub session: SessionId,
    pub asks: Arc<AskBoard>,
    pub pulses: broadcast::Sender<Pulse>,
}
