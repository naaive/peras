//! Print mode (`peras -p "prompt"`): one turn, no interaction. Questions
//! that the permission mode, `--allowed-tools` and the policy leave open are
//! denied (or suspend the session with `--on-ask defer`).

use crate::render::{self, Totals};
use crate::Coding;
use agent::prelude::*;
use agent::proto::exit_code;
use serde_json::Value;
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// The final answer only.
    Text,
    /// One JSON object with the answer, session id, usage and cost.
    Json,
    /// JSON Lines: every update, then the result object.
    StreamJson,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<OutputFormat> {
        match s {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            "stream-json" => Some(OutputFormat::StreamJson),
            _ => None,
        }
    }
}

/// What a print-mode run produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub session: String,
    pub outcome: Option<TurnOutcome>,
    pub totals: Totals,
    pub tool_calls: u64,
    pub duration_ms: u64,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match &self.outcome {
            Some(TurnOutcome::Done { .. }) => exit_code::OK,
            Some(TurnOutcome::Suspended { .. }) => exit_code::SUSPENDED,
            Some(TurnOutcome::Interrupted) => exit_code::INTERRUPTED,
            _ => exit_code::FAILED,
        }
    }

    pub fn text(&self) -> String {
        match &self.outcome {
            Some(TurnOutcome::Done { text }) => text.clone(),
            Some(TurnOutcome::Failed { error }) => format!("Error: {error}"),
            Some(TurnOutcome::BudgetExhausted { what }) => format!("Error: budget exhausted ({what})"),
            Some(TurnOutcome::Suspended { .. }) => "Suspended: waiting for an approval (resume with --resume)".into(),
            Some(TurnOutcome::Interrupted) => "Interrupted".into(),
            None => "Error: the turn did not finish".into(),
        }
    }

    pub fn json(&self) -> Value {
        let subtype = match &self.outcome {
            Some(TurnOutcome::Done { .. }) => "success",
            Some(TurnOutcome::Suspended { .. }) => "suspended",
            Some(TurnOutcome::Interrupted) => "interrupted",
            Some(TurnOutcome::BudgetExhausted { .. }) => "error_budget",
            _ => "error",
        };
        json!({
            "type": "result",
            "subtype": subtype,
            "is_error": !matches!(self.outcome, Some(TurnOutcome::Done { .. })),
            "result": self.text(),
            "session_id": self.session,
            "num_replies": self.totals.replies,
            "num_tool_calls": self.tool_calls,
            "duration_ms": self.duration_ms,
            "total_cost_usd": self.totals.cost_micros as f64 / 1e6,
            "usage": {
                "input_tokens": self.totals.input,
                "output_tokens": self.totals.output,
                "cache_read_input_tokens": self.totals.cache_read,
                "cache_creation_input_tokens": self.totals.cache_write,
            },
        })
    }
}

/// Run one turn: in `resume`'s conversation, or a new one (recorded in the
/// workspace's conversation index).
pub async fn run(coding: &Coding, resume: Option<String>, prompt: String, format: OutputFormat, out: &mut dyn Write) -> Outcome {
    let started = std::time::Instant::now();
    let mut run = match &resume {
        Some(id) => coding.agent.session(id.clone()).stream(prompt.clone()),
        None => coding.agent.run(prompt.clone()),
    };
    let session = run.session_id().to_string();
    if resume.is_none() {
        let _ = coding.history().record(&session, &prompt);
    }
    let line = |out: &mut dyn Write, v: Value| {
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    };
    if format == OutputFormat::StreamJson {
        line(out, json!({"type": "system", "subtype": "init", "session_id": session, "mode": coding.permissions.mode().name()}));
    }
    let mut totals = Totals::default();
    let mut tool_calls = 0;
    let mut outcome = None;
    while let Some(u) = run.next().await {
        match u {
            Update::Reply(m) => {
                totals.add(&m.usage);
                if format == OutputFormat::StreamJson {
                    line(out, json!({"type": "assistant", "message": m}));
                }
            }
            Update::Tool { call, result } => {
                tool_calls += 1;
                if format == OutputFormat::StreamJson {
                    line(out, json!({"type": "tool_result", "call": call, "result": result, "text": render::result_text(&result)}));
                }
            }
            Update::Ask(a) => {
                if format == OutputFormat::StreamJson {
                    line(out, json!({"type": "permission_denied", "question": a.question}));
                }
                a.deny("Not allowed in print mode: no one can approve it (use --allowed-tools or --permission-mode)");
            }
            Update::Done(o) => outcome = Some(o),
            _ => {}
        }
    }
    let result = Outcome { session, outcome, totals, tool_calls, duration_ms: started.elapsed().as_millis() as u64 };
    match format {
        OutputFormat::Text => {
            let _ = writeln!(out, "{}", result.text());
        }
        OutputFormat::Json | OutputFormat::StreamJson => line(out, result.json()),
    }
    result
}
