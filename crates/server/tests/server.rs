use agent_kernel::{Decider, Decision, Rejection};
use agent_proto::*;
use agent_runtime::{Runtime, RuntimeOptions};
use agent_server::*;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------- toy decider

/// Each `Submit` appends one user message; everything else is rejected.
struct Toy;

impl Decider for Toy {
    type State = usize;

    fn decide(_s: &usize, _at: Timestamp, input: Input) -> Result<Decision, Rejection> {
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
            other => Err(Rejection::new(format!("toy rejects {other:?}"))),
        }
    }

    fn evolve(s: &mut usize, _ev: &Envelope<Event>) {
        *s += 1;
    }

    fn outstanding(_s: &usize) -> Vec<(EffectId, Effect)> {
        vec![]
    }
}

fn start() -> Decision {
    Decision { events: vec![Draft::internal(Event::Paused)], effects: vec![] }
}

fn server() -> Server<Toy> {
    // Tiny in-memory window so replays also go through the journal.
    let options = RuntimeOptions { recent_capacity: 2, ..Default::default() };
    let rt: Runtime<Toy> = Runtime::builder().options(options).build();
    Server::new(Arc::new(rt), OpenWith(|_: &SessionId| start()))
}

fn sid() -> SessionId {
    SessionId::new("s")
}

fn submit(key: &str, text: &str) -> ClientMessage {
    ClientMessage::Command {
        session: sid(),
        key: key.into(),
        command: Command::Signal(Signal::Submit { text: text.into(), attachments: vec![] }),
    }
}

fn subscribe(from_seq: Seq) -> ClientMessage {
    ClientMessage::Subscribe { session: sid(), from_seq, pulses: true }
}

async fn recv(c: &mut ChannelClient) -> ServerMessage {
    tokio::time::timeout(Duration::from_secs(5), c.recv()).await.expect("timed out").expect("connection closed")
}

async fn hello(c: &mut ChannelClient, name: &str) {
    c.send(ClientMessage::Hello { versions: vec![PROTOCOL_VERSION], client: name.into() }).await.unwrap();
    assert_eq!(recv(c).await, ServerMessage::Welcome { version: PROTOCOL_VERSION });
}

/// Read until `n` events arrived; returns them and every other message seen.
async fn events(c: &mut ChannelClient, n: usize) -> (Vec<Envelope<Event>>, Vec<ServerMessage>) {
    let (mut evs, mut other) = (vec![], vec![]);
    while evs.len() < n {
        match recv(c).await {
            ServerMessage::Event { session, event } => {
                assert_eq!(session, sid());
                evs.push(*event);
            }
            m => other.push(m),
        }
    }
    (evs, other)
}

/// Ping and collect everything up to the Pong.
async fn fence(c: &mut ChannelClient) -> Vec<ServerMessage> {
    c.send(ClientMessage::Ping).await.unwrap();
    let mut out = vec![];
    loop {
        match recv(c).await {
            ServerMessage::Pong => return out,
            m => out.push(m),
        }
    }
}

fn seqs(evs: &[Envelope<Event>]) -> Vec<Seq> {
    evs.iter().map(|e| e.seq).collect()
}

fn ack(key: &str) -> ServerMessage {
    ServerMessage::Ack { key: key.into(), accepted: true, error: None }
}

// ---------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_see_the_same_ordered_stream() {
    let srv = server();
    let mut a = srv.connect();
    let mut b = srv.connect();
    hello(&mut a, "alice").await;
    hello(&mut b, "bob").await;
    a.send(subscribe(0)).await.unwrap();
    b.send(subscribe(0)).await.unwrap();
    let (ea, _) = events(&mut a, 1).await;
    let (eb, _) = events(&mut b, 1).await;
    assert_eq!(ea, eb);

    for i in 0..5 {
        let (c, who) = if i % 2 == 0 { (&a, "a") } else { (&b, "b") };
        c.send(submit(&format!("{who}{i}"), &format!("m{i}"))).await.unwrap();
    }
    let (ea, oa) = events(&mut a, 5).await;
    let (eb, ob) = events(&mut b, 5).await;
    assert_eq!(seqs(&ea), vec![1, 2, 3, 4, 5]);
    assert_eq!(ea, eb, "identical events in identical order");
    let texts: Vec<_> = ea
        .iter()
        .map(|e| match &e.body {
            Event::UserMessage { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    // Cross-client order is the runtime's arrival order; per-client order holds.
    let pos = |t: &str| texts.iter().position(|x| x == t).unwrap();
    assert!(pos("m0") < pos("m2") && pos("m2") < pos("m4"), "{texts:?}");
    assert!(pos("m1") < pos("m3"), "{texts:?}");

    // Each client got exactly its own acks.
    let oa = [oa, fence(&mut a).await].concat();
    let ob = [ob, fence(&mut b).await].concat();
    assert_eq!(oa, vec![ack("a0"), ack("a2"), ack("a4")]);
    assert_eq!(ob, vec![ack("b1"), ack("b3")]);

    // A retried key gets the original ack and applies nothing.
    let h = srv.session(&sid()).await.unwrap();
    let before = h.next_seq();
    b.send(submit("a0", "again")).await.unwrap();
    assert_eq!(fence(&mut b).await, vec![ack("a0")]);
    assert_eq!(h.next_seq(), before);
    assert!(fence(&mut a).await.is_empty(), "no event for the duplicate");

    // A rejected command acks false with the reason.
    a.send(ClientMessage::Command { session: sid(), key: "p".into(), command: Command::Control(Control::Pause) })
        .await
        .unwrap();
    match &fence(&mut a).await[..] {
        [ServerMessage::Ack { key, accepted: false, error: Some(e) }] => {
            assert_eq!(key, "p");
            assert!(e.contains("rejected"), "{e}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_replays_missed_events_then_goes_live() {
    let srv = server();
    let mut a = srv.connect();
    hello(&mut a, "a").await;
    a.send(subscribe(0)).await.unwrap();
    for i in 0..3 {
        a.send(submit(&format!("k{i}"), "x")).await.unwrap();
    }
    let (first, _) = events(&mut a, 4).await;
    assert_eq!(seqs(&first), vec![0, 1, 2, 3]);
    let cursor = first.last().unwrap().seq + 1;
    drop(a);

    // Events happen while disconnected.
    let mut other = srv.connect();
    hello(&mut other, "other").await;
    for i in 3..8 {
        other.send(submit(&format!("k{i}"), &format!("missed{i}"))).await.unwrap();
    }
    fence(&mut other).await;
    let h = srv.session(&sid()).await.unwrap();
    assert_eq!(h.next_seq(), 9);

    // Reconnect from the cursor: replay without gaps or duplicates...
    let mut a = srv.connect();
    hello(&mut a, "a").await;
    a.send(subscribe(cursor)).await.unwrap();
    let (replayed, _) = events(&mut a, 5).await;
    assert_eq!(seqs(&replayed), vec![4, 5, 6, 7, 8]);
    // ...then live, while submitting concurrently with the replay of another client.
    let mut late = srv.connect();
    hello(&mut late, "late").await;
    late.send(subscribe(0)).await.unwrap();
    for i in 8..12 {
        other.send(submit(&format!("k{i}"), "live")).await.unwrap();
    }
    let (live, _) = events(&mut a, 4).await;
    assert_eq!(seqs(&live), vec![9, 10, 11, 12]);
    let (all, _) = events(&mut late, 13).await;
    assert_eq!(seqs(&all), (0..13).collect::<Vec<_>>());
    assert_eq!([first, replayed, live].concat(), all);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_answers_first_wins_others_closed() {
    for round in 0..20 {
        let srv = server();
        let mut a = srv.connect();
        let mut b = srv.connect();
        let mut watcher = srv.connect();
        hello(&mut a, "alice").await;
        hello(&mut b, "bob").await;
        hello(&mut watcher, "watcher").await;
        for c in [&mut a, &mut b, &mut watcher] {
            c.send(subscribe(0)).await.unwrap();
            events(c, 1).await;
        }
        let h = srv.session(&sid()).await.unwrap();
        let q: Question = serde_json::from_value(serde_json::json!({"id": "q1", "prompt": "ok?"})).unwrap();
        h.asks().open(&q);

        let answer = |key: &str, answer: Answer| ClientMessage::Answer {
            session: sid(),
            key: key.into(),
            question: q.id.clone(),
            answer,
        };
        let (sa, sb) = (a.sender(), b.sender());
        let (ra, rb) = tokio::join!(
            sa.send(answer("ka", Answer::Allow { remember: false })),
            sb.send(answer("kb", Answer::Deny { reason: None })),
        );
        ra.unwrap();
        rb.unwrap();
        let ma = fence(&mut a).await;
        let mb = fence(&mut b).await;
        let mw = fence(&mut watcher).await;
        let closed = ServerMessage::QuestionClosed { session: sid(), question: q.id.clone() };
        let lost = |key: &str| ServerMessage::Ack {
            key: key.into(),
            accepted: false,
            error: Some(ALREADY_ANSWERED.into()),
        };
        let a_won = ma.contains(&ack("ka"));
        let b_won = mb.contains(&ack("kb"));
        assert!(a_won ^ b_won, "round {round}: exactly one winner: {ma:?} {mb:?}");
        if a_won {
            assert_eq!(ma, vec![ack("ka")]);
            assert_eq!(mb, vec![closed.clone(), lost("kb")]);
        } else {
            assert_eq!(mb, vec![ack("kb")]);
            assert_eq!(ma, vec![closed.clone(), lost("ka")]);
        }
        assert_eq!(mw, vec![closed.clone()]);
        let (winner, responder) = h.asks().wait(&q.id).await.unwrap();
        if a_won {
            assert_eq!((winner, responder), (Answer::Allow { remember: false }, Responder::Human("alice".into())));
        } else {
            assert_eq!((winner, responder), (Answer::Deny { reason: None }, Responder::Human("bob".into())));
        }

        // Retrying the winning key is idempotent; nobody gets a second close.
        let (wc, wkey) = if a_won { (&mut a, "ka") } else { (&mut b, "kb") };
        wc.send(answer(wkey, Answer::Allow { remember: false })).await.unwrap();
        assert_eq!(fence(wc).await, vec![ack(wkey)]);
        assert!(fence(&mut watcher).await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_lines_round_trip_over_duplex() {
    let srv = server();
    // The server loop is half-duplex per connection (it does not read while a
    // write is blocked), so the buffer must hold what the client writes
    // without reading; the long line below still exceeds BufReader's 8 KiB.
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server_io);
    let serving = {
        let srv = srv.clone();
        tokio::spawn(async move { srv.serve(JsonLines::new(sr, sw)).await })
    };
    let (cr, cw) = tokio::io::split(client_io);
    let mut c = JsonLines::new(cr, cw);
    async fn read<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
        c: &mut JsonLines<R, W>,
    ) -> ServerMessage {
        tokio::time::timeout(Duration::from_secs(5), c.read::<ServerMessage>()).await.unwrap().unwrap().unwrap()
    }

    // Before hello: only ping works.
    c.write(&submit("early", "x")).await.unwrap();
    assert!(matches!(read(&mut c).await, ServerMessage::Error { message } if message.contains("hello")));
    c.write(&ClientMessage::Ping).await.unwrap();
    assert_eq!(read(&mut c).await, ServerMessage::Pong);

    c.write(&ClientMessage::Hello { versions: vec![PROTOCOL_VERSION, 7], client: "cli".into() }).await.unwrap();
    assert_eq!(read(&mut c).await, ServerMessage::Welcome { version: PROTOCOL_VERSION });
    // A malformed message is reported; the connection survives.
    c.write(&serde_json::json!({"type": "nonsense"})).await.unwrap();
    assert!(matches!(read(&mut c).await, ServerMessage::Error { message } if message.contains("bad message")));

    c.write(&subscribe(0)).await.unwrap();
    // A long text exercises lines larger than the read buffer.
    let long = "y".repeat(10_000);
    c.write(&submit("k", &long)).await.unwrap();
    let mut got = vec![];
    while got.len() < 3 {
        got.push(read(&mut c).await);
    }
    let evs: Vec<_> = got
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Event { event, .. } => Some(event.as_ref().clone()),
            _ => None,
        })
        .collect();
    assert_eq!(seqs(&evs), vec![0, 1]);
    assert!(matches!(&evs[1].body, Event::UserMessage { text, .. } if *text == long));
    assert!(got.contains(&ack("k")));

    // Closing the client ends the connection cleanly.
    drop(c);
    let r = tokio::time::timeout(Duration::from_secs(5), serving).await.unwrap().unwrap();
    assert!(r.is_ok(), "{r:?}");
}

#[tokio::test]
async fn version_negotiation_failure_closes_the_connection() {
    assert_eq!(negotiate(&[3, 0, 1], &[0, 1, 2]), Some(1));
    assert_eq!(negotiate(&[5], &[0]), None);

    let srv = server();
    let (t, mut c) = channel();
    let serving = {
        let srv = srv.clone();
        tokio::spawn(async move { srv.serve(t).await })
    };
    c.send(ClientMessage::Hello { versions: vec![PROTOCOL_VERSION + 100], client: "future".into() }).await.unwrap();
    match recv(&mut c).await {
        ServerMessage::Error { message } => assert!(message.contains("no common protocol version"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(tokio::time::timeout(Duration::from_secs(5), c.recv()).await.unwrap().is_none(), "closed");
    let r = serving.await.unwrap();
    assert!(matches!(r, Err(ConnError::NoCommonVersion { .. })), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_session_with_resume_only_is_an_error() {
    let rt: Runtime<Toy> = Runtime::builder().build();
    let srv = Server::new(Arc::new(rt), ResumeOnly);
    let mut c = srv.connect();
    hello(&mut c, "c").await;
    c.send(subscribe(0)).await.unwrap();
    assert!(matches!(recv(&mut c).await, ServerMessage::Error { message } if message.contains("not found")));
    c.send(submit("k", "x")).await.unwrap();
    assert!(matches!(recv(&mut c).await, ServerMessage::Ack { accepted: false, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_transport() {
    use tokio_tungstenite::tungstenite::Message;
    let srv = server();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let srv = srv.clone();
        tokio::spawn(async move { srv.serve_ws_listener(listener).await });
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
    let hello = ClientMessage::Hello { versions: vec![PROTOCOL_VERSION], client: "ws".into() };
    for m in [hello, subscribe(0), submit("k", "hi")] {
        ws.send(Message::Text(serde_json::to_string(&m).unwrap())).await.unwrap();
    }
    let mut got = vec![];
    while got.len() < 4 {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        if let Message::Text(t) = m {
            got.push(serde_json::from_str::<ServerMessage>(&t).unwrap());
        }
    }
    assert_eq!(got[0], ServerMessage::Welcome { version: PROTOCOL_VERSION });
    assert!(got.contains(&ack("k")));
    let n = got.iter().filter(|m| matches!(m, ServerMessage::Event { .. })).count();
    assert_eq!(n, 2);
}
