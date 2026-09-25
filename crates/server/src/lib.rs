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
//! - **Command** is forwarded with its idempotency key; the reply is an `Ack`
//!   (a retried key gets the original ack). `Control::Answer` sent as a command
//!   takes the answer path below.
//! - **Answer** is compare-and-swap through the runtime's ask board: the winner
//!   gets `Ack { accepted: true }` and every *other* connection subscribed to the
//!   session gets `QuestionClosed`; losers get
//!   `Ack { accepted: false, error: "already answered" }`. Answer keys are
//!   idempotent too.
//! - **Ping** -> `Pong`.
//!
//! Unknown sessions are obtained through a [`SessionOpener`] ([`ResumeOnly`],
//! [`OpenWith`] closure adapter, or your own).

mod server;
pub mod transport;

pub use server::{
    negotiate, ConnError, OpenWith, ResumeOnly, Server, SessionOpener, ALREADY_ANSWERED, OUTGOING_CAPACITY,
    SUPPORTED_VERSIONS,
};
pub use transport::{channel, ChannelClient, ChannelTransport, JsonLines, Transport, TransportError, WsTransport};
