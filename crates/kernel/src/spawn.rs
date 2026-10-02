//! Session starts beyond the plain one, and the events that tie a sub-agent to
//! its parent.
//!
//! - [`start_session_with`]: a new session that may be a sub-agent child
//!   (`parent_session` set, the parent's taint inherited, optionally forked
//!   from the parent's completed turns) and may load long-term memory into
//!   the Durable layer.
//! - Parent side: `SubagentStarted` when a sub-agent call is dispatched,
//!   `SubagentFinished` when its result is recorded (outcome and usage come
//!   from the result's [`SubagentReport`]).
//! - Subdirectory instructions: files found by tools are journaled as pending
//!   and injected at the next step within the instruction byte budget.
//!
//! Everything here is pure: the runtime / SDK read memory, the parent's state
//! and instruction files and hand them in as data.

use crate::context::{Entry, EntryKind};
use crate::decide::{fnv, static_update_notice, user_draft, Cx};
use crate::state::*;
use crate::{Decision, State};
use agent_proto::*;
use std::sync::Arc;

/// How a new session starts (see [`start_session_with`]).
#[derive(Debug, Clone, Default)]
pub struct SessionStart {
    /// The spawning session, for sub-agents.
    pub parent: Option<SessionId>,
    /// Long-term memory loaded at session start (Durable layer). Ignored for
    /// forks: the inherited history already carries the parent's.
    pub memory: Option<String>,
    /// Taint inherited from the parent (see [`inherited_taint`]).
    pub taint: Option<Taint>,
    /// Seed a fork from the parent's completed turns (see [`fork_seed`]).
    pub fork: Option<ForkSeed>,
}

/// The parent's sequence head and the rendered fragments of its completed
/// turns, taken when a fork is spawned. Opaque.
#[derive(Debug, Clone)]
pub struct ForkSeed {
    head: SeqHead,
    entries: Vec<Entry>,
}

impl ForkSeed {
    /// Number of inherited context fragments.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The taint a child inherits from `s`, if any.
pub fn inherited_taint(s: &State) -> Option<Taint> {
    (s.taint.tainted || s.taint.private_read).then(|| s.taint.clone())
}

/// What a fork of `s` starts from: the current sequence head and the context
/// fragments of the completed turns (everything before the running turn).
pub fn fork_seed(s: &State) -> Option<ForkSeed> {
    let head = (**s.head.as_ref()?).clone();
    let until = s.turn.as_ref().map(|t| t.start_seq);
    let entries = s.context.iter().filter(|e| until.is_none_or(|u| e.seq < u)).cloned().collect();
    Some(ForkSeed { head, entries })
}

/// Tokens and cost (micro-dollars) consumed by the session so far, sub-agents
/// included (their usage is charged when their result is recorded).
pub fn usage(s: &State) -> (u64, u64) {
    (s.tokens_used, s.cost_used)
}

/// Budgets a child may use: the parent's remaining token / cost budget is
/// carved out for it (0 left of a limited budget is reported as `None`: no
/// child can run), per-turn limits can only be narrower than the parent's.
pub fn child_budgets(s: &State, child: &Budgets) -> Option<Budgets> {
    let parent = &s.config.as_ref()?.budgets;
    let carve = |limit: u64, used: u64, own: u64| -> Option<u64> {
        if limit == 0 {
            return Some(own);
        }
        let left = limit.saturating_sub(used);
        if left == 0 {
            return None;
        }
        Some(if own == 0 { left } else { own.min(left) })
    };
    let narrow = |p: u64, c: u64| match (p, c) {
        (0, c) => c,
        (p, 0) => p,
        (p, c) => p.min(c),
    };
    Some(Budgets {
        max_tokens: carve(parent.max_tokens, s.tokens_used, child.max_tokens)?,
        max_cost_micros: carve(parent.max_cost_micros, s.cost_used, child.max_cost_micros)?,
        max_turn_ms: narrow(parent.max_turn_ms, child.max_turn_ms),
        max_calls_per_turn: narrow(parent.max_calls_per_turn as u64, child.max_calls_per_turn as u64) as u32,
        max_repeat_calls: narrow(parent.max_repeat_calls as u64, child.max_repeat_calls as u64) as u32,
        max_continuations: parent.max_continuations.min(child.max_continuations),
    })
}

/// Drafts for a new session: `SessionStarted` (with `parent_session`), the
/// inherited taint, the first `SequenceOpened` (a fork reuses the parent's
/// head and inherited fragments verbatim, then appends the prompt / tool
/// differences as a mid-sequence update, or opens a new sequence when the model
/// cannot take one), and `MemoryLoaded`.
pub fn start_session_with(session: SessionId, profile_hash: String, config: KernelConfig, start: SessionStart) -> Decision {
    let mut cx = Cx::new(State::default(), Timestamp(0));
    cx.emit(Draft {
        origin: Origin::System,
        ..user_draft(Event::SessionStarted {
            session,
            profile_hash,
            config: config.clone(),
            parent_session: start.parent.clone(),
        })
    });
    if let Some(t) = &start.taint {
        let data = serde_json::json!({
            "tainted": t.tainted,
            "labels": t.labels.iter().collect::<Vec<_>>(),
            "private_read": t.private_read,
        });
        let source = start.parent.as_ref().map(|p| format!("session:{p}")).unwrap_or_else(|| "parent".into());
        cx.emit(Draft {
            origin: Origin::Session(source),
            ..Draft::internal(Event::Plugin { kind: TAINT_INHERITED_KIND.into(), ignorable: false, data })
        });
    }
    match start.fork {
        None => {
            cx.open_sequence(false);
            if let Some(text) = start.memory.filter(|m| !m.trim().is_empty()) {
                cx.visible(Origin::System, Trust::Guidance, Event::MemoryLoaded { text });
            }
        }
        Some(seed) => fork(&mut cx, seed, &config),
    }
    cx.out
}

fn fork(cx: &mut Cx, seed: ForkSeed, config: &KernelConfig) {
    let head = seed.head;
    cx.internal(Event::SequenceOpened { head: head.clone() });
    for e in seed.entries {
        let data = serde_json::json!({ "kind": e.kind, "untrusted": e.untrusted });
        cx.emit(Draft {
            parent: Parent::Head,
            origin: Origin::System,
            trust: Trust::Internal,
            audience: Audience::Model,
            body: Event::Plugin { kind: FORK_ENTRY_KIND.into(), ignorable: false, data },
            rendered: Some((*e.rendered).clone()),
        });
    }
    let caps = &config.caps;
    let model_changed = caps.model != head.model || caps.render != head.render;
    let enc_changed = config.encoder_version != head.encoder_version;
    let static_changed = config.system != head.system || config.tools != head.tools;
    if model_changed || enc_changed {
        cx.open_sequence(model_changed);
    } else if static_changed {
        if caps.mid_sequence_updates {
            let old = KernelConfig { system: head.system.clone(), tools: head.tools.clone(), ..config.clone() };
            if let Some(text) = static_update_notice(&old, config, &head) {
                cx.visible(Origin::System, Trust::Guidance, Event::Injected { source: "system-update".into(), text });
            }
        } else {
            cx.open_sequence(false);
        }
    }
}

/// Evolve: a `FORK_ENTRY_KIND` event becomes a context entry with the
/// inherited rendering.
pub(crate) fn fork_entry(ev: &Envelope<Event>, data: &serde_json::Value) -> Option<Entry> {
    let rendered = ev.rendered.clone()?;
    let kind: EntryKind = data.get("kind").and_then(|k| serde_json::from_value(k.clone()).ok()).unwrap_or(EntryKind::Other);
    let untrusted: Vec<String> =
        data.get("untrusted").and_then(|u| serde_json::from_value(u.clone()).ok()).unwrap_or_default();
    Some(Entry {
        seq: ev.seq,
        id: ev.id.clone(),
        kind,
        rendered: Arc::new(rendered),
        untrusted,
        source: None,
        trimmed: false,
        erased: false,
        at: ev.at,
    })
}

/// Evolve: the parent's taint at spawn time.
pub(crate) fn on_taint_inherited(s: &mut State, id: &EventId, data: &serde_json::Value) {
    if data.get("tainted").and_then(|v| v.as_bool()).unwrap_or(false) {
        let labels: Vec<String> =
            data.get("labels").and_then(|l| serde_json::from_value(l.clone()).ok()).unwrap_or_default();
        if labels.is_empty() {
            taint(s, id, "parent");
        }
        for l in labels {
            taint(s, id, &l);
        }
    }
    if data.get("private_read").and_then(|v| v.as_bool()).unwrap_or(false) {
        s.taint.private_read = true;
    }
}

impl Cx {
    fn is_subagent(&self, call: &ToolCall) -> bool {
        self.cfg().tools.iter().any(|t| t.name == call.name && t.subagent)
    }

    /// A batch with sub-agent calls was dispatched: record their start. The
    /// child session id is derived from the call, so recovery finds it again.
    pub(crate) fn subagents_started(&mut self, calls: &[ToolCall]) {
        let Some(session) = self.s.session.clone() else { return };
        for c in calls {
            if self.is_subagent(c) && !self.s.children.contains_key(&c.id) {
                self.user(Event::SubagentStarted { call: c.id.clone(), child: session.child(&c.id) });
            }
        }
    }

    /// `SubagentFinished` for a sub-agent call whose result is being recorded:
    /// the child's reported outcome, or `Interrupted` when the call was
    /// cancelled. A result without a report (a background sub-agent still
    /// running) ends nothing.
    pub(crate) fn subagent_ended(&self, call: &ToolCall, result: &ToolResult, executed: bool) -> Option<Event> {
        let started = self.s.children.get(&call.id).filter(|c| c.outcome.is_none())?;
        let outcome = match (&result.subagent, executed) {
            (Some(r), _) => r.outcome.clone(),
            (None, false) => TurnOutcome::Interrupted,
            (None, true) => return None,
        };
        Some(Event::SubagentFinished { call: call.id.clone(), child: started.child.clone(), outcome })
    }

    /// Instruction files a tool found: new or changed ones become pending.
    pub(crate) fn instructions_found(&mut self, found: Vec<FoundInstructions>) {
        for f in found {
            let hash = fnv(&f.text);
            let known = self.s.instr.get(&f.path).is_some_and(|i| i.hash == hash);
            let pending = self.s.instr_pending.get(&f.path).is_some_and(|t| *t == f.text);
            if known || pending {
                continue;
            }
            let data = serde_json::json!({ "path": f.path, "text": f.text });
            self.internal(Event::Plugin { kind: INSTRUCTIONS_PENDING_KIND.into(), ignorable: true, data });
        }
    }

    /// Safe point: inject pending instruction files (broadest first omitted,
    /// then the most specific truncated, to stay within the byte budget) and
    /// re-inject the ones compaction removed from the context.
    pub(crate) fn inject_instructions(&mut self) {
        if self.s.instr.is_empty() && self.s.instr_pending.is_empty() {
            return;
        }
        let trust = |cx: &Cx, path: &str| {
            if cx.cfg().security.workspace_trusted {
                Trust::Guidance
            } else {
                Trust::Untrusted { source: format!("fs://{path}") }
            }
        };
        let in_context = |s: &State, id: &EventId| s.context.iter().any(|e| &e.id == id);
        let lost: Vec<(String, String)> = self
            .s
            .instr
            .iter()
            .filter(|(p, _)| !self.s.instr_pending.contains_key(*p))
            .filter(|(_, i)| i.event.as_ref().is_some_and(|id| !in_context(&self.s, id)))
            .map(|(p, i)| (p.clone(), i.text.clone()))
            .collect();
        for (path, text) in lost {
            let t = trust(self, &path);
            self.visible(Origin::System, t, Event::InstructionsInjected { path, text });
        }
        if self.s.instr_pending.is_empty() {
            return;
        }
        let budget = self.cfg().instruction_budget as usize;
        let used: usize = self
            .s
            .instr
            .iter()
            .filter(|(p, _)| !self.s.instr_pending.contains_key(*p))
            .filter(|(_, i)| i.event.as_ref().is_some_and(|id| in_context(&self.s, id)))
            .map(|(_, i)| i.text.len())
            .sum();
        let mut left = budget.saturating_sub(used);
        // Broadest (shortest directory chain) first.
        let mut pending: Vec<(String, String)> = self.s.instr_pending.clone().into_iter().collect();
        pending.sort_by_key(|(p, _)| (p.matches('/').count(), p.clone()));
        let mut total: usize = pending.iter().map(|(_, t)| t.len()).sum();
        let mut omitted = Vec::new();
        while total > left && pending.len() > 1 {
            let (p, t) = pending.remove(0);
            total -= t.len();
            omitted.push((p, t));
        }
        for (path, text) in omitted {
            let data = serde_json::json!({ "path": path, "hash": fnv(&text) });
            self.internal(Event::Plugin { kind: INSTRUCTIONS_OMITTED_KIND.into(), ignorable: true, data });
        }
        for (path, mut text) in pending {
            if text.len() > left {
                let mut cut = left;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                if text.is_empty() {
                    let data = serde_json::json!({ "path": path, "hash": fnv(&self.s.instr_pending[&path]) });
                    self.internal(Event::Plugin { kind: INSTRUCTIONS_OMITTED_KIND.into(), ignorable: true, data });
                    continue;
                }
            }
            left = left.saturating_sub(text.len());
            let t = trust(self, &path);
            self.visible(Origin::System, t, Event::InstructionsInjected { path, text });
        }
    }
}
