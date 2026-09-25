//! `agent-server`: the session service with multi-client fan-out.
//!
//! A [`Server`] wraps a [`agent_runtime::Runtime`] and serves the wire protocol
//! of [`agent_proto::protocol`] over any [`Transport`]:
//!
//! - **Hello** negotiates the highest common protocol version
//!   ([`SUPPORTED_VERSIONS`]); no common version -> `Error` and the connection
//!   closes. Everything but `Ping` requires a successful `Hello` first.
//! - **Subscribe** streams events from `from_seq` (replay, then live; ordered,
//!   gap-free) and, optionally, pulses (lossy: dropped when the connection is
//!   backed up, lagged receivers skip ahead). Subscribing again replaces the
//!   previous subscription to that session.
//! - **Flow control**: every connection has an independent reader and writer
//!   (see [`transport`]); a client that stops reading never blocks the server
//!   from reading. When a reliable message cannot be queued within
//!   [`ServerOptions::stall_timeout`], the client gets an `Error` starting with
//!   [`SLOW_CONSUMER`] naming the `from_seq` to resubscribe from, and is
//!   disconnected.
//! - **Command** is forwarded with its idempotency key; the reply is an `Ack`
//!   (a retried key gets the original ack). `Control::Answer` sent as a command
//!   takes the answer path below.
//! - **Answer** is compare-and-swap through the runtime's ask board: the winner
//!   gets `Ack { accepted: true }` and every *other* connection subscribed to the
//!   session gets `QuestionClosed`; losers get
//!   `Ack { accepted: false, error: "already answered" }`. Answer keys are
//!   idempotent too.
//! - **QuestionClosed** is also derived from the event stream, so answers that
//!   never went through this server (in-process code, auto rules, another
//!   server) close dialogs too: `QuestionAnswered`, a final verdict for the
//!   asked subject, or a hard interrupt. Each connection gets at most one
//!   `QuestionClosed` per question.
//! - **Ping** -> `Pong`.
//!
//! Unknown sessions are obtained through a [`SessionOpener`] ([`ResumeOnly`],
//! [`OpenWith`] closure adapter, or your own).

mod server;
pub mod transport;

pub use server::{
    negotiate, ConnError, OpenWith, ResumeOnly, Server, ServerOptions, SessionOpener, ALREADY_ANSWERED,
    OUTGOING_CAPACITY, SLOW_CONSUMER, STALL_TIMEOUT, SUPPORTED_VERSIONS,
};
pub use transport::{
    channel, channel_with_capacity, connect_ws, ChannelClient, ChannelTransport, JsonLines, Transport, TransportError,
    TransportRead, TransportWrite, WsTransport,
};
