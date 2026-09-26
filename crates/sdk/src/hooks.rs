//! Hooks and auto-answer rules defined in configuration files.
//!
//! Executors: an external command (GateRequest JSON on stdin, verdict JSON on
//! stdout; exit code 2 = deny with stderr as the reason) or an HTTP endpoint
//! (POST GateRequest JSON, verdict JSON response). A verdict is either the
//! protocol `Verdict` JSON or the short form
//! `{"decision": "allow"|"deny"|"ask", "reason": "...", "context": "..."}`.

use agent_profile::{AutoAnswer, AutoAnswerRule, HookDef, HookExecutor};
use agent_proto::*;
use agent_runtime::{AutoRule, Hook};
use async_trait::async_trait;
use globset::Glob;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub(crate) fn from_def(def: &HookDef) -> Option<Arc<dyn Hook>> {
    match &def.executor {
        HookExecutor::Command(_) | HookExecutor::Http(_) => Some(Arc::new(ConfigHook { def: def.clone() })),
        HookExecutor::Mcp(_) => None,
    }
}

struct ConfigHook {
    def: HookDef,
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

pub(crate) fn parse_verdict(out: &str) -> Result<Verdict, String> {
    let out = out.trim();
    if out.is_empty() {
        return Ok(Verdict::Allow);
    }
    if let Ok(v) = serde_json::from_str::<Verdict>(out) {
        return Ok(v);
    }
    let v: serde_json::Value = serde_json::from_str(out).map_err(|e| format!("hook output is not JSON: {e}"))?;
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
        let payload = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        let timeout = Duration::from_millis(self.def.timeout_ms.unwrap_or(30_000));
        let fut = async {
            match &self.def.executor {
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
                        stdin.write_all(&payload).await.map_err(|e| e.to_string())?;
                    }
                    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
                    match out.status.code() {
                        Some(0) => parse_verdict(&String::from_utf8_lossy(&out.stdout)),
                        Some(2) => Ok(Verdict::deny(String::from_utf8_lossy(&out.stderr).trim().to_string())),
                        code => Err(format!("hook exited with {code:?}")),
                    }
                }
                HookExecutor::Http(h) => {
                    let resp = reqwest::Client::new()
                        .post(&h.http)
                        .header("content-type", "application/json")
                        .body(payload)
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    if !resp.status().is_success() {
                        return Err(format!("hook http status {}", resp.status()));
                    }
                    parse_verdict(&resp.text().await.map_err(|e| e.to_string())?)
                }
                HookExecutor::Mcp(_) => Err("mcp hook executors are not supported".into()),
            }
        };
        tokio::time::timeout(timeout, fut).await.map_err(|_| "hook timed out".to_string())?
    }
}

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
        assert!(parse_verdict("nope").is_err());
    }
}
