//! The session service: one [`Server`] per [`Runtime`], any number of
//! connections, any number of connections per session.

use crate::transport::{channel, ChannelClient, JsonLines, Transport, TransportError, WsTransport};
use agent_kernel::{Decider, Decision};
use agent_proto::*;
use agent_runtime::{AnswerError, DriverError, Runtime, SessionHandle};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Protocol versions this server speaks.
pub const SUPPORTED_VERSIONS: &[u32] = &[PROTOCOL_VERSION];

/// Outgoing buffer per connection. Events wait for room (reliable); pulses are
/// dropped when it is full (lossy).
pub const OUTGOING_CAPACITY: usize = 1024;

/// Answer idempotency keys remembered per session.
const ANSWER_DEDUPE: usize = 1024;

/// Ack error text for the losers of an approval race.
pub const ALREADY_ANSWERED: &str = "already answered";

/// Highest version both sides support.
pub fn negotiate(client: &[u32], server: &[u32]) -> Option<u32> {
    client.iter().copied().filter(|v| server.contains(v)).max()
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

// ---------------------------------------------------------------- server

type Ack = (bool, Option<String>);

/// Per-session fan-out state (outlives session handles: a closed and reopened
/// session keeps its subscribers' registrations).
#[derive(Default)]
struct Hub {
    /// Connections subscribed to this session.
    subscribers: Mutex<HashMap<u64, mpsc::Sender<ServerMessage>>>,
    /// Answer idempotency; the lock also serializes answers per session.
    answers: tokio::sync::Mutex<VecDeque<(String, Ack)>>,
}

struct Inner<D: Decider> {
    rt: Arc<Runtime<D>>,
    opener: Box<dyn SessionOpener<D>>,
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
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl<D> Server<D>
where
    D: Decider + 'static,
    D::State: Send + Sync + 'static,
{
    pub fn new(rt: Arc<Runtime<D>>, opener: impl SessionOpener<D> + 'static) -> Self {
        Server {
            inner: Arc::new(Inner {
                rt,
                opener: Box::new(opener),
                hubs: Mutex::new(HashMap::new()),
                open_lock: tokio::sync::Mutex::new(()),
                next_conn: AtomicU64::new(1),
            }),
        }
    }

    pub fn runtime(&self) -> &Arc<Runtime<D>> {
        &self.inner.rt
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

    /// Serve one connection until the peer disconnects (or version
    /// negotiation fails).
    pub async fn serve<T: Transport>(&self, mut transport: T) -> Result<(), ConnError> {
        let conn_id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        let (out_tx, mut out_rx) = mpsc::channel::<ServerMessage>(OUTGOING_CAPACITY);
        let (req_tx, req_rx) = mpsc::channel::<ClientMessage>(64);
        let conn = Conn { server: self.clone(), id: conn_id, out: out_tx, client: None, subs: HashMap::new() };
        let mut processor = tokio::spawn(conn.run(req_rx));
        let mut req_tx = Some(req_tx);
        let mut pending: Option<ClientMessage> = None;

        // IO loop: never blocks on request processing, so outgoing messages
        // always drain (no deadlock between acks and a full outgoing queue).
        let result = loop {
            let reserve = match (&pending, &req_tx) {
                (Some(_), Some(tx)) => Some(tx.clone().reserve_owned()),
                _ => None,
            };
            let reserving = reserve.is_some();
            tokio::select! {
                biased;
                done = &mut processor => {
                    break match done {
                        Ok(r) => r,
                        Err(e) => { tracing::error!(conn = conn_id, error = %e, "connection processor panicked"); Ok(()) }
                    };
                }
                Some(msg) = out_rx.recv() => {
                    if let Err(e) = transport.send(msg).await {
                        break Err(e.into());
                    }
                }
                permit = async move { reserve.unwrap().await }, if reserving => {
                    match permit {
                        Ok(p) => { p.send(pending.take().unwrap()); }
                        Err(_) => { pending = None; req_tx = None; }
                    }
                }
                r = transport.recv(), if pending.is_none() && req_tx.is_some() => match r {
                    Ok(Some(m)) => pending = Some(m),
                    Ok(None) => req_tx = None, // processor sees EOF, cleans up, ends
                    Err(TransportError::Decode(e)) => {
                        if let Err(e) = transport.send(ServerMessage::Error { message: format!("bad message: {e}") }).await {
                            break Err(e.into());
                        }
                    }
                    Err(e) => break Err(e.into()),
                },
            }
        };
        processor.abort();
        // Flush what was queued before the processor finished (e.g. the
        // version error), best effort.
        if matches!(result, Ok(()) | Err(ConnError::NoCommonVersion { .. })) {
            while let Ok(msg) = out_rx.try_recv() {
                if transport.send(msg).await.is_err() {
                    break;
                }
            }
        }
        result
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
    out: mpsc::Sender<ServerMessage>,
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
        let _ = self.out.send(msg).await;
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
        hub.subscribers.lock().unwrap().insert(self.id, self.out.clone());
        let events = handle.subscribe(from_seq);
        let pulses = pulses.then(|| handle.pulses());
        let task = tokio::spawn(pump(session.clone(), events, pulses, self.out.clone()));
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
                    let others: Vec<_> = hub
                        .subscribers
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(id, _)| **id != self.id)
                        .map(|(_, tx)| tx.clone())
                        .collect();
                    for tx in others {
                        let msg = ServerMessage::QuestionClosed { session: session.clone(), question: question.clone() };
                        // Never let a slow peer stall this connection.
                        if let Err(mpsc::error::TrySendError::Full(msg)) = tx.try_send(msg) {
                            tokio::spawn(async move {
                                let _ = tx.send(msg).await;
                            });
                        }
                    }
                }
                ack
            }
        };
        self.reply(ServerMessage::Ack { key, accepted: ack.0, error: ack.1 }).await;
    }
}

fn to_ack<T>(r: Result<T, DriverError>) -> Ack {
    match r {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    }
}

/// Forward one session's events (reliable) and pulses (lossy) to a connection.
async fn pump(
    session: SessionId,
    mut events: BoxStream<'static, Envelope<Event>>,
    mut pulses: Option<broadcast::Receiver<Pulse>>,
    out: mpsc::Sender<ServerMessage>,
) {
    loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(e) => {
                    let msg = ServerMessage::Event { session: session.clone(), event: Box::new(e) };
                    if out.send(msg).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = out.send(ServerMessage::Error { message: format!("session {session} closed") }).await;
                    return;
                }
            },
            p = next_pulse(&mut pulses) => match p {
                Some(pulse) => {
                    // Lossy: drop when the connection is backed up.
                    let _ = out.try_send(ServerMessage::Pulse { session: session.clone(), pulse });
                }
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
