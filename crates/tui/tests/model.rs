use agent_proto::*;
use agent_tui::model::{Action, Entry, InputMode, Model, Phase};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn sid() -> SessionId {
    SessionId::new("s1")
}

fn model() -> Model {
    Model::new(sid(), "t")
}

fn env(seq: Seq, body: Event) -> ServerMessage {
    env_trust(seq, body, Trust::Internal)
}

fn env_trust(seq: Seq, body: Event, trust: Trust) -> ServerMessage {
    ServerMessage::Event {
        session: sid(),
        event: Box::new(Envelope {
            id: EventId::new(format!("e{seq}")),
            parent: None,
            seq,
            at: Default::default(),
            origin: Origin::Kernel,
            trust,
            audience: Audience::Both,
            schema: EVENT_SCHEMA,
            body,
            rendered: None,
        }),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_text(m: &mut Model, text: &str) {
    for c in text.chars() {
        assert!(m.on_key(key(KeyCode::Char(c))).is_empty());
    }
}

fn command(a: &Action) -> (&str, &Command) {
    match a {
        Action::Send(ClientMessage::Command { key, command, session }) => {
            assert_eq!(*session, sid());
            (key.as_str(), command)
        }
        other => panic!("{other:?}"),
    }
}

fn question(id: &str, prompt: &str) -> Question {
    serde_json::from_value(serde_json::json!({"id": id, "prompt": prompt})).unwrap()
}

fn reply(text: &str, calls: Vec<ToolCall>, usage: (u32, u32)) -> AssistantMessage {
    let mut content = vec![ContentBlock::Text { text: text.into() }];
    content.extend(calls.into_iter().map(ContentBlock::ToolUse));
    serde_json::from_value(serde_json::json!({
        "content": content,
        "stop": "end_turn",
        "usage": {"input_tokens": usage.0, "output_tokens": usage.1},
    }))
    .unwrap()
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: CallId::new(id),
        name: name.into(),
        input: serde_json::json!({"path": "src/lib.rs"}),
        access: vec![],
        class: EffectClass::Pure,
    }
}

fn result(id: &str, text: &str, is_error: bool) -> ToolResult {
    ToolResult::text(CallId::new(id), text, is_error)
}

#[test]
fn handshake_resumes_from_the_cursor() {
    let mut m = model();
    assert_eq!(
        m.handshake()[1],
        ClientMessage::Subscribe { session: sid(), from_seq: 0, pulses: true }
    );
    m.apply(env(0, Event::Paused));
    m.apply(env(1, Event::Resumed));
    // Replayed duplicates are ignored.
    m.apply(env(0, Event::UserMessage { text: "dup".into(), attachments: vec![] }));
    assert!(m.transcript.is_empty());
    assert!(matches!(&m.handshake()[1], ClientMessage::Subscribe { from_seq: 2, .. }));
    assert!(matches!(&m.handshake()[0], ClientMessage::Hello { .. }));
}

#[test]
fn streaming_text_then_final_reply_and_tool_lines() {
    let mut m = model();
    m.apply(ServerMessage::Welcome { version: PROTOCOL_VERSION });
    assert!(m.connected);
    m.apply(env(0, Event::TurnStarted { cause: TurnCause::User }));
    m.apply(env(1, Event::UserMessage { text: "fix it".into(), attachments: vec![] }));
    assert!(m.busy);
    assert_eq!(m.phase, Phase::Sampling);
    let eff = EffectId { epoch: 0, n: 7 };
    for t in ["Look", "ing"] {
        m.apply(ServerMessage::Pulse { session: sid(), pulse: Pulse::TextDelta { effect: eff, text: t.into() } });
    }
    assert_eq!(m.transcript[1], Entry::Assistant { text: "Looking".into(), effect: Some(eff), live: true });

    m.apply(env(2, Event::AssistantReplied { message: reply("Looking now.", vec![call("c1", "read")], (100, 20)), effect: eff }));
    assert_eq!(m.transcript[1], Entry::Assistant { text: "Looking now.".into(), effect: Some(eff), live: false });
    // A late delta does not touch the final text.
    m.apply(ServerMessage::Pulse { session: sid(), pulse: Pulse::TextDelta { effect: eff, text: "x".into() } });
    assert!(matches!(&m.transcript[1], Entry::Assistant { text, .. } if text == "Looking now."));
    assert!(matches!(&m.transcript[2], Entry::Tool { name, result: None, .. } if name == "read"));
    assert_eq!((m.input_tokens, m.output_tokens), (100, 20));

    m.apply(ServerMessage::Pulse { session: sid(), pulse: Pulse::ToolProgress { call: "c1".into(), message: "reading".into() } });
    assert!(matches!(&m.transcript[2], Entry::Tool { progress: Some(p), .. } if p == "reading"));

    m.apply(env_trust(
        3,
        Event::ToolResulted { call: call("c1", "read"), result: result("c1", "line one\nline two\nline three", false) },
        Trust::Untrusted { source: "web".into() },
    ));
    match &m.transcript[2] {
        Entry::Tool { result: Some(r), progress: None, .. } => {
            assert_eq!(r.text, "line one (+2 lines)");
            assert!(!r.is_error);
        }
        other => panic!("{other:?}"),
    }
    assert!(m.tainted);
    m.apply(env(4, Event::TurnEnded { outcome: TurnOutcome::Done { text: "ok".into() } }));
    assert!(!m.busy);
    assert_eq!(m.phase, Phase::Idle);
    m.apply(env(5, Event::TaintCleared));
    assert!(!m.tainted);
}

#[test]
fn enter_submits_when_idle_and_steers_when_busy_alt_enter_queues() {
    let mut m = model();
    assert!(m.on_key(key(KeyCode::Enter)).is_empty(), "empty input sends nothing");
    type_text(&mut m, "hello");
    let a = m.on_key(key(KeyCode::Enter));
    let (k1, c) = command(&a[0]);
    assert_eq!(*c, Command::Signal(Signal::Submit { text: "hello".into(), attachments: vec![] }));
    assert!(m.input.is_empty());

    m.apply(env(0, Event::TurnStarted { cause: TurnCause::User }));
    type_text(&mut m, "use tabs");
    let a = m.on_key(key(KeyCode::Enter));
    let (k2, c) = command(&a[0]);
    assert_eq!(*c, Command::Signal(Signal::Steer { text: "use tabs".into() }));
    assert_ne!(k1, k2, "fresh idempotency keys");

    type_text(&mut m, "next");
    let a = m.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(*command(&a[0]).1, Command::Signal(Signal::Queue { text: "next".into() }));
    type_text(&mut m, "later");
    let a = m.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    assert_eq!(*command(&a[0]).1, Command::Signal(Signal::Queue { text: "later".into() }));

    // Backspace and Ctrl+U edit.
    type_text(&mut m, "abc");
    m.on_key(key(KeyCode::Backspace));
    assert_eq!(m.input, "ab");
    m.on_key(ctrl('u'));
    assert_eq!(m.input, "");
}

#[test]
fn interrupts_esc_soft_ctrl_c_twice_hard_or_quit() {
    let mut m = model();
    // Idle: Esc does nothing; Ctrl+C twice quits.
    assert!(m.on_key(key(KeyCode::Esc)).is_empty());
    assert!(m.on_key(ctrl('c')).is_empty());
    assert!(m.ctrl_c_armed);
    assert_eq!(m.on_key(ctrl('c')), vec![Action::Quit]);

    m.apply(env(0, Event::TurnStarted { cause: TurnCause::User }));
    let a = m.on_key(key(KeyCode::Esc));
    assert_eq!(*command(&a[0]).1, Command::Control(Control::SoftInterrupt));
    assert!(m.on_key(ctrl('c')).is_empty());
    // Another key disarms.
    type_text(&mut m, "x");
    assert!(!m.ctrl_c_armed);
    assert!(m.on_key(ctrl('c')).is_empty());
    let a = m.on_key(ctrl('c'));
    assert_eq!(*command(&a[0]).1, Command::Control(Control::HardInterrupt));
    assert!(m.hint.is_none());
}

#[test]
fn approvals_allow_always_and_deny_with_reason() {
    let mut m = model();
    m.apply(env(0, Event::QuestionAsked { question: question("q1", "run rm?"), subject: GateRef::Session }));
    m.apply(env(1, Event::QuestionAsked { question: question("q2", "fetch url?"), subject: GateRef::Session }));
    assert_eq!(m.phase, Phase::Gated);
    assert_eq!(m.pending.len(), 2);

    // y answers the oldest question.
    let a = m.on_key(key(KeyCode::Char('y')));
    match &a[0] {
        Action::Send(ClientMessage::Answer { question, answer, .. }) => {
            assert_eq!(question.as_str(), "q1");
            assert_eq!(*answer, Answer::Allow { remember: false });
        }
        other => panic!("{other:?}"),
    }
    // The server closes it (we won, or someone else did).
    m.apply(ServerMessage::QuestionClosed { session: sid(), question: QuestionId::new("q1") });
    assert_eq!(m.pending.len(), 1);

    // a = allow always.
    let a = m.on_key(key(KeyCode::Char('a')));
    assert!(matches!(&a[0], Action::Send(ClientMessage::Answer { answer: Answer::Allow { remember: true }, .. })));

    // n opens the reason prompt; typing goes to the reason, Enter denies.
    assert!(m.on_key(key(KeyCode::Char('n'))).is_empty());
    assert_eq!(m.mode, InputMode::DenyReason(QuestionId::new("q2")));
    type_text(&mut m, "not now");
    let a = m.on_key(key(KeyCode::Enter));
    match &a[0] {
        Action::Send(ClientMessage::Answer { question, answer, .. }) => {
            assert_eq!(question.as_str(), "q2");
            assert_eq!(*answer, Answer::Deny { reason: Some("not now".into()) });
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(m.mode, InputMode::Message);

    // Esc cancels a deny; an answer from elsewhere closes the prompt.
    m.on_key(key(KeyCode::Char('n')));
    m.on_key(key(KeyCode::Esc));
    assert_eq!(m.mode, InputMode::Message);
    m.on_key(key(KeyCode::Char('n')));
    m.apply(env(
        2,
        Event::QuestionAnswered {
            question: QuestionId::new("q2"),
            answer: Answer::Allow { remember: false },
            responder: Responder::Human("bob".into()),
        },
    ));
    assert!(m.pending.is_empty());
    assert_eq!(m.mode, InputMode::Message);
    assert!(matches!(m.transcript.last(), Some(Entry::Notice(n)) if n == "allowed by bob"));

    // With text in the input, y is just a letter.
    m.apply(env(3, Event::QuestionAsked { question: question("q3", "?"), subject: GateRef::Session }));
    type_text(&mut m, "ok");
    type_text(&mut m, "y");
    assert_eq!(m.input, "oky");
}

#[test]
fn acks_errors_and_scrolling() {
    let mut m = model();
    m.apply(ServerMessage::Ack { key: "k".into(), accepted: false, error: Some("rejected: busy".into()) });
    assert_eq!(m.transcript.last(), Some(&Entry::Error("rejected: busy".into())));
    m.apply(ServerMessage::Ack { key: "k".into(), accepted: false, error: Some(agent_server::ALREADY_ANSWERED.into()) });
    assert!(matches!(m.transcript.last(), Some(Entry::Notice(_))));
    m.apply(ServerMessage::Welcome { version: 0 });
    m.apply(ServerMessage::Error { message: format!("{}: resubscribe", agent_server::SLOW_CONSUMER) });
    assert!(!m.connected);

    m.page = 5;
    m.on_key(key(KeyCode::PageUp));
    m.on_key(key(KeyCode::PageUp));
    assert_eq!(m.scroll, 10);
    m.on_key(key(KeyCode::PageDown));
    assert_eq!(m.scroll, 5);
    assert_eq!(m.on_key(ctrl('d')), vec![Action::Quit]);
}

// ---------------------------------------------------------------- rendering

fn screen(m: &Model, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| agent_tui::ui::render(f, m)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        for x in 0..w {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn render_smoke() {
    let mut m = model();
    m.apply(ServerMessage::Welcome { version: PROTOCOL_VERSION });
    m.apply(env(0, Event::TurnStarted { cause: TurnCause::User }));
    m.apply(env(1, Event::UserMessage { text: "fix the tests".into(), attachments: vec![] }));
    m.apply(env(2, Event::AssistantReplied { message: reply("On it.", vec![call("c1", "bash")], (10, 5)), effect: EffectId { epoch: 0, n: 1 } }));
    m.apply(env(3, Event::ToolResulted { call: call("c1", "bash"), result: result("c1", "3 passed", false) }));
    m.apply(env(4, Event::QuestionAsked { question: question("q1", "Allow `git push`?"), subject: GateRef::Session }));
    type_text(&mut m, "draft");

    let s = screen(&m, 80, 20);
    assert!(s.contains("> fix the tests"), "{s}");
    assert!(s.contains("On it."), "{s}");
    assert!(s.contains("+ bash("), "{s}");
    assert!(s.contains("-> 3 passed"), "{s}");
    assert!(s.contains("Allow `git push`?"), "{s}");
    assert!(s.contains("y = allow"), "{s}");
    assert!(s.contains("draft"), "{s}");
    assert!(s.contains("s1 | awaiting approval"), "{s}");
    assert!(s.contains("tok 10/5"), "{s}");

    // Tiny terminals and long transcripts do not panic; the bottom is shown.
    for i in 0..50 {
        m.apply(env(5 + i, Event::UserMessage { text: format!("message number {i} ").repeat(5), attachments: vec![] }));
    }
    let s = screen(&m, 30, 12);
    assert!(s.contains("number 49"), "{s}");
    m.scroll = 1000;
    let _ = screen(&m, 30, 12);
    let _ = screen(&m, 5, 3);
}

#[test]
fn visible_window_clamps() {
    use agent_tui::ui::visible_window;
    assert_eq!(visible_window(100, 10, 0), 90..100);
    assert_eq!(visible_window(100, 10, 5), 85..95);
    assert_eq!(visible_window(100, 10, 500), 0..10);
    assert_eq!(visible_window(3, 10, 2), 0..3);
}
