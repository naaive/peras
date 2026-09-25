//! `Run`: both a `Future` (the final text) and a `Stream` of updates. It starts
//! lazily on first poll. Iterating first and then awaiting continues from what
//! is left (nothing runs twice).
//!
//! Ending a run: dropping an unfinished `Run` is a soft interrupt; `detach()`
//! keeps it going in the background and returns the session id; `cancel()` is
//! a hard interrupt.

use crate::agent::Agent;
use crate::error::Error;
use agent_kernel::Kernel;
use agent_proto::*;
use agent_runtime::SessionHandle;
use futures::{Stream, StreamExt};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context as TaskCx, Poll};
use tokio::sync::mpsc;

/// What happens during a run.
#[derive(Debug)]
pub enum Update {
    /// Streamed text delta (lossy pulse stream; the final text is authoritative).
    Text(String),
    Thinking(String),
    /// A complete model reply.
    Reply(AssistantMessage),
    /// A tool finished.
    Tool { call: ToolCall, result: ToolResult },
    /// Something needs approval. Answer it (`allow`/`deny`); interactive answers
    /// from code count as the user.
    Ask(Ask),
    /// Any other journal event visible to the user.
    Event(Box<Envelope<Event>>),
    /// The turn ended (always the last update).
    Done(TurnOutcome),
}

pub(crate) enum Ctrl {
    Input(Input),
    Answer(QuestionId, Answer),
}

/// A pending approval.
#[derive(Debug)]
pub struct Ask {
    pub question: Question,
    tx: mpsc::UnboundedSender<Ctrl>,
}

impl Ask {
    pub fn allow(&self) {
        self.answer(Answer::Allow { remember: false });
    }
    /// Allow and remember the destination for the rest of the session.
    pub fn allow_always(&self) {
        self.answer(Answer::Allow { remember: true });
    }
    pub fn deny(&self, reason: impl Into<String>) {
        self.answer(Answer::Deny { reason: Some(reason.into()) });
    }
    pub fn answer(&self, a: Answer) {
        let _ = self.tx.send(Ctrl::Answer(self.question.id.clone(), a));
    }
}

/// Steer / interrupt a run.
#[derive(Clone)]
pub struct RunControl {
    tx: mpsc::UnboundedSender<Ctrl>,
}

impl RunControl {
    /// Delivered at the next safe point without interrupting current work.
    pub fn steer(&self, text: impl Into<String>) {
        let _ = self.tx.send(Ctrl::Input(Input::Signal(Signal::Steer { text: text.into() })));
    }
    /// Starts a new turn after this one.
    pub fn queue(&self, text: impl Into<String>) {
        let _ = self.tx.send(Ctrl::Input(Input::Signal(Signal::Queue { text: text.into() })));
    }
    pub fn notify(&self, source: impl Into<String>, key: impl Into<String>, text: impl Into<String>) {
        let _ = self.tx.send(Ctrl::Input(Input::Signal(Signal::Notify {
            source: source.into(),
            key: key.into(),
            text: text.into(),
            untrusted: false,
        })));
    }
    pub fn soft_interrupt(&self) {
        let _ = self.tx.send(Ctrl::Input(Input::Control(Control::SoftInterrupt)));
    }
    pub fn interrupt(&self) {
        let _ = self.tx.send(Ctrl::Input(Input::Control(Control::HardInterrupt)));
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Ctrl::Input(Input::Control(Control::Pause)));
    }
    pub fn resume(&self) {
        let _ = self.tx.send(Ctrl::Input(Input::Control(Control::Resume)));
    }
}

pub(crate) enum Target {
    New(SessionId),
    Open(SessionId),
}

impl Target {
    fn id(&self) -> &SessionId {
        match self {
            Target::New(id) | Target::Open(id) => id,
        }
    }
}

struct Pending {
    agent: Agent,
    target: Target,
    input: Input,
}

enum St {
    NotStarted(Box<Pending>),
    Running(mpsc::UnboundedReceiver<Update>),
    Done,
}

pub struct Run {
    session: SessionId,
    st: St,
    ctrl_tx: mpsc::UnboundedSender<Ctrl>,
    ctrl_rx: Option<mpsc::UnboundedReceiver<Ctrl>>,
    outcome: Option<TurnOutcome>,
    detached: bool,
}

impl Run {
    pub(crate) fn new(agent: Agent, target: Target, prompt: String) -> Run {
        Run::with_input(agent, target, Input::Signal(Signal::Submit { text: prompt, attachments: vec![] }))
    }

    pub(crate) fn with_input(agent: Agent, target: Target, input: Input) -> Run {
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        Run {
            session: target.id().clone(),
            st: St::NotStarted(Box::new(Pending { agent, target, input })),
            ctrl_tx,
            ctrl_rx: Some(ctrl_rx),
            outcome: None,
            detached: false,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    pub fn control(&self) -> RunControl {
        RunControl { tx: self.ctrl_tx.clone() }
    }

    /// Keep running in the background; returns the session id.
    pub fn detach(mut self) -> SessionId {
        self.detached = true;
        self.start();
        if let St::Running(rx) = std::mem::replace(&mut self.st, St::Done) {
            // Drain in the background so the driving task never blocks.
            tokio::spawn(async move {
                let mut rx = rx;
                while rx.recv().await.is_some() {}
            });
        }
        self.session.clone()
    }

    /// Hard interrupt; returns the outcome.
    pub async fn cancel(mut self) -> Result<TurnOutcome, Error> {
        self.start();
        self.control().interrupt();
        while self.next().await.is_some() {}
        self.outcome.clone().ok_or(Error::Ended)
    }

    /// Ask for a JSON answer matching `T`'s schema and parse it.
    pub async fn json<T>(mut self) -> Result<T, Error>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema,
    {
        if let St::NotStarted(p) = &mut self.st {
            let Input::Signal(Signal::Submit { text, .. }) = &mut p.input else { return self.await.and_then(|o| parse_json(&o)) };
            let schema = serde_json::to_string(&schemars::schema_for!(T)).unwrap_or_default();
            text.push_str(&format!(
                "\n\nRespond with only a single JSON value (no prose, no code fences) matching this JSON Schema:\n{schema}"
            ));
        }
        let out = self.await?;
        parse_json(&out)
    }

    fn start(&mut self) {
        if !matches!(self.st, St::NotStarted(_)) {
            return;
        }
        let St::NotStarted(p) = std::mem::replace(&mut self.st, St::Done) else { unreachable!() };
        let Pending { agent, target, input } = *p;
        let (tx, rx) = mpsc::unbounded_channel();
        let ctrl_rx = self.ctrl_rx.take().expect("control receiver");
        let ask_tx = self.ctrl_tx.clone();
        tokio::spawn(async move {
            let handle = match &target {
                Target::New(id) | Target::Open(id) => agent.open(id).await,
            };
            match handle {
                Ok(h) => drive(h, input, ctrl_rx, ask_tx, tx).await,
                Err(e) => {
                    let _ = tx.send(Update::Done(TurnOutcome::Failed { error: e.to_string() }));
                }
            }
        });
        self.st = St::Running(rx);
    }
}

pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(out: &str) -> Result<T, Error> {
    let t = out.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end().trim_end_matches("```"))
        .unwrap_or(t)
        .trim();
    if let Ok(v) = serde_json::from_str(t) {
        return Ok(v);
    }
    // Fall back to the outermost JSON object/array in the text.
    let start = t.find(['{', '[']);
    let end = t.rfind(['}', ']']);
    match (start, end) {
        (Some(s), Some(e)) if e > s => serde_json::from_str(&t[s..=e]).map_err(|e| Error::Json(e.to_string())),
        _ => Err(Error::Json(format!("no JSON in answer: {t}"))),
    }
}

/// Drive one turn on a session: subscribe first (no gaps), send the input, map
/// events and pulses into updates until `TurnEnded`.
pub(crate) async fn drive(
    h: SessionHandle<Kernel>,
    first: Input,
    mut ctrl: mpsc::UnboundedReceiver<Ctrl>,
    ask_tx: mpsc::UnboundedSender<Ctrl>,
    tx: mpsc::UnboundedSender<Update>,
) {
    let from = h.next_seq();
    let mut events = h.subscribe(from);
    let mut pulses = h.pulses();
    if let Err(e) = h.send(first).await {
        let _ = tx.send(Update::Done(TurnOutcome::Failed { error: e.to_string() }));
        return;
    }
    let mut ctrl_open = true;
    let mut pulses_open = true;
    loop {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else {
                    let _ = tx.send(Update::Done(TurnOutcome::Failed { error: "session closed".into() }));
                    return;
                };
                let (update, done) = map_event(ev, &ask_tx);
                if let Some(u) = update { let _ = tx.send(u); }
                if done { return; }
            }
            p = pulses.recv(), if pulses_open => {
                match p {
                    Ok(Pulse::TextDelta { text, .. }) => { let _ = tx.send(Update::Text(text)); }
                    Ok(Pulse::ThinkingDelta { text, .. }) => { let _ = tx.send(Update::Thinking(text)); }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // Pulses gone: keep following events only.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => pulses_open = false,
                }
            }
            c = ctrl.recv(), if ctrl_open => {
                match c {
                    Some(Ctrl::Input(i)) => { if let Err(e) = h.send(i).await { tracing::warn!(error = %e, "control rejected"); } }
                    Some(Ctrl::Answer(q, a)) => { if let Err(e) = h.answer(q, a, "code").await { tracing::debug!(error = %e, "answer not applied"); } }
                    None => ctrl_open = false,
                }
            }
        }
    }
}

fn map_event(ev: Envelope<Event>, ask_tx: &mpsc::UnboundedSender<Ctrl>) -> (Option<Update>, bool) {
    match ev.body {
        Event::AssistantReplied { message, .. } => (Some(Update::Reply(message)), false),
        Event::ToolResulted { call, result } => (Some(Update::Tool { call, result }), false),
        Event::QuestionAsked { question, .. } => (Some(Update::Ask(Ask { question, tx: ask_tx.clone() })), false),
        Event::TurnEnded { outcome } => (Some(Update::Done(outcome)), true),
        _ if ev.audience.user_visible() => (Some(Update::Event(Box::new(ev))), false),
        _ => (None, false),
    }
}

impl Stream for Run {
    type Item = Update;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<Option<Update>> {
        self.start();
        let this = &mut *self;
        let St::Running(rx) = &mut this.st else { return Poll::Ready(None) };
        match rx.poll_recv(cx) {
            Poll::Ready(Some(Update::Done(o))) => {
                this.outcome = Some(o.clone());
                this.st = St::Done;
                Poll::Ready(Some(Update::Done(o)))
            }
            Poll::Ready(None) => {
                this.st = St::Done;
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !self.detached && matches!(self.st, St::Running(_)) {
            let _ = self.ctrl_tx.send(Ctrl::Input(Input::Control(Control::SoftInterrupt)));
        }
    }
}

pub(crate) fn outcome_to_result(session: &SessionId, o: TurnOutcome) -> Result<String, Error> {
    match o {
        TurnOutcome::Done { text } => Ok(text),
        TurnOutcome::Interrupted => Err(Error::Interrupted),
        TurnOutcome::Suspended { question } => {
            Err(Error::Suspended { session: session.clone(), question: question.map(Box::new) })
        }
        TurnOutcome::Failed { error } => Err(Error::Failed(error)),
        TurnOutcome::BudgetExhausted { what } => Err(Error::Budget(what)),
    }
}

impl IntoFuture for Run {
    type Output = Result<String, Error>;
    type IntoFuture = Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>;

    /// Awaiting drains the remaining updates. Asks nobody handles are denied
    /// (the run is awaited without an approver attached).
    fn into_future(mut self) -> Self::IntoFuture {
        Box::pin(async move {
            while let Some(u) = self.next().await {
                if let Update::Ask(a) = u {
                    a.deny("no approver attached (the run was awaited without handling Update::Ask)");
                }
            }
            let o = self.outcome.clone().ok_or(Error::Ended)?;
            outcome_to_result(&self.session, o)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse_json;

    #[test]
    fn json_parsing() {
        let v: serde_json::Value = parse_json("```json\n{\"a\":1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        let v: Vec<u8> = parse_json("Here: [1,2] done").unwrap();
        assert_eq!(v, vec![1, 2]);
    }
}
