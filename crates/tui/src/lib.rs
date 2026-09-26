//! `agent-tui`: a terminal client for the session service.
//!
//! It speaks the wire protocol of [`agent_proto::protocol`] over any
//! connection that yields an [`agent_server::ChannelClient`]: an in-process
//! [`agent_server::Server`] ([`app::in_process`]) or a remote WebSocket server
//! ([`app::ws`]).
//!
//! - [`model`] — the pure view-model: [`Model::apply`] folds server messages,
//!   [`Model::on_key`] turns keys into client messages.
//! - [`ui`] — stateless rendering of a model (ratatui).
//! - [`app`] — the terminal loop with reconnect-and-resume.
//!
//! Keys: Enter = submit (idle) / steer (busy); Alt+Enter or Ctrl+Enter =
//! queue; Esc = soft interrupt; Ctrl+C twice = hard interrupt (quit when
//! idle); Ctrl+D on empty input = quit; PgUp / PgDn = scroll; Ctrl+U = clear
//! input. With an approval pending and empty input: y = allow, a = allow
//! always, n = deny with a reason (Enter sends, Esc cancels).

pub mod app;
pub mod model;
pub mod ui;

pub use app::{in_process, run, ws, Connect};
pub use model::{Action, Entry, InputMode, Model, Phase};
