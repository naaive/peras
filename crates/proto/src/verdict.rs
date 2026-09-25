//! Gate verdicts, rings, hook points, questions and approval levels.

use crate::envelope::Trust;
use crate::ids::QuestionId;
use crate::tool::{ToolCall, ToolResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The five rings of the gate chain, evaluated in order. Each ring may only make
/// the result stricter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ring {
    /// Built into the kernel, cannot be disabled.
    Invariant = 1,
    /// Kernel, rules from the Profile.
    Policy = 2,
    /// Kernel: tokens, money, time, calls, repeats.
    Budget = 3,
    /// Runtime hook executors.
    Hook = 4,
    /// Auto-answer rules, then the client.
    Human = 5,
}

/// Hook points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    SessionStart,
    UserSubmit,
    PreSample,
    PreTool,
    Permission,
    PostTool,
    PreCompact,
    Stop,
}

impl HookPoint {
    /// Behaviour when the hook executor itself fails.
    pub fn on_failure(self) -> FailureMode {
        match self {
            HookPoint::PreSample | HookPoint::PreTool => FailureMode::Block,
            HookPoint::Permission => FailureMode::Human,
            _ => FailureMode::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    Allow,
    Block,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct Reason(pub String);

impl<S: Into<String>> From<S> for Reason {
    fn from(s: S) -> Self {
        Reason(s.into())
    }
}

/// A rewrite proposal: new call arguments or a new tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Proposal {
    Call(ToolCall),
    Result(ToolResult),
    UserText(String),
}

/// Context injected by a gate; the source is annotated automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Context {
    pub text: String,
    /// Filled by the framework, not by the gate.
    #[serde(default = "default_guidance")]
    pub trust: Trust,
}

fn default_guidance() -> Trust {
    Trust::Guidance
}

/// Approval level. Assigned by the framework from the ring that asked; a gate
/// cannot self-report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLevel {
    /// Rings 2–4: auto-answer rules first, then a human.
    Policy,
    /// Ring 1: human only (or a framework-launched disposable environment).
    Invariant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub id: QuestionId,
    pub prompt: String,
    /// Filled by the framework.
    #[serde(default = "default_level")]
    pub level: ApprovalLevel,
    /// The ring that raised it.
    #[serde(default = "default_ring")]
    pub ring: Ring,
    /// Rule(s) that produced it, for approval statistics.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Offer "allow this destination for the rest of the session".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember_destination: Option<String>,
}

fn default_level() -> ApprovalLevel {
    ApprovalLevel::Policy
}
fn default_ring() -> Ring {
    Ring::Human
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "verdict", content = "value", rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    /// The reason is returned to the model as the tool result.
    Deny(Reason),
    /// Rewrite call arguments or a result. Re-checked from ring 1.
    Rewrite(Proposal),
    /// Inject context (source annotated automatically).
    Annotate(Context),
    /// Hand to a human.
    Ask(Question),
    /// Stop hook only: ask the model to keep going.
    Continue(Reason),
    /// Suspend the session; re-evaluated on resume.
    Defer,
}

impl Verdict {
    pub fn ask(prompt: impl Into<String>) -> Verdict {
        Verdict::Ask(Question {
            id: QuestionId::default(),
            prompt: prompt.into(),
            level: ApprovalLevel::Policy,
            ring: Ring::Hook,
            rules: vec![],
            remember_destination: None,
        })
    }
    pub fn deny(reason: impl Into<String>) -> Verdict {
        Verdict::Deny(Reason(reason.into()))
    }
    /// Strictness order used to enforce "later rings only tighten":
    /// Allow/Annotate < Rewrite < Ask < Defer < Deny.
    pub fn strictness(&self) -> u8 {
        match self {
            Verdict::Allow | Verdict::Annotate(_) | Verdict::Continue(_) => 0,
            Verdict::Rewrite(_) => 1,
            Verdict::Ask(_) => 2,
            Verdict::Defer => 3,
            Verdict::Deny(_) => 4,
        }
    }
}

/// Who answered a gate / question. Recorded with every verdict; replay reads it
/// back instead of re-running hooks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Responder {
    Kernel,
    Policy(String),
    Budget,
    Hook(String),
    AutoRule(String),
    Human(String),
    /// Embedding code (`ask.allow()`); counts as the user in interactive mode.
    Code,
    /// Unattended fallback configured in the profile.
    Unattended,
    /// A framework-launched disposable environment answered an invariant ask.
    DisposableEnv,
}

/// The human answer to a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    Allow {
        /// Allow this destination for the rest of the session.
        #[serde(default)]
        remember: bool,
    },
    AllowWith(Proposal),
    Deny {
        #[serde(default)]
        reason: Option<String>,
    },
}
