//! The driver: one actor task per session.
//!
//! For each input: `D::decide(state, clock.now(), input)` -> drafts become
//! envelopes (id from [`IdGen`], consecutive seqs, `at` from the clock,
//! `Parent::Head` -> current branch head) -> appended to the journal with the
//! session lease and expected seq (**log first**) -> `D::evolve` -> published to
//! subscribers -> **then** effects are dispatched as tasks whose results come
//! back as `Input::Completed`. Rejections write nothing.
//!
//! State snapshots are not written: the kernel state has no serialization
//! contract yet, so load = fold the whole journal.

use crate::assemble::Assembler;
use crate::dispatch::{self, EffectSink, Env, RuntimeOptions};
use crate::gate::{human_responder, AnswerError, AskBoard, GateChain};
use crate::mem::*;
use crate::ports::*;
use crate::registry::ToolRegistry;
use agent_kernel::{Decider, Decision};
use agent_proto::*;
use futures::stream::{self, BoxStream, StreamExt};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- errors / acks

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    /// The kernel refused the input; nothing was written.
    #[error("rejected: {0}")]
    Rejected(String),
    /// Another driver owns the session (stale lease / seq conflict). The
    /// session handle is dead; resume elsewhere.
    #[error("fenced: {0}")]
    Fenced(String),
    #[error("store: {0}")]
    Store(String),
    #[error(transparent)]
    Answer(#[from] AnswerError),
    #[error("session {0} already exists")]
    AlreadyExists(SessionId),
    #[error("session {0} not found")]
    NotFound(SessionId),
    #[error("session closed")]
    Closed,
}

impl From<StoreError> for DriverError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::SeqConflict { .. } | StoreError::StaleLease { .. } => DriverError::Fenced(e.to_string()),
            other => DriverError::Store(other.to_string()),
        }
    }
}

/// Result of an accepted input / command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Events appended for it.
    pub events: usize,
    /// Next seq after it.
    pub next_seq: Seq,
    /// Effects dispatched.
    pub effects: usize,
}

type Reply = oneshot::Sender<Result<Applied, DriverError>>;

enum Msg {
    Input(Input, Option<Reply>),
    Command(String, Command, Reply),
    Streamed(EffectId, ToolCall),
    Completed(EffectId, EffectResult),
    Shutdown(oneshot::Sender<()>),
}

// ---------------------------------------------------------------- runtime

/// The runtime: shared services plus a registry of live sessions.
pub struct Runtime<D: Decider> {
    env: Arc<Env>,
    sessions: Arc<Mutex<HashMap<SessionId, SessionHandle<D>>>>,
    _d: PhantomData<fn() -> D>,
}

impl<D: Decider> Clone for Runtime<D> {
    fn clone(&self) -> Self {
        Runtime { env: self.env.clone(), sessions: self.sessions.clone(), _d: PhantomData }
    }
}

pub struct RuntimeBuilder<D> {
    journal: Option<Arc<dyn JournalStore>>,
    blobs: Option<Arc<dyn BlobStore>>,
    model: Option<Arc<dyn ModelPort>>,
    tools: Option<ToolRegistry>,
    gates: Option<Arc<dyn GateExecutor>>,
    checkpointer: Option<Arc<dyn Checkpointer>>,
    sandbox: Option<Arc<dyn SandboxPort>>,
    secrets: Option<Arc<dyn SecretSource>>,
    memory: Option<Arc<dyn MemoryStore>>,
    subagents: Option<Arc<dyn SubagentSpawner>>,
    clock: Option<Arc<dyn Clock>>,
    ids: Option<Arc<dyn IdGen>>,
    observers: Vec<Arc<dyn Observer>>,
    options: RuntimeOptions,
    _d: PhantomData<fn() -> D>,
}

impl<D> Default for RuntimeBuilder<D> {
    fn default() -> Self {
        RuntimeBuilder {
            journal: None,
            blobs: None,
            model: None,
            tools: None,
            gates: None,
            checkpointer: None,
            sandbox: None,
            secrets: None,
            memory: None,
            subagents: None,
            clock: None,
            ids: None,
            observers: vec![],
            options: RuntimeOptions::default(),
            _d: PhantomData,
        }
    }
}

impl<D: Decider + 'static> RuntimeBuilder<D>
where
    D::State: Send + Sync + 'static,
{
    pub fn journal(mut self, j: Arc<dyn JournalStore>) -> Self {
        self.journal = Some(j);
        self
    }
    pub fn blobs(mut self, b: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(b);
        self
    }
    pub fn model(mut self, m: Arc<dyn ModelPort>) -> Self {
        self.model = Some(m);
        self
    }
    pub fn tools(mut self, t: ToolRegistry) -> Self {
        self.tools = Some(t);
        self
    }
    pub fn gates(mut self, g: Arc<dyn GateExecutor>) -> Self {
        self.gates = Some(g);
        self
    }
    pub fn checkpointer(mut self, c: Arc<dyn Checkpointer>) -> Self {
        self.checkpointer = Some(c);
        self
    }
    pub fn sandbox(mut self, s: Arc<dyn SandboxPort>) -> Self {
        self.sandbox = Some(s);
        self
    }
    pub fn secrets(mut self, s: Arc<dyn SecretSource>) -> Self {
        self.secrets = Some(s);
        self
    }
    pub fn memory(mut self, m: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(m);
        self
    }
    pub fn subagents(mut self, s: Arc<dyn SubagentSpawner>) -> Self {
        self.subagents = Some(s);
        self
    }
    pub fn clock(mut self, c: Arc<dyn Clock>) -> Self {
        self.clock = Some(c);
        self
    }
    pub fn ids(mut self, i: Arc<dyn IdGen>) -> Self {
        self.ids = Some(i);
        self
    }
    pub fn observer(mut self, o: Arc<dyn Observer>) -> Self {
        self.observers.push(o);
        self
    }
    pub fn options(mut self, o: RuntimeOptions) -> Self {
        self.options = o;
        self
    }

    /// Never fails: missing ports get in-memory / null defaults
    /// (`MemJournal`, `MemBlobStore`, `NoModel`, empty tools, `GateChain`,
    /// `NullCheckpointer`, `NullSandbox`, `EnvSecrets`, `SystemClock`, `UlidGen`).
    pub fn build(self) -> Runtime<D> {
        let options = self.options;
        let env = Env {
            journal: self.journal.unwrap_or_else(|| Arc::new(MemJournal::new())),
            blobs: self.blobs.unwrap_or_else(|| Arc::new(MemBlobStore::new())),
            model: self.model.unwrap_or_else(|| Arc::new(NoModel::default())),
            tools: Arc::new(self.tools.unwrap_or_else(|| ToolRegistry::new(options.workspace.clone()))),
            gates: self.gates.unwrap_or_else(|| Arc::new(GateChain::default())),
            checkpointer: self.checkpointer.unwrap_or_else(|| Arc::new(NullCheckpointer::default())),
            sandbox: self.sandbox.unwrap_or_else(|| Arc::new(NullSandbox)),
            secrets: self.secrets.unwrap_or_else(|| Arc::new(EnvSecrets::new())),
            memory: self.memory,
            subagents: self.subagents,
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            ids: self.ids.unwrap_or_else(|| Arc::new(UlidGen::new())),
            observers: self.observers,
            options,
        };
        Runtime { env: Arc::new(env), sessions: Arc::default(), _d: PhantomData }
    }
}

impl<D: Decider + 'static> Runtime<D>
where
    D::State: Send + Sync + 'static,
{
    pub fn builder() -> RuntimeBuilder<D> {
        RuntimeBuilder::default()
    }

    pub fn env(&self) -> &Arc<Env> {
        &self.env
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.env.tools
    }

    /// Live session handle, if this runtime drives it.
    pub fn session(&self, id: &SessionId) -> Option<SessionHandle<D>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Start a new session: acquire the lease, append `initial`'s events (for
    /// the real kernel: `agent_kernel::state::start_session(..)`), dispatch its
    /// effects.
    pub async fn create_session(&self, id: SessionId, initial: Decision) -> Result<SessionHandle<D>, DriverError> {
        let lease = self.env.journal.acquire_lease(&id).await?;
        if self.env.journal.next_seq(&id).await? != 0 {
            return Err(DriverError::AlreadyExists(id));
        }
        let (mut actor, handle, rx) = Actor::<D>::new(self.env.clone(), id.clone(), lease, D::State::default(), vec![]);
        let at = self.env.clock.now();
        let (_, effects) = actor.commit_async(initial, at).await?;
        for (eid, eff) in effects {
            actor.dispatch(eid, eff, false);
        }
        self.start(actor, handle.clone(), rx, 0);
        Ok(handle)
    }

    /// Crash recovery: take the lease, fold the journal, reconcile outstanding
    /// effects (see [`dispatch::recover_batch`] for tool calls).
    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle<D>, DriverError> {
        let lease = self.env.journal.acquire_lease(&id).await?;
        let events = self.env.journal.load(&id, 0).await?;
        if events.is_empty() {
            return Err(DriverError::NotFound(id));
        }
        let mut state = D::State::default();
        for e in &events {
            D::evolve(&mut state, e);
        }
        let outstanding = D::outstanding(&state);
        let keep = self.env.options.recent_capacity.max(1);
        let recent: Vec<_> = events[events.len().saturating_sub(keep)..].to_vec();
        let (mut actor, handle, rx) = Actor::<D>::new(self.env.clone(), id.clone(), lease, state, recent);
        let next = actor.next_seq;
        for (eid, eff) in outstanding {
            actor.dispatch(eid, eff, true);
        }
        let observe_from = if self.env.options.observer_replay_on_resume { 0 } else { next };
        self.start(actor, handle.clone(), rx, observe_from);
        Ok(handle)
    }

    /// Create when the journal has no events for `id`, otherwise resume.
    pub async fn open_session(
        &self,
        id: SessionId,
        initial: impl FnOnce() -> Decision,
    ) -> Result<SessionHandle<D>, DriverError> {
        if let Some(h) = self.session(&id) {
            return Ok(h);
        }
        if self.env.journal.next_seq(&id).await? == 0 {
            self.create_session(id, initial()).await
        } else {
            self.resume_session(id).await
        }
    }

    /// Stop driving a session (in-flight effects are cancelled).
    pub async fn close_session(&self, id: &SessionId) {
        let h = self.sessions.lock().unwrap().remove(id);
        if let Some(h) = h {
            h.shutdown().await;
        }
    }

    fn start(&self, actor: Actor<D>, handle: SessionHandle<D>, rx: mpsc::UnboundedReceiver<Msg>, observe_from: Seq) {
        for obs in &self.env.observers {
            let mut events = handle.subscribe(observe_from);
            let obs = obs.clone();
            let sid = handle.id().clone();
            tokio::spawn(async move {
                while let Some(ev) = events.next().await {
                    if let Err(e) = obs.on_event(&sid, &ev).await {
                        tracing::warn!(observer = obs.name(), seq = ev.seq, error = %e, "observer failed");
                    }
                }
            });
        }
        self.sessions.lock().unwrap().insert(handle.id().clone(), handle);
        tokio::spawn(actor.run(rx));
    }
}

// ---------------------------------------------------------------- session handle

struct Shared<S> {
    id: SessionId,
    state: RwLock<S>,
    recent: RwLock<VecDeque<Envelope<Event>>>,
    recent_capacity: usize,
    head_rx: watch::Receiver<Seq>,
    finish_rx: watch::Receiver<(u64, Option<TurnOutcome>)>,
    pulses: broadcast::Sender<Pulse>,
    asks: Arc<AskBoard>,
    journal: Arc<dyn JournalStore>,
}

impl<S> Shared<S> {
    async fn fetch(&self, cursor: Seq) -> Result<Vec<Envelope<Event>>, StoreError> {
        {
            let recent = self.recent.read().unwrap();
            if let Some(front) = recent.front() {
                if front.seq <= cursor {
                    let skip = (cursor - front.seq) as usize;
                    return Ok(recent.iter().skip(skip).cloned().collect());
                }
            }
        }
        self.journal.load(&self.id, cursor).await
    }
}

/// Handle to a live session. Cheap to clone; every client holds one.
pub struct SessionHandle<D: Decider> {
    shared: Arc<Shared<D::State>>,
    tx: mpsc::UnboundedSender<Msg>,
}

impl<D: Decider> Clone for SessionHandle<D> {
    fn clone(&self) -> Self {
        SessionHandle { shared: self.shared.clone(), tx: self.tx.clone() }
    }
}

impl<D: Decider + 'static> SessionHandle<D>
where
    D::State: Send + Sync + 'static,
{
    pub fn id(&self) -> &SessionId {
        &self.shared.id
    }

    /// Feed an input to the kernel; resolves once it is decided, logged and
    /// its effects dispatched. `Control::Answer` and `Control::HardInterrupt`
    /// are handled by the runtime (see module docs).
    pub async fn send(&self, input: Input) -> Result<Applied, DriverError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Input(input, Some(tx))).map_err(|_| DriverError::Closed)?;
        rx.await.map_err(|_| DriverError::Closed)?
    }

    /// Fire and forget.
    pub fn post(&self, input: Input) -> Result<(), DriverError> {
        self.tx.send(Msg::Input(input, None)).map_err(|_| DriverError::Closed)
    }

    /// A client command with an idempotency key: a retried key returns the
    /// original result without re-applying.
    pub async fn command(&self, key: impl Into<String>, cmd: Command) -> Result<Applied, DriverError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Command(key.into(), cmd, tx)).map_err(|_| DriverError::Closed)?;
        rx.await.map_err(|_| DriverError::Closed)?
    }

    /// Answer a pending question (compare-and-swap: first answer wins).
    pub async fn answer(&self, question: QuestionId, answer: Answer, responder: &str) -> Result<Applied, DriverError> {
        self.send(Input::Control(Control::Answer { question, answer, responder: responder.to_string() })).await
    }

    /// Unanswered questions currently waiting on a human.
    pub fn pending_questions(&self) -> Vec<Question> {
        self.shared.asks.pending()
    }

    pub fn asks(&self) -> &Arc<AskBoard> {
        &self.shared.asks
    }

    /// Events with `seq >= from_seq`: replayed from memory / the journal, then
    /// live. Ordered, gap-free; a slow subscriber lags, never blocks the driver.
    /// Ends when the session actor stops.
    pub fn subscribe(&self, from_seq: Seq) -> BoxStream<'static, Envelope<Event>> {
        struct St<S> {
            shared: Arc<Shared<S>>,
            cursor: Seq,
            buf: VecDeque<Envelope<Event>>,
            head_rx: watch::Receiver<Seq>,
            closed: bool,
        }
        let st = St {
            shared: self.shared.clone(),
            cursor: from_seq,
            buf: VecDeque::new(),
            head_rx: self.shared.head_rx.clone(),
            closed: false,
        };
        stream::unfold(st, |mut st| async move {
            loop {
                if let Some(e) = st.buf.pop_front() {
                    return Some((e, st));
                }
                let head = *st.head_rx.borrow_and_update();
                if st.cursor < head {
                    match st.shared.fetch(st.cursor).await {
                        Ok(evs) => {
                            for e in evs {
                                if e.seq == st.cursor && e.seq < head {
                                    st.cursor += 1;
                                    st.buf.push_back(e);
                                }
                            }
                            if !st.buf.is_empty() {
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "subscription fetch failed");
                            return None;
                        }
                    }
                }
                if st.closed {
                    return None;
                }
                if st.head_rx.changed().await.is_err() {
                    st.closed = true;
                }
            }
        })
        .boxed()
    }

    /// Transient pulses (token deltas, tool progress). Lossy.
    pub fn pulses(&self) -> broadcast::Receiver<Pulse> {
        self.shared.pulses.subscribe()
    }

    /// Next seq to be written.
    pub fn next_seq(&self) -> Seq {
        *self.shared.head_rx.borrow()
    }

    /// Number of `Finish` effects seen so far.
    pub fn finish_count(&self) -> u64 {
        self.shared.finish_rx.borrow().0
    }

    /// The outcome of the most recent `Finish`.
    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        self.shared.finish_rx.borrow().1.clone()
    }

    /// Wait for the next `Finish` after this call.
    pub async fn wait_finish(&self) -> Result<TurnOutcome, DriverError> {
        let n = self.finish_count();
        self.wait_finish_after(n).await
    }

    /// Wait for a `Finish` numbered above `count` (race-free with `send`:
    /// read `finish_count()` first).
    pub async fn wait_finish_after(&self, count: u64) -> Result<TurnOutcome, DriverError> {
        let mut rx = self.shared.finish_rx.clone();
        let v = rx.wait_for(|f| f.0 > count).await.map_err(|_| DriverError::Closed)?;
        v.1.clone().ok_or(DriverError::Closed)
    }

    /// Send an input and wait for the turn's outcome.
    pub async fn run(&self, input: Input) -> Result<TurnOutcome, DriverError> {
        let n = self.finish_count();
        self.send(input).await?;
        self.wait_finish_after(n).await
    }

    /// Read the kernel state.
    pub fn with_state<R>(&self, f: impl FnOnce(&D::State) -> R) -> R {
        f(&self.shared.state.read().unwrap())
    }

    /// Clone of the kernel state.
    pub fn state(&self) -> D::State
    where
        D::State: Clone,
    {
        self.shared.state.read().unwrap().clone()
    }

    /// Stop the actor (cancels in-flight effects). Idempotent.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

// ---------------------------------------------------------------- actor

struct InFlight {
    cancel: CancellationToken,
    /// Survives hard interrupts (restores run to completion).
    uncancellable: bool,
    sample: Option<Arc<Mutex<Assembler>>>,
    question: Option<QuestionId>,
}

struct TxSink(mpsc::UnboundedSender<Msg>);

impl EffectSink for TxSink {
    fn streamed(&self, id: EffectId, call: ToolCall) {
        let _ = self.0.send(Msg::Streamed(id, call));
    }
}

struct Actor<D: Decider> {
    env: Arc<Env>,
    shared: Arc<Shared<D::State>>,
    lease: LeaseGen,
    next_seq: Seq,
    head: Option<EventId>,
    head_tx: watch::Sender<Seq>,
    finish_tx: watch::Sender<(u64, Option<TurnOutcome>)>,
    inflight: BTreeMap<EffectId, InFlight>,
    dedupe: VecDeque<(String, Result<Applied, DriverError>)>,
    tx: mpsc::WeakUnboundedSender<Msg>,
    fenced: Option<String>,
}

impl<D: Decider + 'static> Actor<D>
where
    D::State: Send + Sync + 'static,
{
    fn new(
        env: Arc<Env>,
        id: SessionId,
        lease: LeaseGen,
        state: D::State,
        recent: Vec<Envelope<Event>>,
    ) -> (Self, SessionHandle<D>, mpsc::UnboundedReceiver<Msg>) {
        let next_seq = recent.last().map(|e| e.seq + 1).unwrap_or(0);
        let head = recent.last().map(|e| e.id.clone());
        let (head_tx, head_rx) = watch::channel(next_seq);
        let (finish_tx, finish_rx) = watch::channel((0, None));
        let (pulses, _) = broadcast::channel(env.options.pulse_capacity.max(1));
        let shared = Arc::new(Shared {
            id,
            state: RwLock::new(state),
            recent: RwLock::new(recent.into()),
            recent_capacity: env.options.recent_capacity,
            head_rx,
            finish_rx,
            pulses,
            asks: Arc::new(AskBoard::new()),
            journal: env.journal.clone(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor {
            env,
            shared: shared.clone(),
            lease,
            next_seq,
            head,
            head_tx,
            finish_tx,
            inflight: BTreeMap::new(),
            dedupe: VecDeque::new(),
            tx: tx.downgrade(),
            fenced: None,
        };
        (actor, SessionHandle { shared, tx }, rx)
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Input(input, reply) => {
                    let r = self.handle(input).await;
                    if let Err(e) = &r {
                        tracing::debug!(session = %self.shared.id, error = %e, "input not applied");
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(r);
                    }
                }
                Msg::Command(key, cmd, reply) => {
                    if let Some((_, r)) = self.dedupe.iter().find(|(k, _)| *k == key) {
                        let _ = reply.send(r.clone());
                        continue;
                    }
                    let input = match cmd {
                        Command::Signal(s) => Input::Signal(s),
                        Command::Control(c) => Input::Control(c),
                    };
                    let r = self.handle(input).await;
                    self.dedupe.push_back((key, r.clone()));
                    while self.dedupe.len() > self.env.options.dedupe_capacity.max(1) {
                        self.dedupe.pop_front();
                    }
                    let _ = reply.send(r);
                }
                Msg::Streamed(id, call) => {
                    if self.inflight.contains_key(&id) {
                        if let Err(e) = self.apply(Input::Streamed(id, call)).await {
                            tracing::warn!(effect = %id, error = %e, "streamed tool call not applied");
                        }
                    }
                }
                Msg::Completed(id, result) => {
                    if self.inflight.remove(&id).is_none() {
                        tracing::debug!(effect = %id, "dropping result of cancelled effect");
                        continue;
                    }
                    if let Err(e) = self.apply(Input::Completed(id, result)).await {
                        tracing::warn!(effect = %id, error = %e, "effect completion not applied");
                    }
                }
                Msg::Shutdown(done) => {
                    self.cancel_all();
                    let _ = done.send(());
                    break;
                }
            }
        }
        self.cancel_all();
    }

    fn cancel_all(&mut self) {
        for (_, f) in std::mem::take(&mut self.inflight) {
            f.cancel.cancel();
            if let Some(q) = f.question {
                self.shared.asks.close(&q);
            }
        }
    }

    async fn handle(&mut self, input: Input) -> Result<Applied, DriverError> {
        match input {
            Input::Control(Control::Answer { question, answer, responder }) => {
                // Resolved here (CAS); the gate completion carries it to the kernel.
                self.shared.asks.answer(&question, answer, human_responder(&responder))?;
                Ok(Applied { events: 0, next_seq: self.next_seq, effects: 0 })
            }
            Input::Control(Control::HardInterrupt) => self.hard_interrupt().await,
            other => self.apply(other).await,
        }
    }

    /// Hard-interrupt contract: cancel every in-flight effect; for an in-flight
    /// sample FIRST submit `Completed(Sampled(partial, stop=Interrupted))`, THEN
    /// the `HardInterrupt` control. Effects produced by the partial completion
    /// are superseded by the interrupt and are not dispatched.
    async fn hard_interrupt(&mut self) -> Result<Applied, DriverError> {
        let mut partials = vec![];
        let (keep, cancel): (BTreeMap<_, _>, BTreeMap<_, _>) =
            std::mem::take(&mut self.inflight).into_iter().partition(|(_, f)| f.uncancellable);
        self.inflight = keep;
        for (id, f) in cancel {
            f.cancel.cancel();
            if let Some(q) = f.question {
                self.shared.asks.close(&q);
            }
            if let Some(asm) = f.sample {
                partials.push((id, asm.lock().unwrap().partial()));
            }
        }
        for (id, msg) in partials {
            match self.decide_commit(Input::Completed(id, EffectResult::Sampled(msg))).await {
                Ok((_, effects)) if !effects.is_empty() => {
                    tracing::debug!(n = effects.len(), "not dispatching effects superseded by hard interrupt");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(effect = %id, error = %e, "partial reply not applied"),
            }
        }
        self.apply(Input::Control(Control::HardInterrupt)).await
    }

    async fn apply(&mut self, input: Input) -> Result<Applied, DriverError> {
        let (mut applied, effects) = self.decide_commit(input).await?;
        applied.effects = effects.len();
        for (id, eff) in effects {
            self.dispatch(id, eff, false);
        }
        Ok(applied)
    }

    async fn decide_commit(&mut self, input: Input) -> Result<(Applied, Vec<(EffectId, Effect)>), DriverError> {
        if let Some(f) = &self.fenced {
            return Err(DriverError::Fenced(f.clone()));
        }
        let at = self.env.clock.now();
        let decision = {
            let s = self.shared.state.read().unwrap();
            D::decide(&s, at, input).map_err(|r| DriverError::Rejected(r.reason))?
        };
        self.commit_async(decision, at).await
    }

    async fn commit_async(
        &mut self,
        decision: Decision,
        at: Timestamp,
    ) -> Result<(Applied, Vec<(EffectId, Effect)>), DriverError> {
        let Decision { events, effects } = decision;
        if events.is_empty() {
            return Ok((Applied { events: 0, next_seq: self.next_seq, effects: 0 }, effects));
        }
        let mut head = self.head.clone();
        let mut envs = Vec::with_capacity(events.len());
        for (i, d) in events.into_iter().enumerate() {
            let id = self.env.ids.event_id(at);
            let parent = match d.parent {
                Parent::Head => head.clone(),
                Parent::Explicit(p) => Some(p),
            };
            head = Some(id.clone());
            envs.push(Envelope {
                id,
                parent,
                seq: self.next_seq + i as u64,
                at,
                origin: d.origin,
                trust: d.trust,
                audience: d.audience,
                schema: EVENT_SCHEMA,
                body: d.body,
                rendered: d.rendered,
            });
        }
        // Log first.
        if let Err(e) = self.env.journal.append(&self.shared.id, self.lease, self.next_seq, &envs).await {
            let err = DriverError::from(e);
            if let DriverError::Fenced(msg) = &err {
                tracing::error!(session = %self.shared.id, error = %msg, "session fenced; stopping effects");
                self.fenced = Some(msg.clone());
                self.cancel_all();
            }
            return Err(err);
        }
        self.next_seq += envs.len() as u64;
        self.head = head;
        {
            let mut s = self.shared.state.write().unwrap();
            for e in &envs {
                D::evolve(&mut s, e);
            }
        }
        let n = envs.len();
        {
            let mut recent = self.shared.recent.write().unwrap();
            recent.extend(envs);
            while recent.len() > self.shared.recent_capacity.max(1) {
                recent.pop_front();
            }
        }
        self.head_tx.send_replace(self.next_seq);
        Ok((Applied { events: n, next_seq: self.next_seq, effects: 0 }, effects))
    }

    fn dispatch(&mut self, id: EffectId, effect: Effect, recovering: bool) {
        if let Effect::Finish(outcome) = effect {
            self.finish_tx.send_modify(|f| {
                f.0 += 1;
                f.1 = Some(outcome);
            });
            return;
        }
        let Some(tx) = self.tx.upgrade() else {
            tracing::warn!(effect = %id, "session closing; effect not dispatched");
            return;
        };
        let cancel = CancellationToken::new();
        let env = self.env.clone();
        let session = self.shared.id.clone();
        let pulses = self.shared.pulses.clone();
        let mut flight = InFlight { cancel: cancel.clone(), uncancellable: false, sample: None, question: None };
        tracing::debug!(effect = %id, kind = effect.kind(), recovering, "dispatch");
        match effect {
            Effect::Sample(prompt) => {
                let asm = Arc::new(Mutex::new(Assembler::new()));
                flight.sample = Some(asm.clone());
                tokio::spawn(async move {
                    let sink = TxSink(tx.clone());
                    let r = dispatch::sample(&env, id, &prompt, asm, &cancel, &pulses, Some(&sink)).await;
                    if let Some(r) = r {
                        let res = match r {
                            Ok(m) => EffectResult::Sampled(m),
                            Err(e) => EffectResult::SampleFailed(e),
                        };
                        let _ = tx.send(Msg::Completed(id, res));
                    }
                });
            }
            Effect::Compact(job) => {
                tokio::spawn(async move {
                    if let Some(r) = dispatch::compact(&env, id, &job, &cancel).await {
                        let _ = tx.send(Msg::Completed(id, r));
                    }
                });
            }
            Effect::Execute(batch) => {
                tokio::spawn(async move {
                    let r = if recovering {
                        dispatch::recover_batch(&env, &session, &batch, &cancel, &pulses).await
                    } else {
                        dispatch::execute(&env, &session, &batch, &cancel, &pulses).await
                    };
                    if let Some(r) = r {
                        let _ = tx.send(Msg::Completed(id, r));
                    }
                });
            }
            Effect::Gate(req) => {
                let asks = self.shared.asks.clone();
                if let Some(q) = &req.question {
                    // Open synchronously so an answer arriving right after the
                    // `QuestionAsked` event is never "unknown".
                    asks.open(q);
                    flight.question = Some(q.id.clone());
                }
                let ctx = GateCtx { session, asks, cancel };
                tokio::spawn(async move {
                    if let Some(r) = dispatch::gate(&env, &req, ctx).await {
                        let _ = tx.send(Msg::Completed(id, r));
                    }
                });
            }
            Effect::Checkpoint(scope) => {
                tokio::spawn(async move {
                    let r = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        r = dispatch::checkpoint(&env, &scope) => r,
                    };
                    let _ = tx.send(Msg::Completed(id, r));
                });
            }
            Effect::Restore(plan) => {
                // Not cancellable mid-way: a restore runs to completion so the
                // workspace matches the journaled plan (idempotent on re-run).
                flight.uncancellable = true;
                tokio::spawn(async move {
                    let r = dispatch::restore(&env, &plan).await;
                    let _ = tx.send(Msg::Completed(id, r));
                });
            }
            Effect::Finish(_) => unreachable!(),
        }
        self.inflight.insert(id, flight);
    }
}
