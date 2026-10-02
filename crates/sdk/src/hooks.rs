//! Hooks, observers and auto-answer rules defined in configuration files.
//!
//! Hooks and observers share the executors: an external command (JSON on
//! stdin, JSON on stdout; for a hook exit code 2 = deny with stderr as the
//! reason), an HTTP endpoint (POST JSON, JSON response) or an MCP tool
//! (`server/tool`, called with the JSON object as arguments; its text content
//! is the response). Hooks may also make a judgment with the model: a model
//! call (`{ prompt = ".." }`: the instruction plus the gate request) or a
//! sub-agent (`{ agent = "name" }`: a sub-agent definition given the request
//! as its task). Both consume tokens: metered with the runtime's usage and
//! charged to the budget of the session whose gate they evaluate.
//!
//! A hook's answer is either the protocol `Verdict` JSON or the short form
//! `{"decision": "allow"|"deny"|"ask", "reason": "...", "context": "..."}`.
//! An observer receives each matching event envelope; it never affects
//! execution, and can only give feedback by answering `{"signal": <Signal>}`
//! (a notification, wake or silent state update delivered to the session).

use crate::agent::Agent;
use crate::subagent::Link;
use agent_profile::{AutoAnswer, AutoAnswerRule, HookDef, HookExecutor, ObserverDef};
use agent_proto::*;
use agent_runtime::{AutoRule, Delta, Hook, ModelPort, Observer};
use agent_tools::McpClient;
use async_trait::async_trait;
use futures::StreamExt;
use globset::Glob;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// What configured executors can reach.
#[derive(Clone)]
pub(crate) struct HookEnv {
    /// Connected MCP servers, by name.
    pub mcp: BTreeMap<String, Arc<McpClient>>,
    pub model: Arc<dyn ModelPort>,
    /// Sub-agent definitions, by name.
    pub agents: BTreeMap<String, Agent>,
    pub link: Arc<Link>,
}

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Run an executor with a JSON payload; returns its textual answer. `Ok(None)`
/// = a command hook exited with code 2 (deny, stderr is the reason).
async fn execute(exec: &HookExecutor, env: &HookEnv, payload: &serde_json::Value) -> Result<Result<String, String>, String> {
    let bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    match exec {
        HookExecutor::Command(c) => {
            let mut child = tokio::process::Command::new(&c.command)
                .args(&c.args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("spawn {}: {e}", c.command))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&bytes).await.map_err(|e| e.to_string())?;
            }
            let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
            match out.status.code() {
                Some(0) => Ok(Ok(String::from_utf8_lossy(&out.stdout).into_owned())),
                Some(2) => Ok(Err(String::from_utf8_lossy(&out.stderr).trim().to_string())),
                code => Err(format!("exited with {code:?}")),
            }
        }
        HookExecutor::Http(h) => {
            let resp = reqwest::Client::new()
                .post(&h.http)
                .header("content-type", "application/json")
                .body(bytes)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("http status {}", resp.status()));
            }
            Ok(Ok(resp.text().await.map_err(|e| e.to_string())?))
        }
        HookExecutor::Mcp(m) => {
            let (server, tool) = m.mcp.split_once('/').ok_or_else(|| format!("`{}` is not `server/tool`", m.mcp))?;
            let client = env.mcp.get(server).ok_or_else(|| format!("mcp server `{server}` is not connected"))?;
            let r = client.call_tool(tool, payload.clone()).await.map_err(|e| e.to_string())?;
            let text: Vec<String> = r
                .content
                .iter()
                .map(|c| c.get("text").and_then(|t| t.as_str()).map(String::from).unwrap_or_else(|| c.to_string()))
                .collect();
            if r.is_error {
                return Err(format!("mcp tool failed: {}", text.join("\n")));
            }
            Ok(Ok(text.join("\n")))
        }
        HookExecutor::Model(m) => model_judgment(env, &m.prompt, m.max_tokens.unwrap_or(1024), payload).await.map(Ok),
        HookExecutor::Subagent(s) => {
            let agent = env.agents.get(&s.agent).ok_or_else(|| format!("no sub-agent definition `{}`", s.agent))?;
            let task = format!("{VERDICT_FORMAT}\n\nRequest:\n{}", serde_json::to_string_pretty(payload).unwrap_or_default());
            let run = agent.run(task);
            let child = run.session_id().clone();
            let answer = run.await;
            // The judging session's consumption is the gated session's.
            if let Ok(rt) = agent.runtime().await {
                if let Some(h) = rt.session(&child) {
                    let (tokens, cost_micros) = h.with_state(agent_kernel::usage);
                    agent_runtime::charge_hook_spend(Spend { tokens, cost_micros });
                }
            }
            answer.map(Ok).map_err(|e| e.to_string())
        }
    }
}

/// Appended to model / sub-agent judging instructions.
const VERDICT_FORMAT: &str = "Answer with only a JSON object: {\"decision\": \"allow\" | \"deny\" | \"ask\", \"reason\": \"...\"} (\"context\": \"...\" adds a note for the agent).";

/// One model call: the instruction as system prompt, the request as the user
/// message; the reply text is the answer. Usage is metered.
async fn model_judgment(env: &HookEnv, prompt: &str, max_tokens: u32, payload: &serde_json::Value) -> Result<String, String> {
    let caps = env.model.caps().clone();
    let head = SeqHead {
        seq_no: 0,
        model: caps.model.clone(),
        system: vec![format!("{prompt}\n\n{VERDICT_FORMAT}")],
        tools: vec![],
        render: caps.render.clone(),
        encoder_version: env.model.encoder().version(),
    };
    let body = vec![Rendered::text(Role::User, serde_json::to_string_pretty(payload).unwrap_or_default())];
    let req = env.model.encoder().encode(&head, &body, max_tokens);
    let mut stream = env.model.stream(req);
    let mut text = String::new();
    while let Some(d) = stream.next().await {
        match d.map_err(|e| e.to_string())? {
            Delta::Text(t) => text.push_str(&t),
            Delta::Usage(u) => {
                if let Some(m) = env.link.metrics() {
                    m.observe_usage(&u);
                }
                // Charged to the budget of the session being gated.
                agent_runtime::charge_hook_spend(Spend::of(&u));
            }
            _ => {}
        }
    }
    Ok(text)
}

pub(crate) fn from_def(def: &HookDef, env: &HookEnv) -> Arc<dyn Hook> {
    Arc::new(ConfigHook { def: def.clone(), env: env.clone() })
}

struct ConfigHook {
    def: HookDef,
    env: HookEnv,
}

fn matches(pattern: &Option<String>, value: &str) -> bool {
    match pattern {
        None => true,
        Some(p) => Glob::new(p).map(|g| g.compile_matcher().is_match(value)).unwrap_or(false),
    }
}

fn subject_tool(req: &GateRequest) -> Option<&ToolCall> {
    match &req.subject {
        GateSubject::Tool { call } | GateSubject::PostTool { call, .. } => Some(call),
        _ => None,
    }
}

/// Strip a Markdown code fence around a JSON answer.
fn unfence(out: &str) -> &str {
    let t = out.trim();
    t.strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end().trim_end_matches("```").trim())
        .unwrap_or(t)
}

pub(crate) fn parse_verdict(out: &str) -> Result<Verdict, String> {
    let out = unfence(out);
    if out.is_empty() {
        return Ok(Verdict::Allow);
    }
    if let Ok(v) = serde_json::from_str::<Verdict>(out) {
        return Ok(v);
    }
    let v: serde_json::Value = match serde_json::from_str(out) {
        Ok(v) => v,
        // A model may wrap the object in prose: take the outermost object.
        Err(e) => match (out.find('{'), out.rfind('}')) {
            (Some(a), Some(b)) if b > a => serde_json::from_str(&out[a..=b]).map_err(|_| format!("hook output is not JSON: {e}"))?,
            _ => return Err(format!("hook output is not JSON: {e}")),
        },
    };
    let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string();
    match v.get("decision").and_then(|d| d.as_str()) {
        Some("allow") | None => match v.get("context").and_then(|c| c.as_str()) {
            Some(ctx) => Ok(Verdict::Annotate(Context { text: ctx.into(), trust: Trust::Guidance })),
            None => Ok(Verdict::Allow),
        },
        Some("deny") => Ok(Verdict::deny(reason)),
        Some("ask") => Ok(Verdict::ask(if reason.is_empty() { "hook asks for approval".into() } else { reason })),
        Some("continue") => Ok(Verdict::Continue(Reason(reason))),
        Some("defer") => Ok(Verdict::Defer),
        Some(other) => Err(format!("unknown decision `{other}`")),
    }
}

#[async_trait]
impl Hook for ConfigHook {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn points(&self) -> Vec<HookPoint> {
        vec![self.def.point]
    }
    async fn run(&self, req: &GateRequest) -> Result<Verdict, String> {
        if let Some(call) = subject_tool(req) {
            if !matches(&self.def.matcher, &call.name) {
                return Ok(Verdict::Allow);
            }
        }
        let payload = serde_json::to_value(req).map_err(|e| e.to_string())?;
        // Judgments by the model get a longer default.
        let default = if self.def.executor.uses_model() { 4 * DEFAULT_TIMEOUT_MS } else { DEFAULT_TIMEOUT_MS };
        let timeout = Duration::from_millis(self.def.timeout_ms.unwrap_or(default));
        match tokio::time::timeout(timeout, execute(&self.def.executor, &self.env, &payload)).await {
            Err(_) => Err("hook timed out".to_string()),
            Ok(Err(e)) => Err(e),
            Ok(Ok(Err(reason))) => Ok(Verdict::deny(reason)),
            Ok(Ok(Ok(out))) => parse_verdict(&out),
        }
    }
}

// ---------------------------------------------------------------- observers

pub(crate) fn observer(def: &ObserverDef, env: &HookEnv) -> Arc<dyn Observer> {
    Arc::new(ConfigObserver { def: def.clone(), env: env.clone() })
}

struct ConfigObserver {
    def: ObserverDef,
    env: HookEnv,
}

/// The signal an observer answered with, if any (only notifications, wakes and
/// silent state updates: an observer cannot speak for the user).
pub(crate) fn observer_signal(out: &str) -> Result<Option<Signal>, String> {
    let out = unfence(out);
    if out.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(out).map_err(|e| format!("observer output is not JSON: {e}"))?;
    let Some(sig) = v.get("signal") else { return Ok(None) };
    let sig: Signal = serde_json::from_value(sig.clone()).map_err(|e| format!("bad signal: {e}"))?;
    match sig {
        Signal::Notify { .. } | Signal::Wake { .. } | Signal::Silent { .. } => Ok(Some(sig)),
        _ => Err("observers may only send notify, wake or silent signals".into()),
    }
}

#[async_trait]
impl Observer for ConfigObserver {
    fn name(&self) -> &str {
        &self.def.name
    }
    async fn on_event(&self, session: &SessionId, ev: &Envelope<Event>) -> Result<(), String> {
        if !self.def.wants(ev.body.type_name()) {
            return Ok(());
        }
        let payload = serde_json::json!({ "session": session, "event": ev });
        let timeout = Duration::from_millis(self.def.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let out = match tokio::time::timeout(timeout, execute(&self.def.executor, &self.env, &payload)).await {
            Err(_) => return Err("observer timed out".into()),
            Ok(r) => r?,
        };
        let out = out.map_err(|e| format!("observer exited with 2: {e}"))?;
        if let Some(sig) = observer_signal(&out)? {
            let h = self.env.link.session(session).ok_or("session is not live")?;
            h.post(Input::Signal(sig)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- auto rules

pub(crate) fn auto_rule(r: &AutoAnswerRule) -> Arc<dyn AutoRule> {
    Arc::new(ConfigRule { rule: r.clone() })
}

struct ConfigRule {
    rule: AutoAnswerRule,
}

impl AutoRule for ConfigRule {
    fn name(&self) -> &str {
        &self.rule.rule
    }
    fn answer(&self, req: &GateRequest, _q: &Question) -> Option<Answer> {
        let call = subject_tool(req)?;
        if !matches(&self.rule.tool, &call.name) {
            return None;
        }
        if self.rule.resource.is_some() && !call.access.iter().any(|a| matches(&self.rule.resource, a.resource.as_str())) {
            return None;
        }
        Some(match self.rule.answer {
            AutoAnswer::Allow => Answer::Allow { remember: false },
            AutoAnswer::Deny => Answer::Deny { reason: Some(format!("auto rule `{}`", self.rule.rule)) },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_form_verdicts() {
        assert_eq!(parse_verdict("").unwrap(), Verdict::Allow);
        assert_eq!(parse_verdict(r#"{"decision":"deny","reason":"no"}"#).unwrap(), Verdict::deny("no"));
        assert!(matches!(parse_verdict(r#"{"decision":"ask"}"#).unwrap(), Verdict::Ask(_)));
        assert_eq!(parse_verdict(r#"{"verdict":"allow"}"#).unwrap(), Verdict::Allow);
        assert_eq!(
            parse_verdict("```json\n{\"decision\":\"deny\",\"reason\":\"fenced\"}\n```").unwrap(),
            Verdict::deny("fenced")
        );
        assert_eq!(
            parse_verdict("I think: {\"decision\":\"deny\",\"reason\":\"prose\"} ok").unwrap(),
            Verdict::deny("prose")
        );
        assert!(parse_verdict("nope").is_err());
    }

    #[test]
    fn observer_signals() {
        assert_eq!(observer_signal("").unwrap(), None);
        assert_eq!(observer_signal("{}").unwrap(), None);
        let s = observer_signal(r#"{"signal":{"kind":"notify","source":"ci","key":"ci","text":"red"}}"#).unwrap();
        assert!(matches!(s, Some(Signal::Notify { .. })));
        assert!(observer_signal(r#"{"signal":{"kind":"submit","text":"rm -rf"}}"#).is_err());
    }
}
