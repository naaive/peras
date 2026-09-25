//! Transports: how [`ServerMessage`]s go out and [`ClientMessage`]s come in.
//!
//! Every transport splits into an independent read half ([`TransportRead`])
//! and write half ([`TransportWrite`]); the server drives them from separate
//! tasks, so a peer that stops reading never stops the server from reading
//! (and vice versa).
//!
//! - [`channel`] — in-process channel pair ([`ChannelTransport`] / [`ChannelClient`]).
//! - [`JsonLines`] — one JSON document per line over any `AsyncRead + AsyncWrite`
//!   (stdio, pipes, sockets, `tokio::io::duplex`).
//! - [`WsTransport`] — WebSocket text frames (see [`crate::Server::serve_ws`]);
//!   [`connect_ws`] is the matching client.

use agent_proto::{ClientMessage, ServerMessage};
use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
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

/// The server side of a connection: split once into independent halves.
pub trait Transport: Send + 'static {
    type Reader: TransportRead;
    type Writer: TransportWrite;
    fn split(self) -> (Self::Reader, Self::Writer);
}

/// Read half. `Ok(None)` means the peer closed cleanly;
/// [`TransportError::Decode`] is reported to the peer and the connection goes
/// on; any other error ends it.
#[async_trait]
pub trait TransportRead: Send + 'static {
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError>;
}

/// Write half.
#[async_trait]
pub trait TransportWrite: Send + 'static {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError>;
    /// Flush and close (best effort). Called once when the connection ends.
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

// ---------------------------------------------------------------- in-process

/// Buffer of each direction of an in-process [`channel`].
pub const CHANNEL_CAPACITY: usize = 1024;

/// Server end of an in-process connection.
pub struct ChannelTransport {
    tx: mpsc::Sender<ServerMessage>,
    rx: mpsc::Receiver<ClientMessage>,
}

/// Read half of a [`ChannelTransport`].
pub struct ChannelReader(mpsc::Receiver<ClientMessage>);
/// Write half of a [`ChannelTransport`].
pub struct ChannelWriter(mpsc::Sender<ServerMessage>);

/// Client end of a connection (in-process, or bridged by [`connect_ws`]).
/// Dropping it closes the connection.
pub struct ChannelClient {
    tx: mpsc::Sender<ClientMessage>,
    rx: mpsc::Receiver<ServerMessage>,
}

/// An in-process connection: hand the transport to [`crate::Server::serve`],
/// keep the client.
pub fn channel() -> (ChannelTransport, ChannelClient) {
    channel_with_capacity(CHANNEL_CAPACITY)
}

/// [`channel`] with an explicit per-direction buffer.
pub fn channel_with_capacity(capacity: usize) -> (ChannelTransport, ChannelClient) {
    let (ctx, srx) = mpsc::channel(capacity.max(1));
    let (stx, crx) = mpsc::channel(capacity.max(1));
    (ChannelTransport { tx: stx, rx: srx }, ChannelClient { tx: ctx, rx: crx })
}

impl Transport for ChannelTransport {
    type Reader = ChannelReader;
    type Writer = ChannelWriter;
    fn split(self) -> (ChannelReader, ChannelWriter) {
        (ChannelReader(self.rx), ChannelWriter(self.tx))
    }
}

#[async_trait]
impl TransportRead for ChannelReader {
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        Ok(self.0.recv().await)
    }
}

#[async_trait]
impl TransportWrite for ChannelWriter {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        self.0.send(msg).await.map_err(|_| TransportError::Closed)
    }
}

impl ChannelClient {
    /// Assemble a client from raw channel ends (custom bridges).
    pub fn from_parts(tx: mpsc::Sender<ClientMessage>, rx: mpsc::Receiver<ServerMessage>) -> Self {
        ChannelClient { tx, rx }
    }
    pub fn into_parts(self) -> (mpsc::Sender<ClientMessage>, mpsc::Receiver<ServerMessage>) {
        (self.tx, self.rx)
    }
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
/// [`JsonLines::read`] (or [`JsonLines::into_halves`] for full duplex).
pub struct JsonLines<R, W> {
    r: JsonLinesReader<R>,
    w: JsonLinesWriter<W>,
}

/// Read half of [`JsonLines`].
pub struct JsonLinesReader<R> {
    lines: Lines<BufReader<R>>,
}

/// Write half of [`JsonLines`].
pub struct JsonLinesWriter<W> {
    w: W,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> JsonLines<R, W> {
    pub fn new(r: R, w: W) -> Self {
        JsonLines { r: JsonLinesReader { lines: BufReader::new(r).lines() }, w: JsonLinesWriter { w } }
    }

    /// Write one message as a line and flush.
    pub async fn write<T: Serialize + Sync>(&mut self, msg: &T) -> Result<(), TransportError> {
        self.w.write(msg).await
    }

    /// Read the next message (blank lines are skipped). Cancel-safe.
    pub async fn read<T: DeserializeOwned>(&mut self) -> Result<Option<T>, TransportError> {
        self.r.read().await
    }

    pub fn into_halves(self) -> (JsonLinesReader<R>, JsonLinesWriter<W>) {
        (self.r, self.w)
    }

    pub fn into_inner(self) -> (R, W) {
        (self.r.lines.into_inner().into_inner(), self.w.w)
    }
}

impl<R: AsyncRead + Unpin> JsonLinesReader<R> {
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
}

impl<W: AsyncWrite + Unpin> JsonLinesWriter<W> {
    /// Write one message as a line and flush.
    pub async fn write<T: Serialize + Sync>(&mut self, msg: &T) -> Result<(), TransportError> {
        let mut line = serde_json::to_vec(msg).map_err(|e| TransportError::Decode(e.to_string()))?;
        line.push(b'\n');
        self.w.write_all(&line).await?;
        self.w.flush().await?;
        Ok(())
    }
}

impl JsonLines<tokio::io::Stdin, tokio::io::Stdout> {
    /// Process stdin / stdout.
    pub fn stdio() -> Self {
        JsonLines::new(tokio::io::stdin(), tokio::io::stdout())
    }
}

impl<R, W> Transport for JsonLines<R, W>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Reader = JsonLinesReader<R>;
    type Writer = JsonLinesWriter<W>;
    fn split(self) -> (Self::Reader, Self::Writer) {
        self.into_halves()
    }
}

#[async_trait]
impl<R: AsyncRead + Unpin + Send + 'static> TransportRead for JsonLinesReader<R> {
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        self.read().await
    }
}

#[async_trait]
impl<W: AsyncWrite + Unpin + Send + 'static> TransportWrite for JsonLinesWriter<W> {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        self.write(&msg).await
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        self.w.shutdown().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- WebSocket

/// WebSocket transport: one JSON message per text (or binary) frame.
pub struct WsTransport<S> {
    ws: WebSocketStream<S>,
}

/// Read half of [`WsTransport`].
pub struct WsReader<S>(SplitStream<WebSocketStream<S>>);
/// Write half of [`WsTransport`].
pub struct WsWriter<S>(SplitSink<WebSocketStream<S>, Message>);

impl<S> WsTransport<S> {
    pub fn new(ws: WebSocketStream<S>) -> Self {
        WsTransport { ws }
    }
}

impl<S> Transport for WsTransport<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Reader = WsReader<S>;
    type Writer = WsWriter<S>;
    fn split(self) -> (WsReader<S>, WsWriter<S>) {
        let (w, r) = self.ws.split();
        (WsReader(r), WsWriter(w))
    }
}

/// Decode the next JSON frame of a WebSocket stream (control frames skipped).
async fn ws_next<T, St>(st: &mut St) -> Result<Option<T>, TransportError>
where
    T: DeserializeOwned,
    St: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = match st.next().await {
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

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> TransportRead for WsReader<S> {
    async fn recv(&mut self) -> Result<Option<ClientMessage>, TransportError> {
        ws_next(&mut self.0).await
    }
}

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> TransportWrite for WsWriter<S> {
    async fn send(&mut self, msg: ServerMessage) -> Result<(), TransportError> {
        let text = serde_json::to_string(&msg).map_err(|e| TransportError::Decode(e.to_string()))?;
        self.0.send(Message::Text(text)).await.map_err(ws_err)
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        self.0.close().await.map_err(ws_err)
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

/// Connect to a WebSocket server (`ws://host:port`) and bridge it to a
/// [`ChannelClient`]: an independent reader task and writer task, so the
/// client never deadlocks with the server. Malformed server frames are
/// skipped; the client's `recv` returns `None` once the socket closed.
pub async fn connect_ws(url: &str) -> Result<ChannelClient, TransportError> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.map_err(ws_err)?;
    let (mut sink, mut stream) = ws.split();
    let (ctx, mut crx) = mpsc::channel::<ClientMessage>(CHANNEL_CAPACITY);
    let (stx, srx) = mpsc::channel::<ServerMessage>(CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(msg) = crx.recv().await {
            let Ok(text) = serde_json::to_string(&msg) else { continue };
            if sink.send(Message::Text(text)).await.is_err() {
                return;
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        loop {
            match ws_next::<ServerMessage, _>(&mut stream).await {
                Ok(Some(m)) => {
                    if stx.send(m).await.is_err() {
                        return;
                    }
                }
                Err(TransportError::Decode(e)) => tracing::debug!(error = %e, "skipping malformed server frame"),
                Ok(None) | Err(_) => return,
            }
        }
    });
    Ok(ChannelClient::from_parts(ctx, srx))
}
