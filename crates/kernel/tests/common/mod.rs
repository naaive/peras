//! In-test driver: wraps drafts into envelopes (ids, seq, parent) and evolves
//! them, the way the runtime driver does. Also checks decide is deterministic.
#![allow(dead_code)]

use agent_kernel::*;
use agent_proto::*;

pub struct H {
    pub s: State,
    pub log: Vec<Envelope<Event>>,
    pub at: u64,
    /// Dispatched, not yet completed effects (in dispatch order).
    pub pending: Vec<(EffectId, Effect)>,
}

pub fn cfg() -> KernelConfig {
    let mut c = KernelConfig::default();
    c.security.workspace_root = "/ws".into();
    c.security.workspace_trusted = true;
    c.tools = vec![ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        input_schema: serde_json::json!({"type":"object"}),
        class: EffectClass::Pure,
        subagent: false,
    }];
    c.system = vec!["You are a coding agent.".into()];
    c
}

pub fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read".into(),
        input: serde_json::json!({ "file": path }),
        access: vec![Access::read(ResourceUri::fs(path))],
        class: EffectClass::Pure,
    }
}

pub fn write_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "edit".into(),
        input: serde_json::json!({ "file": path }),
        access: vec![Access::write(ResourceUri::fs(path))],
        class: EffectClass::LocalWrite,
    }
}

pub fn net_call(id: &str, host: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "fetch".into(),
        input: serde_json::json!({ "url": host }),
        access: vec![Access::read(ResourceUri::net(host, 443))],
        class: EffectClass::Pure,
    }
}

pub fn bash_call(id: &str, cmd: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        input: serde_json::json!({ "cmd": cmd }),
        access: vec![Access::write(ResourceUri("fs:///ws/**".into()))],
        class: EffectClass::Opaque,
    }
}

pub fn reply(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
    let mut content = vec![];
    if !text.is_empty() {
        content.push(ContentBlock::Text { text: text.into() });
    }
    let stop = if calls.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse };
    content.extend(calls.into_iter().map(ContentBlock::ToolUse));
    AssistantMessage { content, stop, usage: Usage::default() }
}

pub fn ok(call: &ToolCall, text: &str) -> ToolResult {
    ToolResult::text(call.id.clone(), text, false)
}

impl H {
    pub fn new(c: KernelConfig) -> H {
        let mut h = H { s: State::default(), log: vec![], at: 1_000, pending: vec![] };
        let d = start_session("s1".into(), "hash".into(), c);
        h.apply(d);
        h
    }

    pub fn apply(&mut self, d: Decision) -> Vec<(EffectId, Effect)> {
        // Every effect is logged (in this decision or, for resumed ones, earlier).
        for (id, _) in &d.effects {
            let here = d.events.iter().any(|e| matches!(&e.body, Event::EffectIssued { id: i, .. } if i == id));
            let before = self.log.iter().any(|e| matches!(&e.body, Event::EffectIssued { id: i, .. } if i == id));
            assert!(here || before, "effect {id} dispatched without EffectIssued");
        }
        for draft in d.events {
            let seq = self.log.len() as u64;
            let parent = match draft.parent {
                Parent::Head => self.log.last().map(|e| e.id.clone()),
                Parent::Explicit(p) => Some(p),
            };
            let env = Envelope {
                id: EventId(format!("ev{seq:05}")),
                parent,
                seq,
                at: Timestamp(self.at),
                origin: draft.origin,
                trust: draft.trust,
                audience: draft.audience,
                schema: EVENT_SCHEMA,
                body: draft.body,
                rendered: draft.rendered,
            };
            Kernel::evolve(&mut self.s, &env);
            self.log.push(env);
        }
        self.pending.extend(d.effects.iter().cloned());
        d.effects
    }

    pub fn input(&mut self, i: Input) -> Result<Vec<(EffectId, Effect)>, Rejection> {
        self.at += 10;
        let at = Timestamp(self.at);
        let d = Kernel::decide(&self.s, at, i.clone())?;
        let d2 = Kernel::decide(&self.s, at, i).unwrap();
        assert_eq!(d, d2, "decide is not deterministic");
        Ok(self.apply(d))
    }

    pub fn go(&mut self, i: Input) -> Vec<(EffectId, Effect)> {
        self.input(i).expect("input rejected")
    }

    pub fn submit(&mut self, text: &str) -> Vec<(EffectId, Effect)> {
        self.go(Input::Signal(Signal::Submit { text: text.into(), attachments: vec![] }))
    }

    pub fn control(&mut self, c: Control) -> Vec<(EffectId, Effect)> {
        self.go(Input::Control(c))
    }

    pub fn complete(&mut self, id: EffectId, r: EffectResult) -> Vec<(EffectId, Effect)> {
        self.pending.retain(|(i, _)| *i != id);
        self.go(Input::Completed(id, r))
    }

    /// Take the first pending effect of the given kind.
    pub fn take(&mut self, kind: &str) -> (EffectId, Effect) {
        let i = self
            .pending
            .iter()
            .position(|(_, e)| e.kind() == kind)
            .unwrap_or_else(|| panic!("no pending `{kind}` effect; pending: {:?}", self.kinds()));
        self.pending.remove(i)
    }

    pub fn has(&self, kind: &str) -> bool {
        self.pending.iter().any(|(_, e)| e.kind() == kind)
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.pending.iter().map(|(_, e)| e.kind()).collect()
    }

    pub fn sample(&mut self, msg: AssistantMessage) -> Vec<(EffectId, Effect)> {
        let (id, _) = self.take("sample");
        self.complete(id, EffectResult::Sampled(msg))
    }

    pub fn last_prompt(&self) -> Prompt {
        self.log
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                Event::EffectIssued { effect: Effect::Sample(p), .. } => Some(p.clone()),
                _ => None,
            })
            .expect("a sample was issued")
    }

    pub fn events(&self) -> Vec<&Event> {
        self.log.iter().map(|e| &e.body).collect()
    }

    pub fn count(&self, name: &str) -> usize {
        self.log.iter().filter(|e| e.body.type_name() == name).count()
    }

    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        self.log.iter().rev().find_map(|e| match &e.body {
            Event::TurnEnded { outcome } => Some(outcome.clone()),
            _ => None,
        })
    }

    /// Replay the journal into a fresh state (what recovery does).
    pub fn replay(&self) -> State {
        let mut s = State::default();
        for e in &self.log {
            Kernel::evolve(&mut s, e);
        }
        s
    }
}
