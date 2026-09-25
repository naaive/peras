//! The terminal loop: raw mode + alternate screen, keys -> model -> client
//! messages, server messages -> model, redraw. Reconnects (resuming from the
//! model's `next_seq`) when the connection drops.

use crate::model::{Action, Model};
use crate::ui;
use agent_proto::{ClientMessage, ServerMessage, SessionId};
use agent_server::{ChannelClient, TransportError};
use crossterm::event::{Event as TermEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::future::BoxFuture;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

/// Opens a fresh connection (called again after every disconnect).
pub type Connect = Box<dyn FnMut() -> BoxFuture<'static, Result<ChannelClient, TransportError>> + Send>;

/// Connect to a remote server over WebSocket (`ws://host:port`).
pub fn ws(url: impl Into<String>) -> Connect {
    let url = url.into();
    Box::new(move || {
        let url = url.clone();
        Box::pin(async move { agent_server::connect_ws(&url).await })
    })
}

/// Connect to an in-process server.
pub fn in_process<D>(server: agent_server::Server<D>) -> Connect
where
    D: agent_kernel::Decider + 'static,
    D::State: Send + Sync + 'static,
{
    Box::new(move || {
        let c = server.connect();
        Box::pin(async move { Ok(c) })
    })
}

/// Restores the terminal on drop (also on panic unwinding).
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<TermGuard> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Run the TUI on `session` until the user quits.
pub async fn run(session: SessionId, mut connect: Connect) -> io::Result<()> {
    let client_id = format!("tui-{}", ulid::Ulid::new());
    let mut model = Model::new(session, client_id);

    let _guard = TermGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut keys = EventStream::new();

    let mut conn: Option<ChannelClient> = None;
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        draw(&mut terminal, &mut model)?;
        let disconnected = conn.is_none();
        let incoming = async {
            match conn.as_mut() {
                Some(c) => c.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            k = keys.next() => match k {
                Some(Ok(TermEvent::Key(key))) => {
                    for action in model.on_key(key) {
                        match action {
                            Action::Quit => return Ok(()),
                            Action::Send(msg) => send(&mut conn, &mut model, msg).await,
                        }
                    }
                }
                Some(Ok(_)) => {} // resize etc.: redraw
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            m = incoming => match m {
                Some(m) => {
                    model.apply(m);
                    // Drain whatever else is ready before redrawing.
                    if let Some(c) = conn.as_mut() {
                        for _ in 0..256 {
                            match c.try_recv() {
                                Some(m) => model.apply(m),
                                None => break,
                            }
                        }
                    }
                    if !model.connected && conn.is_some() {
                        // Dropped by the server (slow consumer): reconnect.
                        conn = None;
                    }
                }
                None => {
                    conn = None;
                    model.disconnected();
                }
            },
            _ = retry.tick(), if disconnected => {
                match connect().await {
                    Ok(c) => {
                        let mut ok = true;
                        for m in model.handshake() {
                            ok &= c.send(m).await.is_ok();
                        }
                        if ok {
                            conn = Some(c);
                        }
                    }
                    Err(e) => model.hint = Some(format!("connect failed: {e}")),
                }
            }
        }
    }
}

async fn send(conn: &mut Option<ChannelClient>, model: &mut Model, msg: ClientMessage) {
    let delivered = match conn.as_ref() {
        Some(c) => c.send(msg).await.is_ok(),
        None => false,
    };
    if !delivered {
        model.apply(ServerMessage::Error { message: "not connected; message not sent".into() });
    }
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, model: &mut Model) -> io::Result<()> {
    let size = terminal.size()?;
    let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
    let a = ui::areas(model, area);
    let total = ui::transcript_lines(model, a.transcript.width.max(1) as usize).len();
    let height = a.transcript.height as usize;
    model.page = height.saturating_sub(1).max(1);
    model.scroll = model.scroll.min(total.saturating_sub(height));
    terminal.draw(|f| ui::render(f, model))?;
    Ok(())
}
