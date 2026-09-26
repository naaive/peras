//! Minimal MCP client over stdio (JSON-RPC 2.0, newline-delimited messages):
//! `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.
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

/// A connection to one MCP server process.
pub struct McpClient {
    server: String,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    server_info: Mutex<Value>,
    _child: tokio::sync::Mutex<Child>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.server)
            .finish()
    }
}

impl McpClient {
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
                    Ok(Incoming::Response { id, result }) => {
                        if let Some(id) = id.as_u64() {
                            if let Some(tx) =
                                p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id)
                            {
                                let _ = tx.send(result);
                            }
                        }
                    }
                    Ok(Incoming::Request { id, method, .. }) => {
                        let reply = if method == "ping" {
                            encode_response(&id, Ok(json!({})))
                        } else {
                            encode_response(&id, Err((-32601, "method not found")))
                        };
                        let _ = w.lock().await.write_all(reply.as_bytes()).await;
                    }
                    Ok(Incoming::Notification { .. }) | Err(_) => {}
                }
            }
            for (_, tx) in p.lock().unwrap_or_else(|e| e.into_inner()).drain() {
                let _ = tx.send(Err(McpError::Closed));
            }
        });

        let client = Arc::new(McpClient {
            server: server.to_string(),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(120),
            server_info: Mutex::new(Value::Null),
            _child: tokio::sync::Mutex::new(child),
        });
        let info = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "agent-tools", "version": env!("CARGO_PKG_VERSION") }
                }),
            )
            .await?;
        *client.server_info.lock().unwrap_or_else(|e| e.into_inner()) = info;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
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

    async fn write(&self, s: &str) -> Result<(), McpError> {
        let mut w = self.stdin.lock().await;
        w.write_all(s.as_bytes())
            .await
            .map_err(|e| McpError::Io(e.to_string()))?;
        w.flush().await.map_err(|e| McpError::Io(e.to_string()))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
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
            observed: vec![],
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
