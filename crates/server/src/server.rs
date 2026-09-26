//! The session service: one [`Server`] per [`Runtime`], any number of
//! connections, any number of connections per session.
//!
//! Each connection runs three independent parts:
//!
//! - a **reader** that decodes client messages and hands them to the processor;
//! - a **processor** that handles one request at a time and spawns one event
//!   pump per subscription;
//! - a **writer** task that drains the bounded outgoing queue into the
//!   transport's write half.
//!
//! A client that stops reading therefore never stops the server from reading
//! its requests. Backpressure is explicit instead: pulses are dropped once the
//! outgoing queue is backed up (lossy), and a reliable message (event, ack,
//! error) that cannot enter the queue within [`ServerOptions::stall_timeout`]
//! disconnects the client with an `Error` telling it which `from_seq` to
//! resubscribe from.

use crate::transport::{channel, ChannelClient, JsonLines, Transport, TransportError, TransportRead, TransportWrite, WsTransport};
use agent_kernel::{Decider, Decision};
use agent_proto::*;
use agent_runtime::{AnswerError, DriverError, Runtime, SessionHandle};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

/// Protocol versions this server speaks.
pub const SUPPORTED_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// Default outgoing buffer per connection (see [`ServerOptions`]).
pub const OUTGOING_CAPACITY: usize = 1024;

/// Default time a reliable message may wait for room in the outgoing queue
/// before the connection is dropped as a slow consumer.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Answer idempotency keys remembered per session.
const ANSWER_DEDUPE: usize = 1024;

/// Closed question ids remembered per connection.
const CLOSED_DEDUPE: usize = 4096;

/// Requests decoded ahead of the processor.
const REQUEST_BUFFER: usize = 64;

/// Ack error text for the losers of an approval race.
pub const ALREADY_ANSWERED: &str = "already answered";

/// Prefix of the `Error` sent to a client disconnected for not reading.
pub const SLOW_CONSUMER: &str = "slow consumer";

/// Highest version both sides support.
pub fn negotiate(client: &[u32], server: &[u32]) -> Option<u32> {
    client.iter().copied().filter(|v| server.contains(v)).max()
}

/// Per-connection flow control.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Outgoing queue per connection (events, acks, pulses).
    pub outgoing_capacity: usize,
    /// Pulses are dropped while fewer than this many queue slots are free, so
    /// a backed-up connection keeps room for reliable messages.
    pub pulse_headroom: usize,
    /// How long a reliable message may wait for queue room before the client
    /// is disconnected as a slow consumer.
    pub stall_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            outgoing_capacity: OUTGOING_CAPACITY,
            pulse_headroom: OUTGOING_CAPACITY / 4,
            stall_timeout: STALL_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------- opener

/// How the server obtains a session a client names that the runtime is not
/// currently driving.
#[async_trait]
pub trait SessionOpener<D: Decider>: Send + Sync {
    async fn open(&self, rt: &Runtime<D>, id: &SessionId) -> Result<SessionHandle<D>, DriverError>;
}

/// Only resume sessions that already exist in the journal; unknown ids fail.
pub struct ResumeOnly;

#[async_trait]
impl<D> SessionOpener<D> for ResumeOnly
where
    D: Decider + 'static,
    D::State: Send + Sync + 'static,
{
    async fn open(&self, rt: &Runtime<D>, id: &SessionId) -> Result<SessionHandle<D>, DriverError> {
        rt.resume_session(id.clone()).await
    }
}

/// Resume if the journal has the session, otherwise create it with the
/// decision built by the closure (e.g. `agent_kernel::state::start_session`).
pub struct OpenWith<F>(pub F);

#[async_trait]
impl<D, F> SessionOpener<D> for OpenWith<F>
where
    D: Decider + 'static,
    D::State: Send + Sync + 'static,
    F: Fn(&SessionId) -> Decision + Send + Sync,
{
    async fn open(&self, rt: &Runtime<D>, id: &SessionId) -> Result<SessionHandle<D>, DriverError> {
        rt.open_session(id.clone(), || (self.0)(id)).await
    }
}

// ---------------------------------------------------------------- outgoing

/// The connection is gone (or being dropped); stop producing for it.
struct Gone;

/// Producer side of a connection's outgoing queue.
#[derive(Clone)]
struct Outgoing {
    tx: mpsc::Sender<ServerMessage>,
    kill: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    stall: Duration,
    headroom: usize,
}

impl Outgoing {
    /// Queue a reliable message; waits up to the stall timeout for room, then
    /// disconnects the client with `overflow()` as the reason.
    async fn reliable(&self, msg: ServerMessage, overflow: impl FnOnce() -> String) -> Result<(), Gone> {
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Closed(_)) => Err(Gone),
            Err(TrySendError::Full(msg)) => match tokio::time::timeout(self.stall, self.tx.send(msg)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err(Gone),
                Err(_) => {
                    self.disconnect(overflow());
                    Err(Gone)
                }
            },
        }
    }

    /// Queue a lossy message: dropped when the queue is backed up.
    fn lossy(&self, msg: ServerMessage) {
        if self.tx.capacity() > self.headroom {
            let _ = self.tx.try_send(msg);
        }
    }

    fn disconnect(&self, reason: String) {
        if let Some(k) = self.kill.lock().unwrap().take() {
            let _ = k.send(reason);
        }
    }
}

fn slow_consumer(detail: impl std::fmt::Display) -> String {
    format!("{SLOW_CONSUMER}: outgoing queue overflowed; {detail}")
}

/// Drain the outgoing queue into the transport. Ends when every producer is
/// gone (`Ok`), the transport fails, or the connection is killed.
async fn write_loop<W: TransportWrite>(
    mut w: W,
    mut rx: mpsc::Receiver<ServerMessage>,
    mut kill: oneshot::Receiver<String>,
    stall: Duration,
) -> Result<(), ConnError> {
    // The kill sender outlives this loop (the connection keeps it), so the
    // receiver only resolves on an actual kill.
    let result = loop {
        tokio::select! {
            biased;
            reason = &mut kill => {
                let Ok(reason) = reason else { break Ok(()) };
                // Best effort: the peer may not be reading at all.
                let grace = stall.min(Duration::from_secs(1));
                let msg = ServerMessage::Error { message: reason.clone() };
                let _ = tokio::time::timeout(grace, w.send(msg)).await;
                break Err(ConnError::SlowConsumer(reason));
            }
            m = rx.recv() => {
                let Some(m) = m else { break Ok(()) };
                // A blocked write must still notice a kill.
                let sent = tokio::select! {
                    biased;
                    reason = &mut kill => Err(reason),
                    r = w.send(m) => Ok(r),
                };
                match sent {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => break Err(e.into()),
                    Err(Ok(reason)) => break Err(ConnError::SlowConsumer(reason)),
                    Err(Err(_)) => break Ok(()),
                }
            }
        }
    };
    let _ = tokio::time::timeout(stall.min(Duration::from_secs(1)), w.close()).await;
    result
}

// ---------------------------------------------------------------- server

type Ack = (bool, Option<String>);

/// Question ids already closed for one connection (bounded).
#[derive(Default)]
struct ClosedSet {
    set: HashSet<QuestionId>,
    order: VecDeque<QuestionId>,
}

impl ClosedSet {
    /// `true` when newly closed.
    fn insert(&mut self, q: &QuestionId) -> bool {
        if !self.set.insert(q.clone()) {
            return false;
        }
        self.order.push_back(q.clone());
        while self.order.len() > CLOSED_DEDUPE {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

type Closed = Arc<Mutex<ClosedSet>>;

#[derive(Clone)]
struct Subscriber {
    out: Outgoing,
    closed: Closed,
}

/// Per-session fan-out state (outlives session handles: a closed and reopened
/// session keeps its subscribers' registrations).
#[derive(Default)]
struct Hub {
    /// Connections subscribed to this session.
    subscribers: Mutex<HashMap<u64, Subscriber>>,
    /// Answer idempotency; the lock also serializes answers per session.
    answers: tokio::sync::Mutex<VecDeque<(String, Ack)>>,
}

struct Inner<D: Decider> {
    rt: Arc<Runtime<D>>,
    opener: Box<dyn SessionOpener<D>>,
    options: ServerOptions,
    hubs: Mutex<HashMap<SessionId, Arc<Hub>>>,
    open_lock: tokio::sync::Mutex<()>,
    next_conn: AtomicU64,
}

/// The session service. Cheap to clone.
pub struct Server<D: Decider> {
    inner: Arc<Inner<D>>,
}

impl<D: Decider> Clone for Server<D> {
    fn clone(&self) -> Self {
        Server { inner: self.inner.clone() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("no common protocol version (client {client:?}, server {server:?})")]
    NoCommonVersion { client: Vec<u32>, server: Vec<u32> },
    /// The client did not read fast enough and was disconnected.
    #[error("{0}")]
    SlowConsumer(String),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

fn joined(r: Result<Result<(), ConnError>, tokio::task::JoinError>, what: &str) -> Result<(), ConnError> {
    match r {
        Ok(r) => r,
        Err(e) => {
            if e.is_panic() {
                tracing::error!(error = %e, "connection {what} panicked");
            }
            Ok(())
        }
    }
}

impl<D> Server<D>
where
    D: Decider + 'static,
    D::State: Send + Sync + 'static,
{
    pub fn new(rt: Arc<Runtime<D>>, opener: impl SessionOpener<D> + 'static) -> Self {
        Self::with_options(rt, opener, ServerOptions::default())
    }

    pub fn with_options(rt: Arc<Runtime<D>>, opener: impl SessionOpener<D> + 'static, options: ServerOptions) -> Self {
        Server {
            inner: Arc::new(Inner {
                rt,
                opener: Box::new(opener),
                options,
                hubs: Mutex::new(HashMap::new()),
                open_lock: tokio::sync::Mutex::new(()),
                next_conn: AtomicU64::new(1),
            }),
        }
    }

    pub fn runtime(&self) -> &Arc<Runtime<D>> {
        &self.inner.rt
    }

    pub fn options(&self) -> &ServerOptions {
        &self.inner.options
    }

    /// The live handle for `id`, opening it through the [`SessionOpener`].
    pub async fn session(&self, id: &SessionId) -> Result<SessionHandle<D>, DriverError> {
        if let Some(h) = self.inner.rt.session(id) {
            return Ok(h);
        }
        let _g = self.inner.open_lock.lock().await;
        if let Some(h) = self.inner.rt.session(id) {
            return Ok(h);
        }
        self.inner.opener.open(&self.inner.rt, id).await
    }

    fn hub(&self, id: &SessionId) -> Arc<Hub> {
        self.inner.hubs.lock().unwrap().entry(id.clone()).or_default().clone()
    }

    /// Serve one connection until the peer disconnects, version negotiation
    /// fails, or the peer is dropped as a slow consumer.
    pub async fn serve<T: Transport>(&self, transport: T) -> Result<(), ConnError> {
        let (mut reader, writer) = transport.split();
        let opts = &self.inner.options;
        let conn_id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<ServerMessage>(opts.outgoing_capacity.max(1));
        let (kill_tx, kill_rx) = oneshot::channel();
        let out = Outgoing {
            tx,
            kill: Arc::new(Mutex::new(Some(kill_tx))),
            stall: opts.stall_timeout,
            headroom: opts.pulse_headroom.min(opts.outgoing_capacity.saturating_sub(1)),
        };
        // Held until the connection ends so the writer's kill receiver never
        // sees a dropped sender.
        let _kill_keep = out.kill.clone();
        let mut writer = tokio::spawn(write_loop(writer, rx, kill_rx, opts.stall_timeout));

        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(REQUEST_BUFFER);
        let conn = Conn {
            server: self.clone(),
            id: conn_id,
            out: out.clone(),
            closed: Closed::default(),
            client: None,
            subs: HashMap::new(),
        };
        let mut processor = tokio::spawn(conn.run(req_rx));

        // Reader: independent of the writer; only waits for the processor,
        // which itself never waits longer than the stall timeout.
        let mut reading = Box::pin({
            let out = out.clone();
            async move {
                loop {
                    match reader.recv().await {
                        Ok(Some(m)) => {
                            if req_tx.send(m).await.is_err() {
                                return Ok(());
                            }
                        }
                        // Dropping `req_tx` lets the processor finish.
                        Ok(None) => return Ok(()),
                        Err(TransportError::Decode(e)) => {
                            let msg = ServerMessage::Error { message: format!("bad message: {e}") };
                            if out.reliable(msg, || slow_consumer("reconnect")).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(e) => return Err(ConnError::from(e)),
                    }
                }
            }
        });
        drop(out);

        let mut reading_done = false;
        let result = loop {
            tokio::select! {
                biased;
                w = &mut writer => {
                    // Transport failure or slow consumer: tear everything down.
                    processor.abort();
                    return joined(w, "writer");
                }
                p = &mut processor => break joined(p, "processor"),
                r = &mut reading, if !reading_done => {
                    reading_done = true;
                    if let Err(e) = r {
                        processor.abort();
                        writer.abort();
                        return Err(e);
                    }
                }
            }
        };
        // The processor finished (EOF or version failure): flush what it
        // queued, then close the transport.
        drop(reading);
        match tokio::time::timeout(opts.stall_timeout, &mut writer).await {
            Ok(w) => {
                let w = joined(w, "writer");
                result.and(w)
            }
            Err(_) => {
                writer.abort();
                result
            }
        }
    }

    /// Spawn a connection over an in-process channel and return its client end.
    pub fn connect(&self) -> ChannelClient {
        let (t, c) = channel();
        let server = self.clone();
        tokio::spawn(async move {
            if let Err(e) = server.serve(t).await {
                tracing::debug!(error = %e, "in-process connection ended");
            }
        });
        c
    }

    /// Serve one connection on stdin / stdout (JSON Lines).
    pub async fn serve_stdio(&self) -> Result<(), ConnError> {
        self.serve(JsonLines::stdio()).await
    }

    /// Accept WebSocket connections on `addr` forever.
    pub async fn serve_ws(&self, addr: impl ToSocketAddrs) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.serve_ws_listener(listener).await
    }

    /// Accept WebSocket connections on an already bound listener forever.
    pub async fn serve_ws_listener(&self, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                match tokio_tungstenite::accept_async(stream).await {
                    Ok(ws) => {
                        if let Err(e) = server.serve(WsTransport::new(ws)).await {
                            tracing::debug!(%peer, error = %e, "websocket connection ended");
                        }
                    }
                    Err(e) => tracing::debug!(%peer, error = %e, "websocket handshake failed"),
                }
            });
        }
    }
}

// ---------------------------------------------------------------- connection

/// Aborts the task when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Conn<D: Decider> {
    server: Server<D>,
    id: u64,
    out: Outgoing,
    /// Questions already closed on this connection (never close twice).
    closed: Closed,
    /// Client name, set by `Hello`.
    client: Option<String>,
    subs: HashMap<SessionId, Task>,
}

impl<D: Decider> Drop for Conn<D> {
    fn drop(&mut self) {
        let hubs = self.server.inner.hubs.lock().unwrap();
        for sid in self.subs.keys() {
            if let Some(h) = hubs.get(sid) {
                h.subscribers.lock().unwrap().remove(&self.id);
            }
        }
    }
}

impl<D> Conn<D>
where
    D: Decider + 'static,
    D::State: Send + Sync + 'static,
{
    async fn run(mut self, mut rx: mpsc::Receiver<ClientMessage>) -> Result<(), ConnError> {
        while let Some(msg) = rx.recv().await {
            self.handle(msg).await?;
        }
        Ok(())
    }

    async fn reply(&self, msg: ServerMessage) {
        let _ = self.out.reliable(msg, || slow_consumer("reconnect and resubscribe")).await;
    }

    async fn error(&self, message: impl Into<String>) {
        self.reply(ServerMessage::Error { message: message.into() }).await;
    }

    async fn handle(&mut self, msg: ClientMessage) -> Result<(), ConnError> {
        match msg {
            ClientMessage::Ping => self.reply(ServerMessage::Pong).await,
            ClientMessage::Hello { versions, client } => {
                if self.client.is_some() {
                    self.error("already negotiated").await;
                    return Ok(());
                }
                match negotiate(&versions, SUPPORTED_VERSIONS) {
                    Some(version) => {
                        self.client = Some(client);
                        self.reply(ServerMessage::Welcome { version }).await;
                    }
                    None => {
                        let err = ConnError::NoCommonVersion { client: versions, server: SUPPORTED_VERSIONS.to_vec() };
                        self.error(err.to_string()).await;
                        return Err(err);
                    }
                }
            }
            _ if self.client.is_none() => self.error("expected hello").await,
            ClientMessage::Subscribe { session, from_seq, pulses } => self.subscribe(session, from_seq, pulses).await,
            ClientMessage::Command { session, key, command } => match command {
                Command::Control(Control::Answer { question, answer, responder }) => {
                    let responder = if responder.is_empty() { self.client.clone().unwrap_or_default() } else { responder };
                    self.answer(session, key, question, answer, responder).await
                }
                command => {
                    let ack = match self.server.session(&session).await {
                        Ok(h) => to_ack(h.command(key.clone(), command).await),
                        Err(e) => (false, Some(e.to_string())),
                    };
                    self.reply(ServerMessage::Ack { key, accepted: ack.0, error: ack.1 }).await;
                }
            },
            ClientMessage::Answer { session, key, question, answer } => {
                let responder = self.client.clone().unwrap_or_default();
                self.answer(session, key, question, answer, responder).await
            }
        }
        Ok(())
    }

    async fn subscribe(&mut self, session: SessionId, from_seq: Seq, pulses: bool) {
        let handle = match self.server.session(&session).await {
            Ok(h) => h,
            Err(e) => return self.error(format!("subscribe {session}: {e}")).await,
        };
        // Resubscribing replaces the previous subscription.
        self.subs.remove(&session);
        let hub = self.server.hub(&session);
        hub.subscribers
            .lock()
            .unwrap()
            .insert(self.id, Subscriber { out: self.out.clone(), closed: self.closed.clone() });
        let events = handle.subscribe(from_seq);
        let pulses = pulses.then(|| handle.pulses());
        let task = tokio::spawn(pump(session.clone(), events, pulses, self.out.clone(), self.closed.clone()));
        self.subs.insert(session, Task(task));
    }

    async fn answer(&mut self, session: SessionId, key: String, question: QuestionId, answer: Answer, responder: String) {
        let hub = self.server.hub(&session);
        let ack = {
            let mut seen = hub.answers.lock().await;
            if let Some((_, ack)) = seen.iter().find(|(k, _)| *k == key) {
                ack.clone()
            } else {
                let ack = match self.server.session(&session).await {
                    Ok(h) => match h.answer(question.clone(), answer, &responder).await {
                        Err(DriverError::Answer(AnswerError::AlreadyAnswered(_))) => {
                            (false, Some(ALREADY_ANSWERED.to_string()))
                        }
                        r => to_ack(r),
                    },
                    Err(e) => (false, Some(e.to_string())),
                };
                seen.push_back((key.clone(), ack.clone()));
                while seen.len() > ANSWER_DEDUPE {
                    seen.pop_front();
                }
                if ack.0 {
                    // Close the dialog everywhere now (the journaled
                    // `QuestionAnswered` follows later and is deduplicated);
                    // the winner already knows.
                    self.closed.lock().unwrap().insert(&question);
                    let others: Vec<Subscriber> = hub
                        .subscribers
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(id, _)| **id != self.id)
                        .map(|(_, s)| s.clone())
                        .collect();
                    for s in others {
                        close_question(&s, &session, &question);
                    }
                }
                ack
            }
        };
        self.reply(ServerMessage::Ack { key, accepted: ack.0, error: ack.1 }).await;
    }
}

/// Send `QuestionClosed` to another connection unless it already got it;
/// never stalls the caller.
fn close_question(s: &Subscriber, session: &SessionId, question: &QuestionId) {
    if !s.closed.lock().unwrap().insert(question) {
        return;
    }
    let msg = ServerMessage::QuestionClosed { session: session.clone(), question: question.clone() };
    if let Err(TrySendError::Full(msg)) = s.out.tx.try_send(msg) {
        let out = s.out.clone();
        tokio::spawn(async move {
            let _ = out.reliable(msg, || slow_consumer("reconnect and resubscribe")).await;
        });
    }
}

fn to_ack<T>(r: Result<T, DriverError>) -> Ack {
    match r {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    }
}

/// Which questions an event closes, tracking open ones per gate subject.
///
/// Closes on `QuestionAnswered` (any responder: a client, in-process code, an
/// auto rule), on a final verdict recorded for the subject a question was
/// asked about, and on a hard interrupt (which drops every question).
#[derive(Default)]
struct QuestionTracker {
    open: BTreeMap<GateRef, Vec<QuestionId>>,
}

impl QuestionTracker {
    fn closes(&mut self, ev: &Event) -> Vec<QuestionId> {
        match ev {
            Event::QuestionAsked { question, subject } => {
                self.open.entry(subject.clone()).or_default().push(question.id.clone());
                vec![]
            }
            Event::QuestionAnswered { question, .. } => {
                for qs in self.open.values_mut() {
                    qs.retain(|q| q != question);
                }
                self.open.retain(|_, qs| !qs.is_empty());
                vec![question.clone()]
            }
            Event::VerdictRecorded { subject, verdict, .. } if !matches!(verdict, Verdict::Ask(_)) => {
                self.open.remove(subject).unwrap_or_default()
            }
            Event::Interrupted { hard: true, .. } => std::mem::take(&mut self.open).into_values().flatten().collect(),
            _ => vec![],
        }
    }
}

/// Forward one session's events (reliable) and pulses (lossy) to a
/// connection, closing questions as the stream answers them.
async fn pump(
    session: SessionId,
    mut events: BoxStream<'static, Envelope<Event>>,
    mut pulses: Option<broadcast::Receiver<Pulse>>,
    out: Outgoing,
    closed: Closed,
) {
    let mut tracker = QuestionTracker::default();
    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(e) => {
                    let seq = e.seq;
                    let closes = tracker.closes(&e.body);
                    let msg = ServerMessage::Event { session: session.clone(), event: Box::new(e) };
                    let sid = &session;
                    let resume = move |from: Seq| move || slow_consumer(format!("resubscribe to {sid} with from_seq {from}"));
                    if out.reliable(msg, resume(seq)).await.is_err() {
                        return;
                    }
                    for q in closes {
                        if !closed.lock().unwrap().insert(&q) {
                            continue;
                        }
                        let msg = ServerMessage::QuestionClosed { session: session.clone(), question: q };
                        if out.reliable(msg, resume(seq + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                None => {
                    let msg = ServerMessage::Error { message: format!("session {session} closed") };
                    let _ = out.reliable(msg, || slow_consumer("reconnect")).await;
                    return;
                }
            },
            p = next_pulse(&mut pulses) => match p {
                Some(pulse) => out.lossy(ServerMessage::Pulse { session: session.clone(), pulse }),
                None => pulses = None,
            },
        }
    }
}

/// Next pulse; lagged receivers skip ahead. `None` when the channel closed;
/// pending forever when not subscribed.
async fn next_pulse(rx: &mut Option<broadcast::Receiver<Pulse>>) -> Option<Pulse> {
    let Some(rx) = rx else { return std::future::pending().await };
    loop {
        match rx.recv().await {
            Ok(p) => return Some(p),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str) -> Question {
        serde_json::from_value(serde_json::json!({"id": id, "prompt": "?"})).unwrap()
    }

    #[test]
    fn tracker_closes_on_answer_verdict_and_hard_interrupt() {
        let mut t = QuestionTracker::default();
        let call = GateRef::Call(CallId::new("c1"));
        assert!(t.closes(&Event::QuestionAsked { question: q("a"), subject: call.clone() }).is_empty());
        assert!(t.closes(&Event::QuestionAsked { question: q("b"), subject: GateRef::Session }).is_empty());
        assert!(t.closes(&Event::QuestionAsked { question: q("c"), subject: GateRef::Turn(1) }).is_empty());
        // An `Ask` verdict does not close anything.
        let ask = Event::VerdictRecorded {
            subject: call.clone(),
            point: HookPoint::Permission,
            ring: Ring::Hook,
            verdict: Verdict::ask("x"),
            responder: Responder::Hook("h".into()),
        };
        assert!(t.closes(&ask).is_empty());
        let allow = Event::VerdictRecorded {
            subject: call,
            point: HookPoint::Permission,
            ring: Ring::Human,
            verdict: Verdict::Allow,
            responder: Responder::AutoRule("r".into()),
        };
        assert_eq!(t.closes(&allow), vec![QuestionId::new("a")]);
        let answered = Event::QuestionAnswered {
            question: QuestionId::new("b"),
            answer: Answer::Allow { remember: false },
            responder: Responder::Code,
        };
        assert_eq!(t.closes(&answered), vec![QuestionId::new("b")]);
        assert_eq!(t.closes(&Event::Interrupted { hard: false, epoch: 1 }), vec![]);
        assert_eq!(t.closes(&Event::Interrupted { hard: true, epoch: 1 }), vec![QuestionId::new("c")]);
    }

    #[test]
    fn closed_set_is_bounded_and_dedupes() {
        let mut c = ClosedSet::default();
        assert!(c.insert(&QuestionId::new("x")));
        assert!(!c.insert(&QuestionId::new("x")));
        for i in 0..CLOSED_DEDUPE + 10 {
            c.insert(&QuestionId::new(format!("q{i}")));
        }
        assert_eq!(c.set.len(), CLOSED_DEDUPE);
        assert!(c.insert(&QuestionId::new("x")), "evicted ids can close again");
    }
}
