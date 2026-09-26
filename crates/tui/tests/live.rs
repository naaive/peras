//! The view-model driven against a real in-process server.

use agent_kernel::{Decider, Decision, Rejection};
use agent_proto::*;
use agent_runtime::Runtime;
use agent_server::{OpenWith, Server};
use agent_tui::model::{Action, Entry, Model};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;

struct Echo;

impl Decider for Echo {
    type State = ();
    fn decide(_s: &(), _at: Timestamp, input: Input) -> Result<Decision, Rejection> {
        match input {
            Input::Signal(Signal::Submit { text, .. }) => Ok(Decision {
                events: vec![Draft {
                    parent: Parent::Head,
                    origin: Origin::User,
                    trust: Trust::User,
                    audience: Audience::Both,
                    body: Event::UserMessage { text, attachments: vec![] },
                    rendered: None,
                }],
                effects: vec![],
            }),
            other => Err(Rejection::new(format!("echo rejects {other:?}"))),
        }
    }
    fn evolve(_s: &mut (), _ev: &Envelope<Event>) {}
    fn outstanding(_s: &()) -> Vec<(EffectId, Effect)> {
        vec![]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_round_trip_through_the_server() {
    let rt: Runtime<Echo> = Runtime::builder().build();
    let srv = Server::new(
        Arc::new(rt),
        OpenWith(|_: &SessionId| Decision { events: vec![Draft::internal(Event::Paused)], effects: vec![] }),
    );
    let mut conn = agent_tui::in_process(srv.clone());
    let mut c = conn().await.unwrap();
    let mut m = Model::new(SessionId::new("live"), "t");
    for msg in m.handshake() {
        c.send(msg).await.unwrap();
    }
    for ch in "hi there".chars() {
        m.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    for a in m.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
        let Action::Send(msg) = a else { panic!() };
        c.send(msg).await.unwrap();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !m.transcript.contains(&Entry::User("hi there".into())) {
        let msg = tokio::time::timeout_at(deadline, c.recv()).await.expect("timed out").expect("closed");
        m.apply(msg);
    }
    assert!(m.connected);
    assert_eq!(m.next_seq, 2);

    // Reconnect resumes after what was seen: no duplicates.
    drop(c);
    let c2 = conn().await.unwrap();
    let mut c2 = c2;
    for msg in m.handshake() {
        c2.send(msg).await.unwrap();
    }
    c2.send(ClientMessage::Ping).await.unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), c2.recv()).await.unwrap().unwrap();
        if msg == ServerMessage::Pong {
            break;
        }
        m.apply(msg);
    }
    assert_eq!(m.transcript.iter().filter(|e| matches!(e, Entry::User(_))).count(), 1);
}
