//! A sub-agent is a tool: `Agent::new(..).tools((read,)).describe("Review the diff")`
//! can be passed to another agent's `.tools(..)`, and every sub-agent
//! definition file (`.agent/agents/<name>.md`) is registered as one.
//!
//! The child runs in its own session whose id is derived from the parent
//! session and the call id, so recovery always finds the same child (a
//! re-dispatched call resumes it instead of starting over). Its permissions
//! can only be narrower than the parent's (design: Sub-agents):
//!
//! - tools are intersected with the parent's, the parent's policy rules apply
//!   (the child's own allow rules cannot widen them), security lists only
//!   tighten;
//! - the budget is carved out of the parent's remaining budget, and what the
//!   child consumed is charged back to the parent with its result;
//! - approval mode and execution mode come from the parent; questions the
//!   child asks are forwarded to the parent session's question board (clients
//!   of the parent see and answer them);
//! - taint travels both ways: the parent's taint is inherited at spawn, a
//!   tainted child's output is untrusted.
//!
//! Modes: new (a blank session; the task must be self-contained) or fork
//! (`Agent::fork(true)` / `mode: fork`): seeded with the parent's completed
//! turns, reusing its cached prefix. A call with `"background": true` runs the
//! child as a background task (listed by `task_list`, its end notified).

use crate::agent::{Agent, Config, JournalChoice, ModelChoice};
use crate::run::outcome_to_result;
use agent_kernel::{Kernel, Phase, SessionStart};
use agent_profile::Profile;
use agent_proto::*;
use agent_runtime::*;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

/// Links sub-agent tools to the runtime that runs the parent session (set as
/// the parent runtime's [`SubagentSpawner`]).
#[derive(Default)]
pub(crate) struct Link {
    rt: OnceLock<Runtime<Kernel>>,
    /// Sub-agent definitions by name (replaced on hot reload).
    agents: std::sync::RwLock<BTreeMap<String, Agent>>,
}

impl Link {
    pub(crate) fn attach(&self, rt: Runtime<Kernel>) {
        let _ = self.rt.set(rt);
    }

    pub(crate) fn set_agents(&self, agents: BTreeMap<String, Agent>) {
        *self.agents.write().unwrap_or_else(|e| e.into_inner()) = agents;
    }

    pub(crate) fn session(&self, id: &SessionId) -> Option<SessionHandle<Kernel>> {
        self.rt.get()?.session(id)
    }

    pub(crate) fn metrics(&self) -> Option<Arc<Metrics>> {
        self.rt.get().map(|r| r.metrics().clone())
    }
}

#[async_trait]
impl SubagentSpawner for Link {
    /// Run the named sub-agent definition in session `child` to completion.
    async fn run_child(
        &self,
        child: SessionId,
        agent: &str,
        task: String,
        tainted_input: bool,
    ) -> Result<(TurnOutcome, bool), ToolError> {
        let a = self
            .agents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent)
            .cloned()
            .ok_or_else(|| ToolError::Failed(format!("unknown sub-agent `{agent}`")))?;
        let taint = tainted_input.then(|| Taint { tainted: true, labels: ["parent".to_string()].into(), ..Default::default() });
        let start = SessionStart { taint, ..Default::default() };
        let h = a.open_with(&child, start, |_| {}).await.map_err(|e| ToolError::Infra(e.to_string()))?;
        let outcome = follow(&h, Some(task), None, &a.cfg.name, &CancellationToken::new()).await;
        Ok((outcome, h.with_state(agent_kernel::is_tainted)))
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

use agent_kernel::Taint;

/// Child agents for the profile's sub-agent definitions. They share the
/// parent's model port, journal and sandbox; their tools are the parent's
/// (minus sub-agents) narrowed by the definition's `tools` list.
pub(crate) fn from_definitions(
    cfg: &Config,
    profile: &Profile,
    base: &[Arc<dyn Tool>],
    model: Arc<dyn ModelPort>,
    journal: JournalChoice,
    sandbox: (Arc<dyn SandboxPort>, bool),
) -> BTreeMap<String, Agent> {
    let mut out = BTreeMap::new();
    for def in &profile.agents {
        let Some(child) = profile.child(&def.name) else { continue };
        let tools: Vec<Arc<dyn Tool>> = base
            .iter()
            .filter(|t| {
                let s = t.spec();
                !s.subagent && child.tool_allowlist.as_ref().is_none_or(|a| a.contains(&s.name))
            })
            .cloned()
            .collect();
        let mut c = cfg.clone();
        c.model = match &cfg.model {
            ModelChoice::FromProfile if def.model.is_some() => ModelChoice::FromProfile,
            _ => ModelChoice::Port(model.clone()),
        };
        c.tools = Some(tools);
        c.preset = Some(child);
        c.journal = Some(journal.clone());
        c.sandbox = Some(sandbox.clone());
        c.name = def.name.clone();
        c.description = def.description.clone();
        c.fork = def.fork;
        // The parent's checkpointer covers the workspace; the parent's
        // reload covers the configuration (a child's sessions keep the
        // configuration narrowed at spawn).
        c.shadow = false;
        c.hot_reload = false;
        out.insert(def.name.clone(), Agent::from_config(c));
    }
    out
}

/// The child's configuration, narrowed to the parent's (see module docs).
fn narrow(kc: &mut KernelConfig, parent: &KernelConfig, budgets: Budgets) {
    let names: BTreeSet<&str> = parent.tools.iter().map(|t| t.name.as_str()).collect();
    let (kept, dropped): (Vec<ToolSpec>, Vec<ToolSpec>) =
        std::mem::take(&mut kc.tools).into_iter().partition(|t| names.contains(t.name.as_str()));
    kc.tools = kept;
    let mut rules = parent.rules.clone();
    // The child's registry still has the dropped tools: deny calling them.
    for t in dropped {
        rules.push(PolicyRule {
            name: format!("subagent:not-granted:{}", t.name),
            resource: None,
            tool: Some(t.name),
            mode: None,
            action: PolicyAction::Deny,
            layer: Layer::Managed,
        });
    }
    for r in &kc.rules {
        if r.action != PolicyAction::Allow && !rules.contains(r) {
            rules.push(r.clone());
        }
    }
    rules.sort_by_key(|r| (std::cmp::Reverse(r.action), r.layer));
    kc.rules = rules;
    kc.budgets = budgets;
    kc.unattended = parent.unattended;
    kc.read_only_mode |= parent.read_only_mode;
    let (c, p) = (&mut kc.security, &parent.security);
    let union = |a: &mut Vec<String>, b: &[String]| {
        for x in b {
            if !a.contains(x) {
                a.push(x.clone());
            }
        }
    };
    union(&mut c.private, &p.private);
    union(&mut c.untrusted, &p.untrusted);
    union(&mut c.persistence, &p.persistence);
    union(&mut c.self_config, &p.self_config);
    c.egress_allow.retain(|x| p.egress_allow.contains(x));
    c.trusted_sources.retain(|x| p.trusted_sources.contains(x));
    c.workspace_trusted &= p.workspace_trusted;
    c.disposable_env &= p.disposable_env;
}

/// What the parent session hands its child at spawn.
struct FromParent {
    handle: SessionHandle<Kernel>,
    config: KernelConfig,
    budgets: Budgets,
    taint: Option<Taint>,
    fork: Option<agent_kernel::ForkSeed>,
}

fn from_parent(link: &Link, session: &SessionId, fork: bool, own: &Budgets) -> Result<Option<FromParent>, ToolError> {
    let Some(handle) = link.session(session) else { return Ok(None) };
    let parent = handle.with_state(|s| {
        let config = agent_kernel::config(s).cloned()?;
        let budgets = agent_kernel::child_budgets(s, own);
        let taint = agent_kernel::inherited_taint(s);
        let fork = if fork { agent_kernel::fork_seed(s) } else { None };
        Some((config, budgets, taint, fork))
    });
    let Some((config, budgets, taint, fork)) = parent else { return Ok(None) };
    let budgets = budgets.ok_or_else(|| ToolError::Failed("budget exhausted: nothing left to delegate to a sub-agent".into()))?;
    Ok(Some(FromParent { handle, config, budgets, taint, fork }))
}

/// How a (possibly resumed) child session stands.
enum Resume {
    /// The task still has to be sent.
    Fresh,
    /// A turn is running: wait for its end.
    Running,
    /// The task's turn already ended.
    Ended(TurnOutcome),
}

fn resume_state(events: &[Envelope<Event>]) -> Resume {
    let started = events.iter().rposition(|e| matches!(e.body, Event::TurnStarted { .. }));
    let ended = events.iter().rposition(|e| matches!(e.body, Event::TurnEnded { .. }));
    match (started, ended) {
        (None, _) => Resume::Fresh,
        // A suspended child ended its turn as far as the parent is concerned
        // (the same outcome the first dispatch would have reported).
        (Some(s), Some(e)) if e > s => match &events[e].body {
            Event::TurnEnded { outcome } => Resume::Ended(outcome.clone()),
            _ => unreachable!(),
        },
        _ => Resume::Running,
    }
}

/// Drive the child until its turn ends: send `task` (if any), forward its
/// questions to the parent's board (deny them when there is no parent), and
/// hard-interrupt it on `cancel`.
pub(crate) async fn follow(
    child: &SessionHandle<Kernel>,
    task: Option<String>,
    parent: Option<&SessionHandle<Kernel>>,
    name: &str,
    cancel: &CancellationToken,
) -> TurnOutcome {
    let mut events = child.subscribe(child.next_seq());
    match task {
        Some(text) => {
            if let Err(e) = child.send(Input::Signal(Signal::Submit { text, attachments: vec![] })).await {
                return TurnOutcome::Failed { error: e.to_string() };
            }
        }
        None if child.with_state(|s| agent_kernel::phase(s) == Phase::Idle) => {
            if let Some(o) = child.last_outcome() {
                return o;
            }
        }
        None => {}
    }
    // Questions open on the child, forwarded to the parent's board.
    let mut forwarded: BTreeMap<QuestionId, (QuestionId, tokio::task::JoinHandle<()>)> = BTreeMap::new();
    // Questions asked before we subscribed (a resumed child).
    for q in child.with_state(agent_kernel::pending_questions) {
        forward(child, parent, name, q, &mut forwarded).await;
    }
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            _ = cancel.cancelled(), if !interrupted => {
                interrupted = true;
                let _ = child.send(Input::Control(Control::HardInterrupt)).await;
            }
            ev = events.next() => match ev {
                None => break TurnOutcome::Failed { error: "sub-agent session closed".into() },
                Some(ev) => match ev.body {
                    Event::QuestionAsked { question, .. } => forward(child, parent, name, question, &mut forwarded).await,
                    Event::QuestionAnswered { question, .. } => {
                        if let (Some((fwd, task)), Some(p)) = (forwarded.remove(&question), parent) {
                            task.abort();
                            p.asks().close(&fwd);
                        }
                    }
                    Event::TurnEnded { outcome } => break outcome,
                    _ => {}
                },
            }
        }
    };
    for (_, (fwd, task)) in forwarded {
        task.abort();
        if let Some(p) = parent {
            p.asks().close(&fwd);
        }
    }
    outcome
}

/// Put a child's question on the parent's board; the first answer there is
/// relayed to the child (with its responder).
async fn forward(
    child: &SessionHandle<Kernel>,
    parent: Option<&SessionHandle<Kernel>>,
    name: &str,
    q: Question,
    forwarded: &mut BTreeMap<QuestionId, (QuestionId, tokio::task::JoinHandle<()>)>,
) {
    if forwarded.contains_key(&q.id) {
        return;
    }
    let Some(p) = parent else {
        let deny = Answer::Deny { reason: Some("nobody can answer the sub-agent's question".into()) };
        let _ = child.answer(q.id.clone(), deny, "code").await;
        return;
    };
    let fwd = Question {
        id: QuestionId(format!("{FORWARDED_QUESTION_PREFIX}{}:{}", child.id(), q.id)),
        prompt: format!("[sub-agent {name}] {}", q.prompt),
        ..q.clone()
    };
    let board = p.asks().clone();
    board.open(&fwd);
    let (child2, qid, fid) = (child.clone(), q.id.clone(), fwd.id.clone());
    let task = tokio::spawn(async move {
        if let Some((answer, responder)) = board.wait(&fid).await {
            let who = match responder {
                Responder::Human(n) => n,
                _ => "code".to_string(),
            };
            let _ = child2.answer(qid, answer, &who).await;
        }
    });
    forwarded.insert(q.id, (fwd.id, task));
}

#[async_trait]
impl Tool for Agent {
    fn spec(&self) -> ToolSpec {
        let description = if self.cfg.description.is_empty() {
            format!("Delegate a self-contained task to the `{}` sub-agent.", self.cfg.name)
        } else {
            self.cfg.description.clone()
        };
        let task = if self.cfg.fork {
            "The task. The sub-agent has seen the conversation up to the current turn."
        } else {
            "A self-contained task description."
        };
        ToolSpec {
            name: self.cfg.name.clone(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": task },
                    "background": { "type": "boolean", "description": "Run in the background; you are notified when it finishes (read the result with task_output)." }
                },
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
        let background = input.get("background").and_then(|b| b.as_bool()).unwrap_or(false);
        let child_id = ctx.session.child(&ctx.call_id);
        let built = self.built().await.map_err(|e| ToolError::Infra(e.to_string()))?;
        let own = built.compiled().config.budgets;
        let link = ctx.subagents.as_ref().and_then(|s| s.as_any()).and_then(|a| a.downcast_ref::<Link>());
        let parent = match link {
            Some(l) => from_parent(l, &ctx.session, self.cfg.fork, &own)?,
            None => None,
        };
        let events = built.rt.env().journal.load(&child_id, 0).await.map_err(|e| ToolError::Infra(e.to_string()))?;
        let resume = resume_state(&events);
        let (start, parent_cfg, parent_handle) = match parent {
            Some(p) => {
                let start = SessionStart { parent: Some(ctx.session.clone()), taint: p.taint, fork: p.fork, memory: None };
                (start, Some((p.config, p.budgets)), Some(p.handle))
            }
            None => (SessionStart { parent: Some(ctx.session.clone()), ..Default::default() }, None, None),
        };
        let child = self
            .open_with(&child_id, start, |kc| {
                if let Some((pc, budgets)) = &parent_cfg {
                    narrow(kc, pc, budgets.clone());
                }
            })
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?;
        let task = match resume {
            Resume::Fresh => Some(task),
            Resume::Running => None,
            Resume::Ended(o) => return Ok(self.output(&child, o)),
        };
        if background {
            let Some(tasks) = ctx.tasks.clone() else {
                return Err(ToolError::Failed("background sub-agents are not available here".into()));
            };
            let (me, name) = (self.clone(), format!("sub-agent {}", self.cfg.name));
            let id_cell: Arc<OnceLock<TaskId>> = Arc::default();
            let (cell, reg) = (id_cell.clone(), tasks.clone());
            let id = tasks.spawn_for(Some(ctx.session.clone()), name, None, move |cancel| async move {
                let outcome = follow(&child, task, parent_handle.as_ref(), &me.cfg.name, &cancel).await;
                if child.with_state(agent_kernel::is_tainted) {
                    if let Some(id) = cell.get() {
                        reg.set_trust(*id, Trust::Untrusted { source: format!("subagent:{}", me.cfg.name) });
                    }
                }
                outcome_to_result(child.id(), outcome).map(String::into_bytes).map_err(|e| e.to_string())
            });
            let _ = id_cell.set(id);
            // The child writes to the workspace while it runs: its changes are
            // the agent's, and a rewind stops it first.
            tasks.set_writes(id, self.access(&input, &AccessCtx { workspace: ctx.workspace.clone() })?);
            return Ok(ToolOutput::text(format!(
                "Started sub-agent `{}` as background task {id}. You will be notified when it finishes; read its answer with task_output.",
                self.cfg.name
            )));
        }
        let outcome = follow(&child, task, parent_handle.as_ref(), &self.cfg.name, &ctx.cancel).await;
        Ok(self.output(&child, outcome))
    }
}

impl Agent {
    /// The call's result: the child's answer (or what went wrong), its trust,
    /// and the report the parent's kernel records (outcome, usage).
    fn output(&self, child: &SessionHandle<Kernel>, outcome: TurnOutcome) -> ToolOutput {
        let (tainted, (tokens, cost_micros)) = child.with_state(|s| (agent_kernel::is_tainted(s), agent_kernel::usage(s)));
        let text = match outcome_to_result(child.id(), outcome.clone()) {
            Ok(t) => t,
            Err(e) => format!("Sub-agent `{}` did not finish: {e}", self.cfg.name),
        };
        ToolOutput {
            content: vec![ToolContent::Text { text }],
            trust: tainted.then(|| Trust::Untrusted { source: format!("subagent:{}", self.cfg.name) }),
            observed: vec![],
            staged: vec![],
            subagent: Some(Box::new(SubagentReport { child: child.id().clone(), outcome, tokens, cost_micros })),
        }
    }
}
