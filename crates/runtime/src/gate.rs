//! Rings 4 (hooks) and 5 (human) of the gate chain, run in the runtime.
//!
//! - [`Hook`]: user code registered for hook points; each returns a verdict.
//!   Results are combined "only tighten"; executor failures follow
//!   [`HookPoint::on_failure`].
//! - [`AutoRule`]: auto-answer rules for ring 5. Only ever consulted for
//!   [`ApprovalLevel::Policy`] questions: invariant-level asks are never
//!   auto-answered.
//! - [`AskBoard`]: per-session board of pending questions. Answers are
//!   compare-and-swap: first answer wins, later ones get
//!   [`AnswerError::AlreadyAnswered`].
//! - [`GateChain`]: the default [`GateExecutor`].

use crate::ports::{GateCtx, GateExecutor};
use agent_proto::*;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

// ---------------------------------------------------------------- ask board

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    #[error("question {0} already answered")]
    AlreadyAnswered(QuestionId),
    #[error("unknown question {0}")]
    Unknown(QuestionId),
}

struct Slot {
    question: Question,
    tx: watch::Sender<Option<(Answer, Responder)>>,
}

/// Per-session pending questions with compare-and-swap answering.
#[derive(Default)]
pub struct AskBoard {
    slots: Mutex<BTreeMap<QuestionId, Slot>>,
}

const MAX_ANSWERED_KEPT: usize = 1024;

impl AskBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a question (idempotent: re-opening keeps any existing answer).
    pub fn open(&self, q: &Question) {
        let mut slots = self.slots.lock().unwrap();
        if !slots.contains_key(&q.id) {
            let (tx, _) = watch::channel(None);
            slots.insert(q.id.clone(), Slot { question: q.clone(), tx });
        }
        if slots.len() > MAX_ANSWERED_KEPT {
            let answered: Vec<QuestionId> =
                slots.iter().filter(|(_, s)| s.tx.borrow().is_some()).map(|(k, _)| k.clone()).collect();
            for k in answered.into_iter().take(slots.len() - MAX_ANSWERED_KEPT) {
                slots.remove(&k);
            }
        }
    }

    /// Compare-and-swap answer: succeeds only for an open, unanswered question.
    pub fn answer(&self, id: &QuestionId, answer: Answer, responder: Responder) -> Result<(), AnswerError> {
        let slots = self.slots.lock().unwrap();
        let slot = slots.get(id).ok_or_else(|| AnswerError::Unknown(id.clone()))?;
        if slot.tx.borrow().is_some() {
            return Err(AnswerError::AlreadyAnswered(id.clone()));
        }
        slot.tx.send_replace(Some((answer, responder)));
        Ok(())
    }

    /// Wait for the winning answer. `None` if the question is closed (removed)
    /// or unknown.
    pub async fn wait(&self, id: &QuestionId) -> Option<(Answer, Responder)> {
        let mut rx = {
            let slots = self.slots.lock().unwrap();
            slots.get(id)?.tx.subscribe()
        };
        let r = rx.wait_for(|v| v.is_some()).await.ok()?.clone();
        r
    }

    /// Unanswered questions (for clients that connect late).
    pub fn pending(&self) -> Vec<Question> {
        self.slots
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.tx.borrow().is_none())
            .map(|s| s.question.clone())
            .collect()
    }

    pub fn is_answered(&self, id: &QuestionId) -> bool {
        self.slots.lock().unwrap().get(id).map(|s| s.tx.borrow().is_some()).unwrap_or(false)
    }

    /// Remove a question (its gate was cancelled). Waiters get `None`.
    pub fn close(&self, id: &QuestionId) {
        self.slots.lock().unwrap().remove(id);
    }
}

/// Map a human answer to the verdict returned to the kernel.
pub fn answer_to_verdict(a: &Answer) -> Verdict {
    match a {
        Answer::Allow { .. } => Verdict::Allow,
        Answer::AllowWith(p) => Verdict::Rewrite(p.clone()),
        Answer::Deny { reason } => Verdict::deny(reason.clone().unwrap_or_else(|| "denied by user".into())),
    }
}

/// `Control::Answer::responder` string -> [`Responder`] ("code" = embedding code).
pub fn human_responder(name: &str) -> Responder {
    if name == "code" {
        Responder::Code
    } else {
        Responder::Human(name.to_string())
    }
}

// ---------------------------------------------------------------- hooks / rules

/// A ring-4 hook (in-process closure, external command, HTTP... adapters
/// implement this). `Err` is an executor failure (see `HookPoint::on_failure`).
#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    /// Hook points this hook runs at.
    fn points(&self) -> Vec<HookPoint>;
    async fn run(&self, req: &GateRequest) -> Result<Verdict, String>;
}

/// A ring-5 auto-answer rule. Only consulted for policy-level questions.
pub trait AutoRule: Send + Sync {
    fn name(&self) -> &str;
    fn answer(&self, req: &GateRequest, question: &Question) -> Option<Answer>;
}

/// Hook from a plain closure.
pub struct FnHook<F> {
    name: String,
    points: Vec<HookPoint>,
    f: F,
}

impl<F> FnHook<F>
where
    F: Fn(&GateRequest) -> Result<Verdict, String> + Send + Sync,
{
    pub fn new(name: impl Into<String>, points: Vec<HookPoint>, f: F) -> Self {
        FnHook { name: name.into(), points, f }
    }
}

#[async_trait]
impl<F> Hook for FnHook<F>
where
    F: Fn(&GateRequest) -> Result<Verdict, String> + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn points(&self) -> Vec<HookPoint> {
        self.points.clone()
    }
    async fn run(&self, req: &GateRequest) -> Result<Verdict, String> {
        (self.f)(req)
    }
}

/// Auto rule from a plain closure.
pub struct FnRule<F> {
    name: String,
    f: F,
}

impl<F> FnRule<F>
where
    F: Fn(&GateRequest, &Question) -> Option<Answer> + Send + Sync,
{
    pub fn new(name: impl Into<String>, f: F) -> Self {
        FnRule { name: name.into(), f }
    }
}

impl<F> AutoRule for FnRule<F>
where
    F: Fn(&GateRequest, &Question) -> Option<Answer> + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn answer(&self, req: &GateRequest, q: &Question) -> Option<Answer> {
        (self.f)(req, q)
    }
}

// ---------------------------------------------------------------- chain

/// The default gate executor.
///
/// Ring `Hook`: runs every hook registered for the point in registration
/// order and combines their verdicts, keeping the strictest (a `Deny` ends
/// evaluation; several `Annotate`s are merged). Hook failures: `Allow` points
/// ignore them, `Block` points deny, `Human` points turn into an ask.
///
/// Ring `Human`: auto-answer rules (policy level only), then either an
/// interactive answer from the session's [`AskBoard`] (`Responder::Human`)
/// or, when unattended, the `OnAsk` fallback (`Responder::Unattended`).
/// Invariant-level asks when unattended are allowed only in a
/// framework-launched disposable environment (`Responder::DisposableEnv`),
/// otherwise deferred.
pub struct GateChain {
    hooks: Vec<Arc<dyn Hook>>,
    rules: Vec<Arc<dyn AutoRule>>,
    unattended: Option<OnAsk>,
    disposable_env: bool,
    hook_timeout: Duration,
}

impl Default for GateChain {
    fn default() -> Self {
        GateChain {
            hooks: vec![],
            rules: vec![],
            unattended: None,
            disposable_env: false,
            hook_timeout: Duration::from_secs(60),
        }
    }
}

impl GateChain {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn hook(mut self, h: Arc<dyn Hook>) -> Self {
        self.hooks.push(h);
        self
    }
    pub fn rule(mut self, r: Arc<dyn AutoRule>) -> Self {
        self.rules.push(r);
        self
    }
    /// `None` = interactive (wait for a client answer).
    pub fn unattended(mut self, on_ask: Option<OnAsk>) -> Self {
        self.unattended = on_ask;
        self
    }
    pub fn disposable_env(mut self, yes: bool) -> Self {
        self.disposable_env = yes;
        self
    }
    pub fn hook_timeout(mut self, d: Duration) -> Self {
        self.hook_timeout = d;
        self
    }
    /// Hook points with at least one hook (for `KernelConfig::hooked`).
    pub fn hooked_points(&self) -> Vec<HookPoint> {
        let mut v: Vec<HookPoint> = self.hooks.iter().flat_map(|h| h.points()).collect();
        v.sort();
        v.dedup();
        v
    }

    async fn run_hooks(&self, req: &GateRequest) -> (Verdict, Responder) {
        let mut best: Option<(Verdict, Responder)> = None;
        for h in self.hooks.iter().filter(|h| h.points().contains(&req.point)) {
            let name = h.name().to_string();
            let r = match tokio::time::timeout(self.hook_timeout, h.run(req)).await {
                Ok(r) => r,
                Err(_) => Err(format!("timed out after {:?}", self.hook_timeout)),
            };
            let v = match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(hook = %name, point = ?req.point, error = %e, "hook failed");
                    match req.point.on_failure() {
                        FailureMode::Allow => continue,
                        FailureMode::Block => Verdict::deny(format!("hook `{name}` failed: {e}")),
                        FailureMode::Human => Verdict::Ask(Question {
                            id: QuestionId(format!("hook-failed:{name}")),
                            prompt: format!("Hook `{name}` failed ({e}). Allow anyway?"),
                            level: ApprovalLevel::Policy,
                            ring: Ring::Hook,
                            rules: vec![name.clone()],
                            remember_destination: None,
                        }),
                    }
                }
            };
            let responder = Responder::Hook(name);
            if matches!(v, Verdict::Deny(_)) {
                return (v, responder);
            }
            best = Some(match best {
                None => (v, responder),
                Some((Verdict::Annotate(a), r0)) if matches!(v, Verdict::Annotate(_)) => {
                    let Verdict::Annotate(b) = v else { unreachable!() };
                    let merged = Context {
                        text: format!("{}\n{}", a.text, b.text),
                        trust: Trust::weakest(&a.trust, &b.trust),
                    };
                    (Verdict::Annotate(merged), r0)
                }
                Some((cur, r0)) => {
                    if v.strictness() > cur.strictness() {
                        (v, responder)
                    } else {
                        (cur, r0)
                    }
                }
            });
        }
        best.unwrap_or((Verdict::Allow, Responder::Kernel))
    }

    async fn ask_human(&self, req: &GateRequest, ctx: Option<&GateCtx>) -> (Verdict, Responder) {
        let question = req.question.clone().unwrap_or_else(|| Question {
            id: QuestionId(format!("gate:{:?}", req.point)),
            prompt: format!("Approve {:?}?", req.point),
            level: req.level,
            ring: Ring::Human,
            rules: vec![],
            remember_destination: None,
        });
        let invariant = req.level == ApprovalLevel::Invariant || question.level == ApprovalLevel::Invariant;

        // Auto-answer rules: never for invariant-level asks.
        let mut auto: Option<(Answer, Responder)> = None;
        if !invariant {
            for r in &self.rules {
                if let Some(a) = r.answer(req, &question) {
                    auto = Some((a, Responder::AutoRule(r.name().to_string())));
                    break;
                }
            }
        }

        if let Some(ctx) = ctx {
            ctx.asks.open(&question);
            if let Some((a, resp)) = auto {
                // CAS like everyone else: a human may have answered first.
                let _ = ctx.asks.answer(&question.id, a, resp);
            }
        } else if let Some((a, resp)) = auto {
            return (answer_to_verdict(&a), resp);
        }

        let unattended_fallback = |this: &Self| -> (Verdict, Responder) {
            if invariant {
                if this.disposable_env {
                    (Verdict::Allow, Responder::DisposableEnv)
                } else {
                    (Verdict::Defer, Responder::Unattended)
                }
            } else {
                match this.unattended.unwrap_or(OnAsk::Defer) {
                    OnAsk::Allow => (Verdict::Allow, Responder::Unattended),
                    OnAsk::Deny => (Verdict::deny("unattended: approval required"), Responder::Unattended),
                    OnAsk::Defer => (Verdict::Defer, Responder::Unattended),
                }
            }
        };

        match ctx {
            None => unattended_fallback(self),
            Some(ctx) => {
                if self.unattended.is_some() && !ctx.asks.is_answered(&question.id) {
                    let (v, resp) = unattended_fallback(self);
                    let locked = match &v {
                        Verdict::Allow => ctx.asks.answer(&question.id, Answer::Allow { remember: false }, resp.clone()),
                        Verdict::Deny(r) => {
                            ctx.asks.answer(&question.id, Answer::Deny { reason: Some(r.0.clone()) }, resp.clone())
                        }
                        _ => {
                            // Deferred: re-evaluated on resume, keep nothing.
                            ctx.asks.close(&question.id);
                            Ok(())
                        }
                    };
                    if locked.is_ok() {
                        return (v, resp);
                    }
                    // Lost the race to a real answer: fall through and use it.
                }
                tokio::select! {
                    r = ctx.asks.wait(&question.id) => match r {
                        Some((a, resp)) => (answer_to_verdict(&a), resp),
                        None => (Verdict::Defer, Responder::Kernel),
                    },
                    _ = ctx.cancel.cancelled() => (Verdict::Defer, Responder::Kernel),
                }
            }
        }
    }
}

#[async_trait]
impl GateExecutor for GateChain {
    /// Without a session context there is nobody to ask: ring 5 uses the
    /// unattended fallback.
    async fn evaluate(&self, req: &GateRequest) -> (Verdict, Responder) {
        match req.ring {
            Ring::Human => self.ask_human(req, None).await,
            _ => self.run_hooks(req).await,
        }
    }

    async fn evaluate_in(&self, req: &GateRequest, ctx: &GateCtx) -> (Verdict, Responder) {
        match req.ring {
            Ring::Human => self.ask_human(req, Some(ctx)).await,
            _ => self.run_hooks(req).await,
        }
    }
}
