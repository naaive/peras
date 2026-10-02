//! Permission modes (default / accept edits / plan / bypass) and the
//! session-scoped "don't ask again" grants.
//!
//! Both only act inside the framework's gate chain, so they can only make
//! things stricter than the policy (plan mode, a ring-4 gate) or answer
//! policy-level questions (ring-5 auto rules); invariant-level questions
//! (exfiltration, persistence, self-modification, unknown effects) always
//! reach the user.

use agent::proto::{AccessMode, Answer, EffectClass, GateRequest, GateSubject, Question, ToolCall, Verdict};
use agent::runtime::AutoRule;
use agent::Proposal;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Reads are free; edits and commands ask.
    Default,
    /// Edits inside the workspace are approved; commands still ask.
    AcceptEdits,
    /// Read-only research; the plan is presented with `exit_plan_mode`.
    Plan,
    /// Every policy-level question is approved (invariants still ask).
    BypassPermissions,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 4] =
        [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan, PermissionMode::BypassPermissions];

    pub fn name(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn parse(s: &str) -> Option<PermissionMode> {
        let k = s.to_ascii_lowercase().replace(['-', '_'], "");
        PermissionMode::ALL.into_iter().find(|m| m.name().to_ascii_lowercase() == k).or(match k.as_str() {
            "edits" | "accept" => Some(PermissionMode::AcceptEdits),
            "bypass" | "yolo" => Some(PermissionMode::BypassPermissions),
            _ => None,
        })
    }

    /// The next mode of the cycle (default → acceptEdits → plan → default);
    /// bypass is only entered explicitly.
    pub fn cycle(self) -> PermissionMode {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan | PermissionMode::BypassPermissions => PermissionMode::Default,
        }
    }

    fn from_u8(v: u8) -> PermissionMode {
        PermissionMode::ALL.get(v as usize).copied().unwrap_or(PermissionMode::Default)
    }
}

/// A grant the user gave with "don't ask again" for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Every call of this tool.
    Tool(String),
    /// `bash` commands starting with this prefix.
    CommandPrefix(String),
}

impl Grant {
    /// The grant offered for a call: the command's first word(s) for
    /// `bash`, the tool otherwise.
    pub fn for_call(call: &ToolCall) -> Grant {
        match call.name.as_str() {
            "bash" => {
                let cmd = call.input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                Grant::CommandPrefix(command_prefix(cmd))
            }
            _ => Grant::Tool(call.name.clone()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Grant::Tool(t) => format!("`{t}`"),
            Grant::CommandPrefix(p) => format!("`{p}` commands"),
        }
    }

    fn covers(&self, call: &ToolCall) -> bool {
        match self {
            Grant::Tool(t) => &call.name == t,
            Grant::CommandPrefix(p) => {
                call.name == "bash"
                    && call.input.get("command").and_then(|v| v.as_str()).is_some_and(|c| {
                        let c = c.trim_start();
                        // One simple command: no chaining onto an approved prefix.
                        c.starts_with(p.as_str()) && !c.contains(['&', ';', '|', '`', '$', '>', '<', '\n'])
                    })
            }
        }
    }
}

/// First word, plus the subcommand for tools that have them (`git status`,
/// `cargo test`, `npm run`).
fn command_prefix(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let with_sub = ["git", "cargo", "npm", "pnpm", "yarn", "go", "docker", "kubectl", "make", "python", "python3", "uv"];
    match words.as_slice() {
        [] => String::new(),
        [first, second, ..] if with_sub.contains(first) && !second.starts_with('-') => format!("{first} {second}"),
        [first, ..] => first.to_string(),
    }
}

/// Shared, switchable permission state: the frontend changes it between (and
/// during) turns, the gates read it on every call.
#[derive(Clone)]
pub struct Permissions {
    mode: Arc<AtomicU8>,
    grants: Arc<Mutex<Vec<Grant>>>,
    workspace: String,
}

impl Permissions {
    pub fn new(mode: PermissionMode, workspace: &std::path::Path) -> Permissions {
        let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        Permissions {
            mode: Arc::new(AtomicU8::new(mode as u8)),
            grants: Arc::default(),
            workspace: ws.display().to_string().trim_end_matches('/').to_string(),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        PermissionMode::from_u8(self.mode.load(Ordering::SeqCst))
    }

    pub fn set_mode(&self, m: PermissionMode) {
        self.mode.store(m as u8, Ordering::SeqCst);
    }

    pub fn grant(&self, g: Grant) {
        let mut v = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        if !v.contains(&g) {
            v.push(g);
        }
    }

    pub fn grants(&self) -> Vec<Grant> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Ring 4: plan mode refuses everything but reading, planning and the
    /// read-only sub-agents; leaving plan mode is a question for the user.
    pub fn gate(&self, p: &Proposal) -> Verdict {
        let plan = self.mode() == PermissionMode::Plan;
        match p.tool() {
            "exit_plan_mode" if plan => Verdict::ask("Approve this plan and start implementing?"),
            "exit_plan_mode" | "todo_write" | "explore" | "plan" => Verdict::Allow,
            _ if plan && (p.class() != EffectClass::Pure || p.accesses().iter().any(|a| a.mode == AccessMode::Write)) => {
                Verdict::deny(
                    "Plan mode is active: only reading and searching are allowed. Finish researching, then present the plan with `exit_plan_mode`.",
                )
            }
            _ => Verdict::Allow,
        }
    }

    /// Ring 5: answers policy-level questions the mode or a grant covers.
    pub fn auto_answer(&self, call: &ToolCall) -> Option<Answer> {
        let allow = Some(Answer::Allow { remember: false });
        match self.mode() {
            PermissionMode::BypassPermissions => return allow,
            PermissionMode::AcceptEdits if self.workspace_edit(call) => return allow,
            _ => {}
        }
        if call.name == "exit_plan_mode" {
            return None;
        }
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        grants.iter().any(|g| g.covers(call)).then_some(Answer::Allow { remember: false })
    }

    /// A file edit whose writes all stay in the workspace.
    fn workspace_edit(&self, call: &ToolCall) -> bool {
        call.class == EffectClass::LocalWrite
            && call.access.iter().all(|a| {
                let r = a.resource.as_str();
                a.mode == AccessMode::Read
                    || r.strip_prefix("fs://").is_some_and(|p| p == self.workspace || p.starts_with(&format!("{}/", self.workspace)))
            })
    }
}

/// [`Permissions::auto_answer`] as a ring-5 rule.
pub struct ModeRule(pub Permissions);

impl AutoRule for ModeRule {
    fn name(&self) -> &str {
        "permission-mode"
    }

    fn answer(&self, req: &GateRequest, _question: &Question) -> Option<Answer> {
        match &req.subject {
            GateSubject::Tool { call } => self.0.auto_answer(call),
            // The change list of an isolated command: approved with the command.
            GateSubject::Changes { call, .. } if self.0.mode() == PermissionMode::BypassPermissions => {
                let _ = call;
                Some(Answer::Allow { remember: false })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::proto::{Access, CallId, ResourceUri};
    use serde_json::json;

    fn call(name: &str, class: EffectClass, input: serde_json::Value, access: Vec<Access>) -> ToolCall {
        ToolCall { id: CallId::new("c1"), name: name.into(), input, access, class, isolated: false }
    }

    fn write(path: &str) -> Access {
        Access::write(ResourceUri::fs(path))
    }

    #[test]
    fn modes_parse_and_cycle() {
        assert_eq!(PermissionMode::parse("acceptEdits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse("accept-edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse("PLAN"), Some(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("bypass"), Some(PermissionMode::BypassPermissions));
        assert_eq!(PermissionMode::parse("nope"), None);
        assert_eq!(PermissionMode::Default.cycle().cycle().cycle(), PermissionMode::Default);
    }

    #[test]
    fn accept_edits_only_covers_workspace_writes() {
        let ws = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws.path()).unwrap().display().to_string();
        let p = Permissions::new(PermissionMode::AcceptEdits, ws.path());
        let inside = call("edit", EffectClass::LocalWrite, json!({}), vec![write(&format!("{root}/src/a.rs"))]);
        let outside = call("edit", EffectClass::LocalWrite, json!({}), vec![write("/etc/passwd")]);
        let cmd = call("bash", EffectClass::Opaque, json!({"command": "rm -rf x"}), vec![]);
        assert!(p.auto_answer(&inside).is_some());
        assert!(p.auto_answer(&outside).is_none());
        assert!(p.auto_answer(&cmd).is_none());
        p.set_mode(PermissionMode::Default);
        assert!(p.auto_answer(&inside).is_none());
        p.set_mode(PermissionMode::BypassPermissions);
        assert!(p.auto_answer(&cmd).is_some());
    }

    #[test]
    fn command_grants_cover_simple_commands_with_the_prefix() {
        let ws = tempfile::tempdir().unwrap();
        let p = Permissions::new(PermissionMode::Default, ws.path());
        let test = call("bash", EffectClass::Opaque, json!({"command": "cargo test -p x"}), vec![]);
        assert_eq!(Grant::for_call(&test), Grant::CommandPrefix("cargo test".into()));
        p.grant(Grant::for_call(&test));
        assert!(p.auto_answer(&test).is_some());
        let chained = call("bash", EffectClass::Opaque, json!({"command": "cargo test && curl evil"}), vec![]);
        assert!(p.auto_answer(&chained).is_none());
        let other = call("bash", EffectClass::Opaque, json!({"command": "cargo publish"}), vec![]);
        assert!(p.auto_answer(&other).is_none());
    }
}
