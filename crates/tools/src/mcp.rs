//! Minimal MCP client (JSON-RPC 2.0): `initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`. Transports:
//! stdio (a child process, newline-delimited messages), Streamable HTTP
//! (messages POSTed to one endpoint, responses as JSON or an SSE stream, the
//! `Mcp-Session-Id` the server assigns sent back) and, for servers that refuse
//! it, HTTP+SSE (a GET event stream announcing a POST endpoint).
//!
//! Each remote tool is wrapped as an [`McpTool`] declaring `mcp:<server>/<tool>`
//! (write), class `Network` by default, with results labelled
//! `Untrusted { source: "mcp:<server>" }` unless the server is marked trusted.

use crate::caps::check_granted;
use agent_proto::{Access, EffectClass, ResourceUri, ToolContent, ToolSpec, Trust};
use agent_runtime::{AccessCtx, Tool, ToolCtx, ToolError, ToolOutput};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use futures::StreamExt;
use tokio::sync::oneshot;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum McpError {
    #[error("io: {0}")]
    Io(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("server closed the connection")]
    Closed,
    #[error("request timed out")]
    Timeout,
    #[error("http status {status}: {message}")]
    Http { status: u16, message: String },
}

// ------------------------------------------------------------------ framing

/// An incoming JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response {
        id: Value,
        result: Result<Value, McpError>,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// One outgoing message, newline-terminated.
pub fn encode_request(id: u64, method: &str, params: Value) -> String {
    let mut s =
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
    s.push('\n');
    s
}

pub fn encode_notification(method: &str, params: Value) -> String {
    let mut s = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
    s.push('\n');
    s
}

fn encode_response(id: &Value, result: Result<Value, (i64, &str)>) -> String {
    let mut s = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    }
    .to_string();
    s.push('\n');
    s
}

/// Decode one line.
pub fn decode_line(line: &str) -> Result<Incoming, McpError> {
    let v: Value =
        serde_json::from_str(line).map_err(|e| McpError::Protocol(format!("bad json: {e}")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| McpError::Protocol("message is not an object".into()))?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::Protocol("missing jsonrpc 2.0".into()));
    }
    let id = obj.get("id").cloned().filter(|i| !i.is_null());
    match (obj.get("method").and_then(Value::as_str), id) {
        (Some(m), Some(id)) => Ok(Incoming::Request {
            id,
            method: m.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(m), None) => Ok(Incoming::Notification {
            method: m.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => {
            let result = if let Some(e) = obj.get("error") {
                Err(McpError::Rpc {
                    code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: e
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            } else {
                Ok(obj.get("result").cloned().unwrap_or(Value::Null))
            };
            Ok(Incoming::Response { id, result })
        }
        (None, None) => Err(McpError::Protocol(
            "message has neither id nor method".into(),
        )),
    }
}

// ------------------------------------------------------------------ client

/// A tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Result of `tools/call`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

/// How messages reach the server.
enum Transport {
    /// A child process: newline-delimited JSON-RPC on stdin / stdout.
    Stdio {
        stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
        _child: tokio::sync::Mutex<Child>,
    },
    /// Streamable HTTP: every message is POSTed to the endpoint; a request's
    /// response comes back as a JSON body or as an SSE stream.
    Http(HttpTransport),
    /// HTTP+SSE (the earlier remote transport): a long-lived GET event stream
    /// carries the server's messages; client messages are POSTed to the
    /// endpoint the stream announces.
    Sse {
        http: reqwest::Client,
        endpoint: String,
        headers: Vec<(String, String)>,
        reader: tokio::task::JoinHandle<()>,
    },
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Transport::Sse { reader, .. } = self {
            reader.abort();
        }
    }
}

struct HttpTransport {
    http: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    /// `Mcp-Session-Id` assigned by the server at initialization.
    session: Mutex<Option<String>>,
    /// Negotiated protocol version (sent as `MCP-Protocol-Version`).
    version: Mutex<Option<String>>,
}

/// A connection to one MCP server (a child process or a remote endpoint).
pub struct McpClient {
    server: String,
    transport: Transport,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    server_info: Mutex<Value>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.server)
            .finish()
    }
}

/// Route a response to its waiter.
fn deliver(pending: &Pending, id: &Value, result: Result<Value, McpError>) {
    if let Some(id) = id.as_u64() {
        if let Some(tx) = pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            let _ = tx.send(result);
        }
    }
}

fn fail_all(pending: &Pending, e: McpError) {
    for (_, tx) in pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
        let _ = tx.send(Err(e.clone()));
    }
}

/// The reply to a server-initiated request: `ping` is answered, anything else
/// is not supported by this client.
fn reply_to(id: &Value, method: &str) -> String {
    if method == "ping" {
        encode_response(id, Ok(json!({})))
    } else {
        encode_response(id, Err((-32601, "method not found")))
    }
}

/// Incremental parser of a `text/event-stream` body.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` field (`message` when absent).
    pub event: String,
    /// `data:` lines joined with newlines.
    pub data: String,
}

impl SseParser {
    /// Feed text; returns the events it completed.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut out = Vec::new();
        while let Some(end) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..end + 2).collect();
            let mut event = String::from("message");
            let mut data: Vec<&str> = Vec::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v));
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                }
            }
            if !data.is_empty() {
                out.push(SseEvent {
                    event,
                    data: data.join("\n"),
                });
            }
        }
        out
    }
}

/// JSON-RPC messages of a body: one object or a batch array.
fn messages_of(body: &str) -> Vec<Result<Incoming, McpError>> {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Array(items)) => items
            .iter()
            .map(|v| decode_line(&v.to_string()))
            .collect(),
        Ok(v) => vec![decode_line(&v.to_string())],
        Err(e) => vec![Err(McpError::Protocol(format!("bad json: {e}")))],
    }
}

impl HttpTransport {
    fn post(&self, body: String) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .body(body);
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(s) = self.session.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Mcp-Session-Id", s);
        }
        if let Some(v) = self.version.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("MCP-Protocol-Version", v);
        }
        rb
    }

    async fn send(&self, body: String) -> Result<reqwest::Response, McpError> {
        let resp = self
            .post(body)
            .send()
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp.text().await.unwrap_or_default();
            return Err(McpError::Http { status, message });
        }
        Ok(resp)
    }

    /// POST a request and wait for the response with `id`, in a JSON body or
    /// on an SSE stream (server requests on the stream are answered).
    async fn request(&self, id: u64, body: String) -> Result<Value, McpError> {
        let resp = self.send(body).await?;
        let sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        let want = json!(id);
        if !sse {
            let text = resp
                .text()
                .await
                .map_err(|e| McpError::Io(e.to_string()))?;
            for m in messages_of(&text) {
                if let Ok(Incoming::Response { id: rid, result }) = m {
                    if rid == want {
                        return result;
                    }
                }
            }
            return Err(McpError::Protocol(format!("no response to request {id}")));
        }
        let mut parser = SseParser::default();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| McpError::Io(e.to_string()))?;
            for ev in parser.push(&String::from_utf8_lossy(&chunk)) {
                match decode_line(&ev.data) {
                    Ok(Incoming::Response { id: rid, result }) if rid == want => return result,
                    Ok(Incoming::Request {
                        id: rid, method, ..
                    }) => {
                        let _ = self.send(reply_to(&rid, &method)).await;
                    }
                    _ => {}
                }
            }
        }
        Err(McpError::Closed)
    }
}

impl McpClient {
    fn new(server: &str, transport: Transport, pending: Pending) -> Arc<McpClient> {
        Arc::new(McpClient {
            server: server.to_string(),
            transport,
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(120),
            server_info: Mutex::new(Value::Null),
        })
    }

    /// Spawn `program args...` and perform the `initialize` handshake.
    pub async fn spawn(
        server: &str,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Arc<McpClient>, McpError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Io(format!("spawn {program}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Io("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Io("no stdout".into()))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let pending: Pending = Arc::default();

        let (p, w) = (pending.clone(), stdin.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match decode_line(&line) {
                    Ok(Incoming::Response { id, result }) => deliver(&p, &id, result),
                    Ok(Incoming::Request { id, method, .. }) => {
                        let reply = reply_to(&id, &method);
                        let _ = w.lock().await.write_all(reply.as_bytes()).await;
                    }
                    Ok(Incoming::Notification { .. }) | Err(_) => {}
                }
            }
            fail_all(&p, McpError::Closed);
        });

        let transport = Transport::Stdio {
            stdin,
            _child: tokio::sync::Mutex::new(child),
        };
        let client = McpClient::new(server, transport, pending);
        client.initialize().await?;
        Ok(client)
    }

    /// Connect to a remote server at `url` and perform the `initialize`
    /// handshake. Streamable HTTP is tried first; a server that refuses the
    /// POST with 400 / 404 / 405 is spoken to over HTTP+SSE (a GET event
    /// stream). `headers` go with every request (e.g. authorization).
    pub async fn connect(
        server: &str,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Arc<McpClient>, McpError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| McpError::Io(e.to_string()))?;
        let t = HttpTransport {
            http: http.clone(),
            url: url.to_string(),
            headers: headers.to_vec(),
            session: Mutex::new(None),
            version: Mutex::new(None),
        };
        let client = McpClient::new(server, Transport::Http(t), Arc::default());
        match client.initialize().await {
            Ok(()) => Ok(client),
            Err(McpError::Http { status: 400 | 404 | 405, .. }) => {
                McpClient::connect_sse(server, url, headers, http).await
            }
            Err(e) => Err(e),
        }
    }

    async fn connect_sse(
        server: &str,
        url: &str,
        headers: &[(String, String)],
        http: reqwest::Client,
    ) -> Result<Arc<McpClient>, McpError> {
        let mut rb = http
            .get(url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.send().await.map_err(|e| McpError::Io(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(McpError::Http {
                status: resp.status().as_u16(),
                message: "event stream refused".into(),
            });
        }
        let base = url::Url::parse(url).map_err(|e| McpError::Protocol(e.to_string()))?;
        let pending: Pending = Arc::default();
        let (ep_tx, ep_rx) = oneshot::channel::<String>();
        let (p, h2, hdrs) = (pending.clone(), http.clone(), headers.to_vec());
        let reader = tokio::spawn(async move {
            let mut ep_tx = Some(ep_tx);
            let mut endpoint: Option<String> = None;
            let mut parser = SseParser::default();
            let mut stream = resp.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                for ev in parser.push(&String::from_utf8_lossy(&chunk)) {
                    if ev.event == "endpoint" {
                        let ep = base
                            .join(ev.data.trim())
                            .map(|u| u.to_string())
                            .unwrap_or(ev.data);
                        endpoint = Some(ep.clone());
                        if let Some(tx) = ep_tx.take() {
                            let _ = tx.send(ep);
                        }
                        continue;
                    }
                    match decode_line(&ev.data) {
                        Ok(Incoming::Response { id, result }) => deliver(&p, &id, result),
                        Ok(Incoming::Request { id, method, .. }) => {
                            if let Some(ep) = &endpoint {
                                let mut rb = h2
                                    .post(ep)
                                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                                    .body(reply_to(&id, &method));
                                for (k, v) in &hdrs {
                                    rb = rb.header(k.as_str(), v.as_str());
                                }
                                let _ = rb.send().await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            fail_all(&p, McpError::Closed);
        });
        let endpoint = match tokio::time::timeout(Duration::from_secs(30), ep_rx).await {
            Ok(Ok(ep)) => ep,
            _ => {
                reader.abort();
                return Err(McpError::Protocol(
                    "event stream announced no endpoint".into(),
                ));
            }
        };
        let transport = Transport::Sse {
            http,
            endpoint,
            headers: headers.to_vec(),
            reader,
        };
        let client = McpClient::new(server, transport, pending);
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let info = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "agent-tools", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        if let Transport::Http(h) = &self.transport {
            let v = info
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            *h.version.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
        }
        *self.server_info.lock().unwrap_or_else(|e| e.into_inner()) = info;
        self.notify("notifications/initialized", json!({})).await
    }

    /// The transport in use: `stdio`, `http` (Streamable HTTP) or `sse`.
    pub fn transport(&self) -> &'static str {
        match &self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http(_) => "http",
            Transport::Sse { .. } => "sse",
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// The `initialize` result.
    pub fn server_info(&self) -> Value {
        self.server_info
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Send one message on a transport whose responses arrive out of band
    /// (stdio, HTTP+SSE), or a notification on any transport.
    async fn write(&self, s: &str) -> Result<(), McpError> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut w = stdin.lock().await;
                w.write_all(s.as_bytes())
                    .await
                    .map_err(|e| McpError::Io(e.to_string()))?;
                w.flush().await.map_err(|e| McpError::Io(e.to_string()))
            }
            Transport::Sse {
                http,
                endpoint,
                headers,
                ..
            } => {
                let mut rb = http
                    .post(endpoint)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(s.trim_end().to_string());
                for (k, v) in headers {
                    rb = rb.header(k.as_str(), v.as_str());
                }
                let resp = rb.send().await.map_err(|e| McpError::Io(e.to_string()))?;
                if resp.status().is_success() {
                    Ok(())
                } else {
                    Err(McpError::Http {
                        status: resp.status().as_u16(),
                        message: resp.text().await.unwrap_or_default(),
                    })
                }
            }
            Transport::Http(h) => h.send(s.trim_end().to_string()).await.map(|_| ()),
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Transport::Http(h) = &self.transport {
            let body = encode_request(id, method, params).trim_end().to_string();
            return match tokio::time::timeout(self.timeout, h.request(id, body)).await {
                Ok(r) => r,
                Err(_) => Err(McpError::Timeout),
            };
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if let Err(e) = self.write(&encode_request(id, method, params)).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&encode_notification(method, params)).await
    }

    /// `tools/list`, following pagination cursors.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = self.request("tools/list", params).await?;
            let tools = r.get("tools").cloned().unwrap_or(Value::Array(vec![]));
            let defs: Vec<McpToolDef> = serde_json::from_value(tools)
                .map_err(|e| McpError::Protocol(format!("tools/list: {e}")))?;
            out.extend(defs);
            match r.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpCallResult, McpError> {
        let r = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        serde_json::from_value(r).map_err(|e| McpError::Protocol(format!("tools/call: {e}")))
    }

    /// Wrap every advertised tool.
    pub async fn tools(self: &Arc<Self>, trusted: bool) -> Result<Vec<McpTool>, McpError> {
        Ok(self
            .list_tools()
            .await?
            .into_iter()
            .map(|d| McpTool::new(self.clone(), d).trusted(trusted))
            .collect())
    }
}

// ------------------------------------------------------------------ tool

/// A remote MCP tool.
#[derive(Debug, Clone)]
pub struct McpTool {
    client: Arc<McpClient>,
    def: McpToolDef,
    trusted: bool,
    class: EffectClass,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, def: McpToolDef) -> Self {
        McpTool {
            client,
            def,
            trusted: false,
            class: EffectClass::Network,
        }
    }
    /// Results of a trusted server are not labelled untrusted.
    pub fn trusted(mut self, trusted: bool) -> Self {
        self.trusted = trusted;
        self
    }
    pub fn with_class(mut self, class: EffectClass) -> Self {
        self.class = class;
        self
    }
    pub fn resource(&self) -> ResourceUri {
        ResourceUri::mcp(self.client.server(), &self.def.name)
    }
    /// Name shown to the model: `mcp__<server>__<tool>`.
    pub fn name(&self) -> String {
        format!("mcp__{}__{}", self.client.server(), self.def.name)
    }
    pub fn def(&self) -> &McpToolDef {
        &self.def
    }
}

fn map_content(c: &Value) -> ToolContent {
    match c.get("type").and_then(Value::as_str) {
        Some("text") => ToolContent::Text {
            text: c
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        _ => ToolContent::Json { value: c.clone() },
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        let schema = if self.def.input_schema.is_object() {
            self.def.input_schema.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        };
        ToolSpec {
            name: self.name(),
            description: self.def.description.clone().unwrap_or_default(),
            input_schema: schema,
            class: self.class,
            subagent: false,
        }
    }
    fn access(&self, _input: &Value, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::write(self.resource())])
    }
    fn class(&self, _input: &Value) -> EffectClass {
        self.class
    }
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        check_granted(&ctx, &Access::write(self.resource()))?;
        let args = if input.is_null() { json!({}) } else { input };
        let r = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = self.client.call_tool(&self.def.name, args) => r,
        };
        let r = match r {
            Ok(r) => r,
            Err(e @ McpError::Rpc { .. }) => return Err(ToolError::Failed(e.to_string())),
            Err(e) => {
                return Err(ToolError::Infra(format!(
                    "mcp:{}: {e}",
                    self.client.server()
                )))
            }
        };
        let content: Vec<ToolContent> = r.content.iter().map(map_content).collect();
        if r.is_error {
            let text = content
                .iter()
                .map(|c| match c {
                    ToolContent::Text { text } => text.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(ToolError::Failed(text));
        }
        let trust = if self.trusted {
            None
        } else {
            Some(Trust::Untrusted {
                source: format!("mcp:{}", self.client.server()),
            })
        };
        Ok(ToolOutput {
            content,
            trust,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let s = encode_request(3, "tools/list", json!({}));
        assert!(s.ends_with('\n'));
        let v: Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["jsonrpc"], "2.0");
        assert!(encode_notification("notifications/initialized", json!({}))
            .find("\"id\"")
            .is_none());

        assert_eq!(
            decode_line(r#"{"jsonrpc":"2.0","id":1,"result":{"a":1}}"#).unwrap(),
            Incoming::Response {
                id: json!(1),
                result: Ok(json!({"a":1}))
            }
        );
        assert_eq!(
            decode_line(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"nope"}}"#)
                .unwrap(),
            Incoming::Response {
                id: json!(2),
                result: Err(McpError::Rpc {
                    code: -32601,
                    message: "nope".into()
                })
            }
        );
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","id":"x","method":"ping"}"#).unwrap(),
            Incoming::Request { .. }
        ));
        assert!(matches!(
            decode_line(r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#)
                .unwrap(),
            Incoming::Notification { .. }
        ));
        assert!(decode_line("not json").is_err());
        assert!(decode_line(r#"{"id":1}"#).is_err());
    }
}
