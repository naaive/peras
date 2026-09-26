//! The view-model: a pure state reducer over [`ServerMessage`]s plus key
//! handling that yields [`ClientMessage`]s. No IO, no terminal; rendering
//! lives in [`crate::ui`].

use agent_proto::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Client name sent in `Hello`.
pub const CLIENT_NAME: &str = "agent-tui";

/// Longest one-line tool result summary (characters).
const SUMMARY_CHARS: usize = 100;

/// What the kernel is doing, as far as the event stream tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// A turn is open, waiting on the model.
    Sampling,
    /// Tools are running.
    Acting,
    /// Waiting on a gate / approval.
    Gated,
    Suspended,
    Paused,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Sampling => "sampling",
            Phase::Acting => "acting",
            Phase::Gated => "awaiting approval",
            Phase::Suspended => "suspended",
            Phase::Paused => "paused",
        }
    }
}

/// One transcript line group.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// Assistant text; `live` while still streaming from pulses.
    Assistant { text: String, effect: Option<EffectId>, live: bool },
    /// A tool call; `result` is a one-line summary once it finished.
    Tool { call: CallId, name: String, args: String, progress: Option<String>, result: Option<ToolSummary> },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSummary {
    pub text: String,
    pub is_error: bool,
}

/// What the input box is collecting.
#[derive(Debug, Clone, PartialEq)]
pub enum InputMode {
    /// A message: Enter submits (idle) or steers (busy).
    Message,
    /// A deny reason for the given question: Enter sends, Esc cancels.
    DenyReason(QuestionId),
}

/// What the app loop must do after a key.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // short-lived, one per key
pub enum Action {
    Send(ClientMessage),
    Quit,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub session: SessionId,
    /// Prefix of idempotency keys (unique per client instance).
    client_id: String,
    next_key: u64,

    pub connected: bool,
    /// Resume cursor: the next event seq expected.
    pub next_seq: Seq,
    pub phase: Phase,
    /// A turn is open (Enter steers instead of submitting).
    pub busy: bool,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tainted: bool,
    /// Thinking deltas are streaming (cleared by the reply).
    pub thinking: bool,

    pub transcript: Vec<Entry>,
    /// Questions waiting on an answer, oldest first.
    pub pending: Vec<Question>,

    pub input: String,
    pub mode: InputMode,
    /// Lines scrolled up from the bottom of the transcript (0 = follow).
    pub scroll: usize,
    /// Transcript viewport height, for page scrolling (set by the app).
    pub page: usize,
    /// First Ctrl+C seen; a second one interrupts hard (or quits when idle).
    pub ctrl_c_armed: bool,
    /// Transient hint shown in the status line.
    pub hint: Option<String>,
}

impl Model {
    pub fn new(session: SessionId, client_id: impl Into<String>) -> Model {
        Model {
            session,
            client_id: client_id.into(),
            next_key: 0,
            connected: false,
            next_seq: 0,
            phase: Phase::Idle,
            busy: false,
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            tainted: false,
            thinking: false,
            transcript: vec![],
            pending: vec![],
            input: String::new(),
            mode: InputMode::Message,
            scroll: 0,
            page: 10,
            ctrl_c_armed: false,
            hint: None,
        }
    }

    /// Messages to send on every (re)connect: negotiate, then subscribe from
    /// the resume cursor (replay what was missed, then live).
    pub fn handshake(&self) -> Vec<ClientMessage> {
        vec![
            ClientMessage::Hello { versions: vec![PROTOCOL_VERSION], client: CLIENT_NAME.into() },
            ClientMessage::Subscribe { session: self.session.clone(), from_seq: self.next_seq, pulses: true },
        ]
    }

    /// The transport closed.
    pub fn disconnected(&mut self) {
        if self.connected {
            self.connected = false;
            self.transcript.push(Entry::Notice("disconnected; reconnecting".into()));
        }
    }

    fn key(&mut self) -> String {
        self.next_key += 1;
        format!("{}-{}", self.client_id, self.next_key)
    }

    // ------------------------------------------------------------ reducer

    /// Fold one server message into the view.
    pub fn apply(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Welcome { .. } => self.connected = true,
            ServerMessage::Event { session, event } if session == self.session => self.event(*event),
            ServerMessage::Pulse { session, pulse } if session == self.session => self.pulse(pulse),
            ServerMessage::Event { .. } | ServerMessage::Pulse { .. } => {}
            ServerMessage::Ack { accepted: true, .. } | ServerMessage::Pong => {}
            ServerMessage::Ack { accepted: false, error, .. } => {
                let error = error.unwrap_or_else(|| "rejected".into());
                if error == agent_server::ALREADY_ANSWERED {
                    self.transcript.push(Entry::Notice("already answered elsewhere".into()));
                } else {
                    self.transcript.push(Entry::Error(error));
                }
            }
            ServerMessage::QuestionClosed { session, question } if session == self.session => {
                self.close_question(&question);
            }
            ServerMessage::QuestionClosed { .. } => {}
            ServerMessage::Error { message } => {
                if message.starts_with(agent_server::SLOW_CONSUMER) {
                    // The server drops us; the app reconnects from `next_seq`.
                    self.connected = false;
                }
                self.transcript.push(Entry::Error(message));
            }
        }
    }

    fn close_question(&mut self, id: &QuestionId) {
        self.pending.retain(|q| q.id != *id);
        if self.mode == InputMode::DenyReason(id.clone()) {
            self.mode = InputMode::Message;
            self.input.clear();
        }
        if self.pending.is_empty() && self.phase == Phase::Gated {
            self.phase = if self.busy { Phase::Acting } else { Phase::Idle };
        }
    }

    fn event(&mut self, ev: Envelope<Event>) {
        // Replays after a reconnect may overlap what was already seen.
        if ev.seq < self.next_seq {
            return;
        }
        self.next_seq = ev.seq + 1;
        if ev.trust.is_untrusted() {
            self.tainted = true;
        }
        match ev.body {
            Event::SessionStarted { config, .. } => self.model = config.caps.model.to_string(),
            Event::SequenceOpened { head } => self.model = head.model.to_string(),
            Event::ModelSwitched { from, to, reason } => {
                self.model = to.to_string();
                self.transcript.push(Entry::Notice(format!("model switched {from} -> {to} ({reason})")));
            }
            Event::TurnStarted { .. } => {
                self.busy = true;
                self.phase = Phase::Sampling;
            }
            Event::UserMessage { text, .. } => self.transcript.push(Entry::User(text)),
            Event::Injected { source, text } => self.transcript.push(Entry::Notice(format!("[{source}] {text}"))),
            Event::EffectIssued { effect, .. } => match effect {
                Effect::Sample(_) | Effect::SampleRef(_) | Effect::Compact(_) | Effect::CompactRef(_) => self.phase = Phase::Sampling,
                Effect::Execute(_) => self.phase = Phase::Acting,
                Effect::Gate(_) if !self.pending.is_empty() => self.phase = Phase::Gated,
                _ => {}
            },
            Event::AssistantReplied { message, effect } => self.replied(message, effect),
            Event::ToolResulted { call, result } => self.tool_resulted(call, result),
            Event::TurnEnded { outcome } => {
                self.busy = false;
                self.finish_live();
                self.phase = Phase::Idle;
                match outcome {
                    TurnOutcome::Done { .. } => {}
                    TurnOutcome::Interrupted => self.transcript.push(Entry::Notice("interrupted".into())),
                    TurnOutcome::Suspended { .. } => {
                        self.phase = Phase::Suspended;
                        self.transcript.push(Entry::Notice("suspended: waiting on an approval".into()));
                    }
                    TurnOutcome::Failed { error } => self.transcript.push(Entry::Error(format!("turn failed: {error}"))),
                    TurnOutcome::BudgetExhausted { what } => {
                        self.transcript.push(Entry::Error(format!("budget exhausted: {what}")))
                    }
                }
            }
            Event::QuestionAsked { question, .. } => {
                self.transcript.push(Entry::Notice(format!("approval requested: {}", question.prompt)));
                if !self.pending.iter().any(|q| q.id == question.id) {
                    self.pending.push(question);
                }
                self.phase = Phase::Gated;
            }
            Event::QuestionAnswered { question, answer, responder } => {
                let what = match answer {
                    Answer::Allow { remember: true } => "allowed (always)".to_string(),
                    Answer::Allow { .. } => "allowed".to_string(),
                    Answer::AllowWith(_) => "allowed with changes".to_string(),
                    Answer::Deny { reason: Some(r) } => format!("denied: {r}"),
                    Answer::Deny { reason: None } => "denied".to_string(),
                };
                self.transcript.push(Entry::Notice(format!("{what} by {}", responder_name(&responder))));
                self.close_question(&question);
            }
            Event::Interrupted { hard, .. } => {
                self.finish_live();
                if hard {
                    self.pending.clear();
                    self.mode = InputMode::Message;
                }
                self.transcript.push(Entry::Notice(if hard { "hard interrupt" } else { "soft interrupt requested" }.into()));
            }
            Event::Paused => self.phase = Phase::Paused,
            Event::Resumed => self.phase = if self.busy { Phase::Sampling } else { Phase::Idle },
            Event::Suspended { reason } => {
                self.phase = Phase::Suspended;
                self.transcript.push(Entry::Notice(format!("suspended: {reason}")));
            }
            Event::TaintCleared => self.tainted = false,
            Event::RewindCompleted { .. } => self.transcript.push(Entry::Notice("rewind completed".into())),
            Event::SubagentStarted { child, .. } => self.transcript.push(Entry::Notice(format!("sub-agent {child} started"))),
            Event::SubagentFinished { child, .. } => {
                self.transcript.push(Entry::Notice(format!("sub-agent {child} finished")))
            }
            _ => {}
        }
    }

    fn replied(&mut self, message: AssistantMessage, effect: EffectId) {
        self.thinking = false;
        self.input_tokens += u64::from(message.usage.input_tokens);
        self.output_tokens += u64::from(message.usage.output_tokens);
        let text = message.text();
        let live = self
            .transcript
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Assistant { effect: Some(x), .. } if *x == effect));
        match live {
            Some(Entry::Assistant { text: t, live, .. }) => {
                *t = text;
                *live = false;
            }
            _ if !text.is_empty() => {
                self.transcript.push(Entry::Assistant { text, effect: Some(effect), live: false });
            }
            _ => {}
        }
        for call in message.tool_calls() {
            self.transcript.push(Entry::Tool {
                call: call.id.clone(),
                name: call.name.clone(),
                args: one_line(&call.input.to_string(), SUMMARY_CHARS),
                progress: None,
                result: None,
            });
        }
    }

    fn tool_resulted(&mut self, call: ToolCall, result: ToolResult) {
        let summary = ToolSummary { text: summarize(&result.content), is_error: result.is_error };
        let slot = self.transcript.iter_mut().rev().find(|e| matches!(e, Entry::Tool { call: c, .. } if *c == call.id));
        match slot {
            Some(Entry::Tool { result, progress, .. }) => {
                *result = Some(summary);
                *progress = None;
            }
            _ => self.transcript.push(Entry::Tool {
                call: call.id.clone(),
                name: call.name.clone(),
                args: one_line(&call.input.to_string(), SUMMARY_CHARS),
                progress: None,
                result: Some(summary),
            }),
        }
    }

    /// Any streaming text is final now (interrupts, turn end).
    fn finish_live(&mut self) {
        self.thinking = false;
        for e in &mut self.transcript {
            if let Entry::Assistant { live, .. } = e {
                *live = false;
            }
        }
    }

    fn pulse(&mut self, pulse: Pulse) {
        match pulse {
            Pulse::TextDelta { effect, text } => {
                let live = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Assistant { effect: Some(x), .. } if *x == effect));
                match live {
                    Some(Entry::Assistant { text: t, live: true, .. }) => t.push_str(&text),
                    // Already final (the event won the race): ignore late deltas.
                    Some(_) => {}
                    None => self.transcript.push(Entry::Assistant { text, effect: Some(effect), live: true }),
                }
            }
            Pulse::ThinkingDelta { .. } => self.thinking = true,
            Pulse::ToolProgress { call, message } => {
                let slot = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Tool { call: c, result: None, .. } if c.as_str() == call));
                if let Some(Entry::Tool { progress, .. }) = slot {
                    *progress = Some(one_line(&message, SUMMARY_CHARS));
                }
            }
            Pulse::Heartbeat { .. } => {}
        }
    }

    // ------------------------------------------------------------ keys

    /// Handle one key; returns what the app must send (or do).
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return vec![];
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && key.code == KeyCode::Char('c') {
            if self.ctrl_c_armed {
                self.ctrl_c_armed = false;
                self.hint = None;
                return if self.busy { vec![self.command(Command::Control(Control::HardInterrupt))] } else { vec![Action::Quit] };
            }
            self.ctrl_c_armed = true;
            self.hint = Some(if self.busy { "press Ctrl+C again to hard interrupt" } else { "press Ctrl+C again to quit" }.into());
            return vec![];
        }
        if self.ctrl_c_armed {
            self.ctrl_c_armed = false;
            self.hint = None;
        }

        match key.code {
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(self.page.max(1));
                return vec![];
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.page.max(1));
                return vec![];
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => return vec![Action::Quit],
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                return vec![];
            }
            _ => {}
        }

        if let InputMode::DenyReason(q) = self.mode.clone() {
            return match key.code {
                KeyCode::Esc => {
                    self.mode = InputMode::Message;
                    self.input.clear();
                    vec![]
                }
                KeyCode::Enter => {
                    let reason = std::mem::take(&mut self.input).trim().to_string();
                    self.mode = InputMode::Message;
                    vec![self.answer(q, Answer::Deny { reason: (!reason.is_empty()).then_some(reason) })]
                }
                _ => {
                    self.edit(key.code, ctrl || alt);
                    vec![]
                }
            };
        }

        // Approval keys act on the oldest pending question while the input is empty.
        if self.input.is_empty() && !ctrl && !alt {
            if let Some(q) = self.pending.first().map(|q| q.id.clone()) {
                match key.code {
                    KeyCode::Char('y') => return vec![self.answer(q, Answer::Allow { remember: false })],
                    KeyCode::Char('a') => return vec![self.answer(q, Answer::Allow { remember: true })],
                    KeyCode::Char('n') => {
                        self.mode = InputMode::DenyReason(q);
                        return vec![];
                    }
                    _ => {}
                }
            }
        }

        match key.code {
            KeyCode::Esc => {
                if self.busy {
                    vec![self.command(Command::Control(Control::SoftInterrupt))]
                } else {
                    vec![]
                }
            }
            KeyCode::Enter if ctrl || alt => match self.take_input() {
                Some(text) => vec![self.command(Command::Signal(Signal::Queue { text }))],
                None => vec![],
            },
            KeyCode::Enter => match self.take_input() {
                Some(text) if self.busy => vec![self.command(Command::Signal(Signal::Steer { text }))],
                Some(text) => {
                    self.scroll = 0;
                    vec![self.command(Command::Signal(Signal::Submit { text, attachments: vec![] }))]
                }
                None => vec![],
            },
            code => {
                self.edit(code, ctrl || alt);
                vec![]
            }
        }
    }

    fn edit(&mut self, code: KeyCode, modified: bool) {
        match code {
            KeyCode::Char(c) if !modified => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            _ => {}
        }
    }

    fn take_input(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn command(&mut self, command: Command) -> Action {
        Action::Send(ClientMessage::Command { session: self.session.clone(), key: self.key(), command })
    }

    fn answer(&mut self, question: QuestionId, answer: Answer) -> Action {
        Action::Send(ClientMessage::Answer { session: self.session.clone(), key: self.key(), question, answer })
    }
}

fn responder_name(r: &Responder) -> String {
    match r {
        Responder::Human(n) if !n.is_empty() => n.clone(),
        Responder::Human(_) => "a user".into(),
        Responder::Code => "code".into(),
        Responder::AutoRule(n) => format!("rule {n}"),
        Responder::Policy(n) => format!("policy {n}"),
        Responder::Hook(n) => format!("hook {n}"),
        Responder::Kernel => "kernel".into(),
        Responder::Budget => "budget".into(),
        Responder::Unattended => "unattended fallback".into(),
        Responder::DisposableEnv => "disposable environment".into(),
    }
}

/// First line of a tool result, shortened.
fn summarize(content: &[ToolContent]) -> String {
    let first = content.first().map(|c| match c {
        ToolContent::Text { text } => text.clone(),
        ToolContent::Blob { preview, .. } => preview.clone(),
        ToolContent::Image { .. } => "[image]".to_string(),
        ToolContent::Json { value } => value.to_string(),
    });
    let total: usize = content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.lines().count(),
            _ => 1,
        })
        .sum();
    let line = one_line(first.as_deref().unwrap_or(""), SUMMARY_CHARS);
    if total > 1 {
        format!("{line} (+{} lines)", total - 1)
    } else {
        line
    }
}

/// First non-empty line, at most `max` characters.
pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > max {
        let mut out: String = line.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    } else {
        line.to_string()
    }
}
