//! Transports: how [`ServerMessage`]s go out and [`ClientMessage`]s come in.
//!
//! - [`channel`] — in-process channel pair ([`ChannelTransport`] / [`ChannelClient`]).
//! - [`JsonLines`] — one JSON document per line over any `AsyncRead + AsyncWrite`
//!   (stdio, pipes, sockets, `tokio::io::duplex`).
//! - [`WsTransport`] — WebSocket text frames (see [`crate::Server::serve_ws`]).

use agent_proto::{ClientMessage, ServerMessage};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The peer is gone.
    #[error("transport closed")]
    Closed,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A malformed message; the connection stays usable.
    #[error("decode: {0}")]
    Decode(String),
    #[error("websocket: {0}")]
    Ws(String),
}

/// The server side of a connection.
///
/// `recv` MUST be cancel-safe: the connection loop polls it in a `select!`
/// together with outgoing messages. `Ok(None)` means the peer closed cleanly.
/// [`TransportError::Decode`] is reported to the peer and the connection goes
/// on; any other error ends it.
///
/// A connection is half-duplex while a `send` is blocked (the peer is not
/// reading); clients must keep reading while they write.
#[async_trait]
pub trait Transport: Send + 'static {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError>;
}

// ---------------------------------------------------------------- in-process

/// Buffer of each direction of an in-process [`channel`].
pub const CHANNEL_CAPACITY: usize = 1024;

/// Server end of an in-process connection.
pub struct ChannelTransport {
    tx: mpsc::Sender<ServerMessage>,
    rx: mpsc::Receiver<ClientMessage>,
}

/// Client end of an in-process connection. Dropping it closes the connection.
pub struct ChannelClient {
    tx: mpsc::Sender<ClientMessage>,
    rx: mpsc::Receiver<ServerMessage>,
}

/// An in-process connection: hand the transport to [`crate::Server::serve`],
/// keep the client.
pub fn channel() -> (ChannelTransport, ChannelClient) {
    let (ctx, srx) = mpsc::channel(CHANNEL_CAPACITY);
    let (stx, crx) = mpsc::channel(CHANNEL_CAPACITY);
    (ChannelTransport { tx: stx, rx: srx }, ChannelClient { tx: ctx, rx: crx })
}

#[async_trait]
impl Transport for ChannelTransport {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        self.tx.send(msg).await.map_err(|_| TransportError::Closed)
    }
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        Ok(self.rx.recv().await)
    }
}

impl ChannelClient {
    pub async fn send(&self, msg: ClientMessage) -> Result<(), TransportError> {
        self.tx.send(msg).await.map_err(|_| TransportError::Closed)
    }
    /// Next message; `None` once the server closed the connection.
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.rx.recv().await
    }
    pub fn try_recv(&mut self) -> Option<ServerMessage> {
        self.rx.try_recv().ok()
    }
    /// A sender usable from other tasks.
    pub fn sender(&self) -> mpsc::Sender<ClientMessage> {
        self.tx.clone()
    }
}

// ---------------------------------------------------------------- JSON Lines

/// JSON Lines over a reader / writer pair. Usable on both sides: as a server
/// [`Transport`], and as a client through [`JsonLines::write`] /
/// [`JsonLines::read`].
pub struct JsonLines<R, W> {
    lines: Lines<BufReader<R>>,
    w: W,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> JsonLines<R, W> {
    pub fn new(r: R, w: W) -> Self {
        JsonLines { lines: BufReader::new(r).lines(), w }
    }

    /// Write one message as a line and flush.
    pub async fn write<T: Serialize + Sync>(&mut self, msg: &T) -> Result<(), TransportError> {
        let mut line = serde_json::to_vec(msg).map_err(|e| TransportError::Decode(e.to_string()))?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        self.w.flush().await?;
        Ok(())
    }

    /// Read the next message (blank lines are skipped). Cancel-safe.
    pub async fn read<T: DeserializeOwned>(&mut self) -> Result<Option<T>, TransportError> {
        loop {
            match self.lines.next_line().await? {
                None => return Ok(None),
                Some(l) if l.trim().is_empty() => continue,
                Some(l) => return serde_json::from_str(&l).map(Some).map_err(|e| TransportError::Decode(e.to_string())),
            }
        }
    }

    pub fn into_inner(self) -> (R, W) {
        (self.lines.into_inner().into_inner(), self.w)
    }
}

impl JsonLines<tokio::io::Stdin, tokio::io::Stdout> {
    /// Process stdin / stdout.
    pub fn stdio() -> Self {
        JsonLines::new(tokio::io::stdin(), tokio::io::stdout())
    }
}

#[async_trait]
impl<R, W> Transport for JsonLines<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        self.write(&msg).await
    }
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        self.read().await
    }
}

// ---------------------------------------------------------------- WebSocket

/// WebSocket transport: one JSON message per text (or binary) frame.
pub struct WsTransport<S> {
    ws: WebSocketStream<S>,
}

impl<S> WsTransport<S> {
    pub fn new(ws: WebSocketStream<S>) -> Self {
        WsTransport { ws }
    }
}

#[async_trait]
impl<S> Transport for WsTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        let text = serde_json::to_string(&msg).map_err(|e| TransportError::Decode(e.to_string()))?;
        self.ws.send(Message::Text(text)).await.map_err(ws_err)
    }

    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        loop {
            let msg = match self.ws.next().await {
                None => return Ok(None),
                Some(m) => m.map_err(ws_err)?,
            };
            let parsed = match msg {
                Message::Text(t) => serde_json::from_str(&t),
                Message::Binary(b) => serde_json::from_slice(&b),
                Message::Close(_) => return Ok(None),
                // Pings are answered by tungstenite itself.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            return parsed.map(Some).map_err(|e| TransportError::Decode(e.to_string()));
        }
    }
}

fn ws_err(e: tokio_tungstenite::tungstenite::Error) -> TransportError {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::ConnectionClosed | E::AlreadyClosed => TransportError::Closed,
        E::Io(io) => TransportError::Io(io),
        other => TransportError::Ws(other.to_string()),
    }
}
