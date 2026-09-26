//! A pure simulation harness driving a [`Decider`] without the runtime.

use crate::clock::{SeqIds, VirtualClock};
use crate::sched::Scheduler;
use agent_kernel::{Decider, Rejection};
use agent_proto::*;
use std::marker::PhantomData;

/// The outside world as the simulation sees it: resolves effects.
///
/// Returning `None` leaves the effect pending (the world is not ready; another
/// pending effect is tried instead). `Effect::Finish` is special: it is always
/// removed after `resolve` (its outcome is recorded by the sim) and only fed
/// back to the decider if the world returns `Some`.
pub trait World {
    fn resolve(&mut self, id: EffectId, effect: &Effect) -> Option<EffectResult>;
}

impl<W: World + ?Sized> World for &mut W {
    fn resolve(&mut self, id: EffectId, effect: &Effect) -> Option<EffectResult> {
        (**self).resolve(id, effect)
    }
}

/// Result of one [`KernelSim::step_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The effect's result was fed back to the decider.
    Delivered(EffectId),
    /// A `Finish` effect was acknowledged.
    Finished(EffectId),
    /// Effects are pending but the world resolved none of them.
    Stalled,
    /// Nothing pending.
    Idle,
    /// `run_until_idle` hit its step limit (likely a livelock).
    LimitReached,
}

pub struct KernelSim<D: Decider> {
    state: D::State,
    journal: Vec<Envelope<Event>>,
    pending: Vec<(EffectId, Effect)>,
    head: Option<EventId>,
    clock: VirtualClock,
    ids: SeqIds,
    sched: Scheduler,
    tick_ms: u64,
    max_steps: usize,
    boundaries: usize,
    rejections: Vec<Rejection>,
    outcomes: Vec<(EffectId, TurnOutcome)>,
    crashes: usize,
    _d: PhantomData<fn() -> D>,
}

impl<D: Decider> Default for KernelSim<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: Decider> KernelSim<D> {
    pub fn new() -> Self {
        KernelSim {
            state: D::State::default(),
            journal: vec![],
            pending: vec![],
            head: None,
            clock: VirtualClock::new(0),
            ids: SeqIds::new(),
            sched: Scheduler::fifo(),
            tick_ms: 0,
            max_steps: 10_000,
            boundaries: 0,
            rejections: vec![],
            outcomes: vec![],
            crashes: 0,
            _d: PhantomData,
        }
    }

    pub fn with_scheduler(mut self, s: Scheduler) -> Self {
        self.sched = s;
        self
    }
    pub fn with_clock(mut self, c: VirtualClock) -> Self {
        self.clock = c;
        self
    }
    pub fn with_ids(mut self, ids: SeqIds) -> Self {
        self.ids = ids;
        self
    }
    /// Advance the virtual clock by `ms` before every input.
    pub fn with_tick(mut self, ms: u64) -> Self {
        self.tick_ms = ms;
        self
    }
    pub fn with_max_steps(mut self, n: usize) -> Self {
        self.max_steps = n;
        self
    }

    pub fn state(&self) -> &D::State {
        &self.state
    }
    pub fn journal(&self) -> &[Envelope<Event>] {
        &self.journal
    }
    pub fn pending(&self) -> &[(EffectId, Effect)] {
        &self.pending
    }
    pub fn clock(&self) -> &VirtualClock {
        &self.clock
    }
    pub fn ids(&self) -> &SeqIds {
        &self.ids
    }
    /// Inputs the decider refused while stepping.
    pub fn rejections(&self) -> &[Rejection] {
        &self.rejections
    }
    /// `Finish` outcomes seen, deduplicated by effect id (survives crashes).
    pub fn outcomes(&self) -> &[(EffectId, TurnOutcome)] {
        &self.outcomes
    }
    /// Number of effect boundaries passed (effects settled by the world).
    pub fn boundaries(&self) -> usize {
        self.boundaries
    }
    pub fn crashes(&self) -> usize {
        self.crashes
    }

    /// Feed one input: decide -> append (ids, seq, time, parent) -> evolve ->
    /// queue the effects. Nothing is written on rejection. Returns the ids of
    /// newly queued effects.
    pub fn apply(&mut self, input: Input) -> Result<Vec<EffectId>, Rejection> {
        if self.tick_ms > 0 {
            self.clock.advance(self.tick_ms);
        }
        let at = self.clock.now();
        let decision = D::decide(&self.state, at, input)?;
        Ok(self.apply_at(at, decision))
    }

    /// Apply a decision made outside `decide` (e.g. the kernel's
    /// `start_session`). Returns the ids of newly queued effects.
    pub fn apply_decision(&mut self, decision: agent_kernel::Decision) -> Vec<EffectId> {
        let at = self.clock.now();
        self.apply_at(at, decision)
    }

    fn apply_at(&mut self, at: Timestamp, decision: agent_kernel::Decision) -> Vec<EffectId> {
        for d in decision.events {
            let id = self.ids.next_id();
            let parent = match d.parent {
                Parent::Head => self.head.clone(),
                Parent::Explicit(p) => Some(p),
            };
            let env = Envelope {
                id: id.clone(),
                parent,
                seq: self.journal.len() as Seq,
                at,
                origin: d.origin,
                trust: d.trust,
                audience: d.audience,
                schema: EVENT_SCHEMA,
                body: d.body,
                rendered: d.rendered,
            };
            D::evolve(&mut self.state, &env);
            self.journal.push(env);
            self.head = Some(id);
        }
        let mut queued = vec![];
        for (id, e) in decision.effects {
            if !self.pending.iter().any(|(p, _)| *p == id) {
                queued.push(id);
                self.pending.push((id, e));
            }
        }
        queued
    }

    /// Apply, recording a rejection instead of returning it.
    fn feed(&mut self, input: Input) {
        if let Err(r) = self.apply(input) {
            self.rejections.push(r);
        }
    }

    /// Resolve one pending effect, trying them in scheduler order.
    pub fn step_with(&mut self, world: &mut impl World) -> Step {
        if self.pending.is_empty() {
            return Step::Idle;
        }
        let order = self.sched.order(self.pending.len());
        for i in order {
            let (id, effect) = self.pending[i].clone();
            let res = world.resolve(id, &effect);
            if let Effect::Finish(outcome) = &effect {
                self.pending.remove(i);
                self.boundaries += 1;
                if !self.outcomes.iter().any(|(o, _)| *o == id) {
                    self.outcomes.push((id, outcome.clone()));
                }
                if let Some(r) = res {
                    self.feed(Input::Completed(id, r));
                }
                return Step::Finished(id);
            }
            if let Some(r) = res {
                self.pending.remove(i);
                self.boundaries += 1;
                self.feed(Input::Completed(id, r));
                return Step::Delivered(id);
            }
        }
        Step::Stalled
    }

    /// Step until nothing is pending, the world stalls, or the step limit.
    pub fn run_until_idle(&mut self, world: &mut impl World) -> Step {
        for _ in 0..self.max_steps {
            match self.step_with(world) {
                Step::Delivered(_) | Step::Finished(_) => continue,
                other => return other,
            }
        }
        Step::LimitReached
    }

    /// Simulate a crash: in-memory state and pending effects are lost; a fresh
    /// state is rebuilt by folding the journal, then outstanding effects are
    /// reconciled (re-queued for dispatch).
    pub fn crash_and_recover(&mut self) {
        self.crashes += 1;
        let mut s = D::State::default();
        for ev in &self.journal {
            D::evolve(&mut s, ev);
        }
        self.state = s;
        self.head = self.journal.last().map(|e| e.id.clone());
        self.pending = D::outstanding(&self.state);
    }

    /// Run a scenario without faults and return the final journal.
    pub fn run(mut self, inputs: impl IntoIterator<Item = Input>, world: &mut impl World) -> Vec<Envelope<Event>> {
        for i in inputs {
            self.feed(i);
        }
        self.run_until_idle(world);
        self.journal
    }

    /// Fault injection: apply `inputs`, run until `k` effect boundaries have
    /// passed, crash, recover, finish the run and return the final journal.
    /// Compare with [`KernelSim::run`] via [`model_view`] / [`outcomes`] (ids
    /// and timestamps may differ).
    pub fn crash_at_effect(
        mut self,
        k: usize,
        inputs: impl IntoIterator<Item = Input>,
        world: &mut impl World,
    ) -> Vec<Envelope<Event>> {
        for i in inputs {
            self.feed(i);
        }
        let mut steps = 0;
        while self.boundaries < k && steps < self.max_steps {
            steps += 1;
            match self.step_with(world) {
                Step::Delivered(_) | Step::Finished(_) => {}
                _ => break,
            }
        }
        self.crash_and_recover();
        self.run_until_idle(world);
        self.journal
    }
}

/// The model-visible renderings of a journal, in order.
pub fn model_view(journal: &[Envelope<Event>]) -> Vec<Rendered> {
    journal
        .iter()
        .filter(|e| e.audience.model_visible())
        .filter_map(|e| e.rendered.clone())
        .collect()
}

/// Turn outcomes recorded in a journal (`TurnEnded` events), in order.
pub fn outcomes(journal: &[Envelope<Event>]) -> Vec<TurnOutcome> {
    journal
        .iter()
        .filter_map(|e| match &e.body {
            Event::TurnEnded { outcome } => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}
