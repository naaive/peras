//! `agent serve` end to end: a WebSocket client opens a new session on the
//! discovered agent and sees it start.

use agent::proto::*;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_opens_sessions_over_websocket() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
        .args(["serve", "--listen", "127.0.0.1:0"])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let _kill = Kill(child);
    let url = tokio::task::spawn_blocking(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if let Some(url) = line.strip_prefix("listening on ") {
                return url.to_string();
            }
        }
        panic!("server exited before listening");
    })
    .await
    .unwrap();

    let mut c = agent_server::connect_ws(&url).await.unwrap();
    let session = SessionId::new("cli-serve");
    c.send(ClientMessage::Hello { versions: vec![PROTOCOL_VERSION], client: "test".into() }).await.unwrap();
    c.send(ClientMessage::Subscribe { session: session.clone(), from_seq: 0, pulses: false }).await.unwrap();
    let mut welcomed = false;
    loop {
        let m = tokio::time::timeout(Duration::from_secs(20), c.recv()).await.expect("timed out").expect("closed");
        match m {
            ServerMessage::Welcome { .. } => welcomed = true,
            ServerMessage::Event { session: s, event } => {
                assert_eq!(s, session);
                assert!(matches!(event.body, Event::SessionStarted { .. }), "{:?}", event.body);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(welcomed);
    assert!(dir.path().join(".agent/runs.db").exists(), "journals to the default --db");
}
