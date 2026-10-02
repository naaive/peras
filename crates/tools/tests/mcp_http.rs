//! MCP over HTTP against in-process servers: Streamable HTTP (JSON and SSE
//! responses, session id, server-to-client ping) and the HTTP+SSE fallback.

use agent_proto::{Access, ResourceUri};
use agent_runtime::{AccessCtx, Tool};
use agent_tools::testing::{ctx, text_of};
use agent_tools::{McpClient, SseParser};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

async fn read_req(stream: &mut BufReader<TcpStream>) -> Option<Req> {
    let mut line = String::new();
    stream.read_line(&mut line).await.ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = vec![];
    loop {
        let mut h = String::new();
        stream.read_line(&mut h).await.ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.ok()?;
    Some(Req { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

async fn respond(stream: &mut BufReader<TcpStream>, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut out = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = stream.get_mut().write_all(out.as_bytes()).await;
}

fn result(id: &Value, r: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": r }).to_string()
}

fn handle_rpc(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?;
    msg.get("method")?;
    let r = match msg["method"].as_str()? {
        "initialize" => json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} }, "serverInfo": { "name": "http-fake" } }),
        "tools/list" => json!({ "tools": [{ "name": "echo", "description": "echo", "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } }] }),
        "tools/call" => json!({ "content": [{ "type": "text", "text": format!("echo: {}", msg["params"]["arguments"]["text"].as_str().unwrap_or("")) }] }),
        _ => return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } })),
    };
    Some(serde_json::from_str(&result(id, r)).unwrap())
}

#[derive(Default)]
struct Seen {
    /// (method, session header, protocol version header) per JSON-RPC message.
    messages: Vec<(String, Option<String>, Option<String>)>,
    ping_answered: bool,
}

/// Streamable HTTP: `initialize` -> JSON + session id; `tools/list` -> an SSE
/// stream with a server ping before the response; `tools/call` -> JSON;
/// notifications and responses -> 202.
async fn streamable_server(seen: Arc<Mutex<Seen>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut s = BufReader::new(s);
                let Some(req) = read_req(&mut s).await else { return };
                let msg: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("<response>").to_string();
                seen.lock().unwrap().messages.push((
                    method.clone(),
                    req.header("mcp-session-id").map(String::from),
                    req.header("mcp-protocol-version").map(String::from),
                ));
                if method == "<response>" {
                    if msg["id"] == "p1" && msg.get("result").is_some() {
                        seen.lock().unwrap().ping_answered = true;
                    }
                    return respond(&mut s, "202 Accepted", &[], "").await;
                }
                if method != "initialize" && req.header("mcp-session-id") != Some("sess-1") {
                    return respond(&mut s, "400 Bad Request", &[], "missing session").await;
                }
                match handle_rpc(&msg) {
                    None => respond(&mut s, "202 Accepted", &[], "").await,
                    Some(r) if method == "tools/list" => {
                        let ping = json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" });
                        let body = format!("event: message\ndata: {ping}\n\nid: 2\ndata: {r}\n\n");
                        respond(&mut s, "200 OK", &[("content-type", "text/event-stream")], &body).await
                    }
                    Some(r) => {
                        respond(&mut s, "200 OK", &[("content-type", "application/json"), ("mcp-session-id", "sess-1")], &r.to_string()).await
                    }
                }
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[tokio::test]
async fn streamable_http_roundtrip() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let url = streamable_server(seen.clone()).await;
    let client = McpClient::connect("remote", &url, &[("x-team".into(), "a".into())]).await.unwrap();
    assert_eq!(client.transport(), "http");
    assert_eq!(client.server_info()["serverInfo"]["name"], "http-fake");
    let tools = client.tools(false).await.unwrap();
    assert_eq!(tools.len(), 1);
    let echo = &tools[0];
    assert_eq!(echo.spec().name, "mcp__remote__echo");
    let acc = echo.access(&json!({"text": "hi"}), &AccessCtx { workspace: "/w".into() }).unwrap();
    assert_eq!(acc, vec![Access::write(ResourceUri::mcp("remote", "echo"))]);
    let out = echo.call(json!({"text": "hi"}), ctx("/w", acc)).await.unwrap();
    assert_eq!(text_of(&out), "echo: hi");
    assert!(out.trust.unwrap().is_untrusted());

    let seen = seen.lock().unwrap();
    assert!(seen.ping_answered, "server-to-client ping on the SSE stream answered");
    let methods: Vec<&str> = seen.messages.iter().map(|(m, _, _)| m.as_str()).collect();
    assert_eq!(methods[..2], ["initialize", "notifications/initialized"]);
    for (m, session, version) in &seen.messages[1..] {
        assert_eq!(session.as_deref(), Some("sess-1"), "{m}: session id sent back");
        assert_eq!(version.as_deref(), Some("2025-06-18"), "{m}: negotiated version sent");
    }
}

/// HTTP+SSE: POST to the URL is refused (405); GET opens the event stream
/// announcing the POST endpoint; responses arrive on the stream.
async fn legacy_server() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let (tx, rx) = (tx.clone(), rx.clone());
            tokio::spawn(async move {
                let mut s = BufReader::new(s);
                let Some(req) = read_req(&mut s).await else { return };
                match (req.method.as_str(), req.path.as_str()) {
                    ("POST", "/sse") => respond(&mut s, "405 Method Not Allowed", &[], "").await,
                    ("GET", "/sse") => {
                        let Some(mut rx) = rx.lock().await.take() else { return };
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n";
                        let w = s.get_mut();
                        let _ = w.write_all(head.as_bytes()).await;
                        let _ = w.write_all(b"event: endpoint\ndata: /messages?session=7\n\n").await;
                        while let Some(m) = rx.recv().await {
                            if w.write_all(format!("event: message\ndata: {m}\n\n").as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                    ("POST", p) if p.starts_with("/messages") => {
                        let msg: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
                        if let Some(r) = handle_rpc(&msg) {
                            let _ = tx.send(r.to_string());
                        }
                        respond(&mut s, "202 Accepted", &[], "").await
                    }
                    _ => respond(&mut s, "404 Not Found", &[], "").await,
                }
            });
        }
    });
    format!("http://{addr}/sse")
}

#[tokio::test]
async fn http_sse_fallback_roundtrip() {
    let url = legacy_server().await;
    let client = McpClient::connect("old", &url, &[]).await.unwrap();
    assert_eq!(client.transport(), "sse");
    let tools = client.tools(true).await.unwrap();
    let acc = vec![Access::write(tools[0].resource())];
    let out = tools[0].call(json!({"text": "there"}), ctx("/w", acc)).await.unwrap();
    assert_eq!(text_of(&out), "echo: there");
    assert_eq!(out.trust, None, "trusted server");
}

#[test]
fn sse_parser_handles_split_chunks_and_crlf() {
    let mut p = SseParser::default();
    assert!(p.push("event: endpoint\r\nda").is_empty());
    let evs = p.push("ta: /m\r\n\r\ndata: {\"a\":\ndata: 1}\n\n: comment\n\n");
    assert_eq!(evs.len(), 2);
    assert_eq!((evs[0].event.as_str(), evs[0].data.as_str()), ("endpoint", "/m"));
    assert_eq!((evs[1].event.as_str(), evs[1].data.as_str()), ("message", "{\"a\":\n1}"));
}
