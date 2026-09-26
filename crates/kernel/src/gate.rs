//! Rings 1–3 of the gate chain (built-in invariants, policy rules, budgets).
//! Pure functions of configuration data and kernel state; no user code.

use crate::state::State;
use agent_proto::*;
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

/// Compiled glob lists of a [`KernelConfig`].
#[derive(Debug, Default)]
pub(crate) struct Matchers {
    pub private: GlobSet,
    pub untrusted: GlobSet,
    pub trusted: GlobSet,
    pub egress: GlobSet,
    pub persistence: GlobSet,
    pub self_config: GlobSet,
    /// Per policy rule: (resource matcher, tool matcher).
    pub rules: Vec<(Option<GlobMatcher>, Option<GlobMatcher>)>,
}

fn glob(p: &str) -> Option<Glob> {
    GlobBuilder::new(p).literal_separator(false).backslash_escape(true).build().ok()
}

fn set(ps: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in ps {
        if let Some(g) = glob(p) {
            b.add(g);
        }
    }
    b.build().unwrap_or_default()
}

impl Matchers {
    pub fn compile(cfg: &KernelConfig) -> Matchers {
        let s = &cfg.security;
        Matchers {
            private: set(&s.private),
            untrusted: set(&s.untrusted),
            trusted: set(&s.trusted_sources),
            egress: set(&s.egress_allow),
            persistence: set(&s.persistence),
            self_config: set(&s.self_config),
            rules: cfg
                .rules
                .iter()
                .map(|r| {
                    (
                        r.resource.as_deref().and_then(glob).map(|g| g.compile_matcher()),
                        r.tool.as_deref().and_then(glob).map(|g| g.compile_matcher()),
                    )
                })
                .collect(),
        }
    }
}

fn is_write(a: &Access) -> bool {
    a.mode == AccessMode::Write
}

pub(crate) fn has_writes(c: &ToolCall) -> bool {
    c.access.iter().any(is_write)
}

/// Side-effecting = anything but a Pure call without declared writes.
pub(crate) fn side_effecting(c: &ToolCall) -> bool {
    c.class != EffectClass::Pure || has_writes(c)
}

fn in_workspace(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

/// Trust of content produced by a call, derived from its declared accesses.
pub(crate) fn access_trust(s: &State, call: &ToolCall) -> Trust {
    let Some(cfg) = s.config.as_ref() else { return Trust::Internal };
    let sec = &cfg.security;
    let m = s.m();
    let mut out = Trust::Internal;
    for a in &call.access {
        let uri = a.resource.as_str();
        let relevant = a.mode == AccessMode::Read
            || call.class == EffectClass::Opaque
            || a.resource.scheme() == Some(Scheme::Cmd);
        if !relevant || m.trusted.is_match(uri) {
            continue;
        }
        let label = if m.untrusted.is_match(uri) {
            Some(uri.to_string())
        } else if a.resource.scheme() == Some(Scheme::Fs) {
            if !in_workspace(&sec.workspace_root, a.resource.rest()) {
                Some(uri.to_string())
            } else if !sec.workspace_trusted {
                Some("workspace".to_string())
            } else {
                None
            }
        } else if a.resource.scheme() == Some(Scheme::Cmd) && !sec.workspace_trusted {
            Some("workspace".to_string())
        } else {
            None
        };
        if let Some(l) = label {
            out = Trust::weakest(&out, &Trust::Untrusted { source: l });
            break;
        }
    }
    out
}

/// Does the call read private data?
pub(crate) fn reads_private(s: &State, call: &ToolCall) -> bool {
    call.access.iter().any(|a| a.mode == AccessMode::Read && s.m().private.is_match(a.resource.as_str()))
}

/// Result of rings 1–3.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KernelVerdict {
    pub verdict: Verdict,
    pub ring: Ring,
    pub responder: Responder,
}

/// Canonical JSON (object keys sorted) used for repeated-call detection.
pub(crate) fn canonical(v: &serde_json::Value) -> String {
    fn go(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    out.push(':');
                    go(&m[*k], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    go(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap_or_default()),
        }
    }
    let mut s = String::new();
    go(v, &mut s);
    s
}

pub(crate) fn call_key(c: &ToolCall) -> String {
    format!("{}\u{0}{}", c.name, canonical(&c.input))
}

/// Maximum length (bytes) of the input summary in [`call_summary`].
const SUMMARY_BYTES: usize = 80;

/// `name input` with the canonical input JSON cut to a short prefix (used to
/// list irreversible operations in rewind reports).
pub(crate) fn call_summary(c: &ToolCall) -> String {
    let input = canonical(&c.input);
    if input.len() <= SUMMARY_BYTES {
        return format!("{} {input}", c.name);
    }
    let mut end = SUMMARY_BYTES;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} {}...", c.name, &input[..end])
}

/// A tool whose availability differs between the current sequence head and the
/// configuration in force after a mid-sequence update: `tool_removed` (still
/// defined in the head, removed from the configuration) or `tool_not_loaded`
/// (added by the configuration; its definition enters the next sequence head).
pub(crate) fn tool_mismatch(s: &State, name: &str) -> Option<&'static str> {
    let (Some(cfg), Some(head)) = (s.config.as_ref(), s.head.as_ref()) else { return None };
    let in_head = head.tools.iter().any(|t| t.name == name);
    let in_cfg = cfg.tools.iter().any(|t| t.name == name);
    match (in_head, in_cfg) {
        (true, false) => Some("tool_removed"),
        (false, true) => Some("tool_not_loaded"),
        _ => None,
    }
}

pub(crate) fn question_id(call: &CallId, rewrites: u32) -> QuestionId {
    QuestionId(format!("q:{}:{}", call, rewrites))
}

/// Ring 1: built-in invariants. Returns (rule names, destination to remember).
pub(crate) fn invariants(s: &State, call: &ToolCall) -> (Vec<String>, Option<String>) {
    let Some(cfg) = s.config.as_ref() else { return (vec![], None) };
    let sec = &cfg.security;
    let m = s.m();
    let tainted = s.taint.tainted;
    let private = s.taint.private_read || reads_private(s, call);
    let mut hits = Vec::new();
    let mut remember = None;
    let net_exit = call.access.iter().find(|a| {
        a.resource.scheme() == Some(Scheme::Net)
            && !m.egress.is_match(a.resource.as_str())
            && !s.destinations.contains(a.resource.as_str())
    });
    let opaque_net = call.class == EffectClass::Opaque && !sec.sandbox_available;
    if tainted && private && (net_exit.is_some() || opaque_net) {
        hits.push("invariant:exfiltration".to_string());
        remember = net_exit.map(|a| a.resource.as_str().to_string());
    }
    if tainted && call.access.iter().any(|a| is_write(a) && m.persistence.is_match(a.resource.as_str())) {
        hits.push("invariant:persistence".to_string());
    }
    if call.access.iter().any(|a| is_write(a) && m.self_config.is_match(a.resource.as_str())) {
        hits.push("invariant:self_modification".to_string());
    }
    if call.class == EffectClass::Opaque && !sec.isolation_available {
        hits.push("invariant:unknown_effect".to_string());
    }
    (hits, remember)
}

/// Ring 2: policy rules. Deny wins, then Ask, then Allow; unmatched accesses fall
/// back to Ask for side-effecting calls and Allow for pure ones.
pub(crate) fn policy(s: &State, call: &ToolCall) -> (PolicyAction, Vec<String>) {
    let Some(cfg) = s.config.as_ref() else { return (PolicyAction::Deny, vec!["no config".into()]) };
    if cfg.read_only_mode && side_effecting(call) {
        return (PolicyAction::Deny, vec!["read_only_mode".into()]);
    }
    if let Some(why) = tool_mismatch(s, &call.name) {
        return (PolicyAction::Deny, vec![why.into()]);
    }
    let mut deny = Vec::new();
    let mut ask = Vec::new();
    let mut allow = Vec::new();
    let mut covered = vec![false; call.access.len()];
    let mut all_covered_by_tool_rule = false;
    for (i, rule) in cfg.rules.iter().enumerate() {
        let (res_m, tool_m) = match s.m().rules.get(i) {
            Some(x) => x,
            None => continue,
        };
        // A glob that failed to compile never matches.
        if rule.tool.is_some() && tool_m.as_ref().map(|t| t.is_match(&call.name)) != Some(true) {
            continue;
        }
        if rule.resource.is_some() && res_m.is_none() {
            continue;
        }
        let mut record = |action: PolicyAction| {
            let v = match action {
                PolicyAction::Deny => &mut deny,
                PolicyAction::Ask => &mut ask,
                PolicyAction::Allow => &mut allow,
            };
            if !v.contains(&rule.name) {
                v.push(rule.name.clone());
            }
        };
        match res_m {
            None => {
                let mode_ok = match rule.mode {
                    None => true,
                    Some(md) => call.access.iter().any(|a| a.mode == md),
                };
                if mode_ok {
                    record(rule.action);
                    if rule.action == PolicyAction::Allow {
                        if rule.mode.is_none() {
                            all_covered_by_tool_rule = true;
                        } else {
                            for (j, a) in call.access.iter().enumerate() {
                                if Some(a.mode) == rule.mode {
                                    covered[j] = true;
                                }
                            }
                        }
                    }
                }
            }
            Some(g) => {
                for (j, a) in call.access.iter().enumerate() {
                    if rule.mode.map(|md| md == a.mode).unwrap_or(true) && g.is_match(a.resource.as_str()) {
                        record(rule.action);
                        if rule.action == PolicyAction::Allow {
                            covered[j] = true;
                        }
                    }
                }
            }
        }
    }
    if !deny.is_empty() {
        return (PolicyAction::Deny, deny);
    }
    if !ask.is_empty() {
        return (PolicyAction::Ask, ask);
    }
    let covered_all = all_covered_by_tool_rule || (!call.access.is_empty() && covered.iter().all(|c| *c));
    if covered_all {
        return (PolicyAction::Allow, allow);
    }
    if side_effecting(call) {
        (PolicyAction::Ask, vec!["default".into()])
    } else {
        (PolicyAction::Allow, vec!["default".into()])
    }
}

/// Ring 3: budgets. `ordinal`/`repeat` are the call's position in the turn and
/// its identical-call count (both 1-based, including itself).
pub(crate) fn budget(s: &State, at: Timestamp, ordinal: u32, repeat: u32) -> Option<String> {
    let cfg = s.config.as_ref()?;
    let b = &cfg.budgets;
    if b.max_tokens > 0 && s.tokens_used >= b.max_tokens {
        return Some(format!("token budget exhausted ({} tokens)", b.max_tokens));
    }
    if b.max_cost_micros > 0 && s.cost_used >= b.max_cost_micros {
        return Some("cost budget exhausted".into());
    }
    if b.max_calls_per_turn > 0 && ordinal > b.max_calls_per_turn {
        return Some(format!("tool call limit per turn ({}) reached", b.max_calls_per_turn));
    }
    if b.max_repeat_calls > 0 && repeat > b.max_repeat_calls {
        return Some(format!("identical call repeated more than {} times this turn", b.max_repeat_calls));
    }
    if b.max_turn_ms > 0 {
        if let Some(t) = &s.turn {
            if at.saturating_sub(t.started_at) > b.max_turn_ms {
                return Some(format!("turn time budget ({} ms) exhausted", b.max_turn_ms));
            }
        }
    }
    None
}

/// Rings 1–3 combined; later rings only tighten.
pub(crate) fn evaluate(s: &State, at: Timestamp, call: &ToolCall, ordinal: u32, repeat: u32, rewrites: u32) -> KernelVerdict {
    if rewrites > 3 {
        return KernelVerdict {
            verdict: Verdict::deny("rewrite depth exceeded (more than 3 rewrites)"),
            ring: Ring::Invariant,
            responder: Responder::Kernel,
        };
    }
    let qid = question_id(&call.id, rewrites);
    // ring 1
    let (hits, remember) = invariants(s, call);
    let mut cur = if hits.is_empty() {
        KernelVerdict { verdict: Verdict::Allow, ring: Ring::Invariant, responder: Responder::Kernel }
    } else {
        KernelVerdict {
            verdict: Verdict::Ask(Question {
                id: qid.clone(),
                prompt: format!("Allow `{}`? Triggered: {}", call.name, hits.join(", ")),
                level: ApprovalLevel::Invariant,
                ring: Ring::Invariant,
                rules: hits,
                remember_destination: remember,
            }),
            ring: Ring::Invariant,
            responder: Responder::Kernel,
        }
    };
    // ring 2
    let (action, rules) = policy(s, call);
    let rule_name = rules.first().cloned().unwrap_or_else(|| "default".into());
    match action {
        PolicyAction::Deny => {
            cur = KernelVerdict {
                verdict: Verdict::deny(format!("denied by policy ({})", rules.join(", "))),
                ring: Ring::Policy,
                responder: Responder::Policy(rule_name),
            };
        }
        PolicyAction::Ask => match &mut cur.verdict {
            Verdict::Ask(q) => {
                for r in rules {
                    if !q.rules.contains(&r) {
                        q.rules.push(r);
                    }
                }
            }
            _ => {
                cur = KernelVerdict {
                    verdict: Verdict::Ask(Question {
                        id: qid,
                        prompt: format!("Allow `{}`? Policy: {}", call.name, rules.join(", ")),
                        level: ApprovalLevel::Policy,
                        ring: Ring::Policy,
                        rules,
                        remember_destination: None,
                    }),
                    ring: Ring::Policy,
                    responder: Responder::Policy(rule_name),
                };
            }
        },
        PolicyAction::Allow => {
            if matches!(cur.verdict, Verdict::Allow) {
                cur.ring = Ring::Policy;
                cur.responder = Responder::Policy(rule_name);
            }
        }
    }
    // ring 3
    if !matches!(cur.verdict, Verdict::Deny(_)) {
        if let Some(why) = budget(s, at, ordinal, repeat) {
            cur = KernelVerdict { verdict: Verdict::deny(why), ring: Ring::Budget, responder: Responder::Budget };
        }
    }
    cur
}

/// "Only tighten": combine an earlier verdict with a later ring's verdict.
/// Never returns something less strict than `earlier`.
pub fn tighten(earlier: &Verdict, later: &Verdict) -> Verdict {
    if later.strictness() >= earlier.strictness() {
        later.clone()
    } else {
        earlier.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_uri_forms() {
        let g = set(&["fs:///**/.env*".into(), "net:*".into(), "cmd:cargo test*".into()]);
        assert!(g.is_match("fs:///home/u/proj/.env"));
        assert!(g.is_match("fs:///w/.env.local"));
        assert!(g.is_match("net:api.github.com:443"));
        assert!(g.is_match("cmd:cargo test -p a/b"));
        assert!(!g.is_match("fs:///w/src/main.rs"));
        let r = set(&["fs:///repo/src/**".into()]);
        assert!(r.is_match("fs:///repo/src/a/b.rs"));
        assert!(!r.is_match("fs:///repo/other.rs"));
    }

    #[test]
    fn canonical_sorts_keys() {
        let a = serde_json::json!({"b":1,"a":{"d":2,"c":[1,{"z":0,"y":1}]}});
        assert_eq!(canonical(&a), r#"{"a":{"c":[1,{"y":1,"z":0}],"d":2},"b":1}"#);
    }

    #[test]
    fn tighten_never_loosens() {
        let all = [Verdict::Allow, Verdict::deny("x"), Verdict::ask("q"), Verdict::Defer];
        for a in &all {
            for b in &all {
                assert!(tighten(a, b).strictness() >= a.strictness());
            }
        }
    }
}
