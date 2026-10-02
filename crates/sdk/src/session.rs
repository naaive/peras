//! `Chat`: a named, persistent conversation (created if missing, resumed
//! otherwise).

use crate::agent::Agent;
use crate::error::Error;
use crate::run::{Run, Target};
use agent_proto::*;
use futures::StreamExt;

pub struct Chat {
    agent: Agent,
    id: SessionId,
}

impl Chat {
    pub(crate) fn new(agent: Agent, id: SessionId) -> Chat {
        Chat { agent, id }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Send a message; resolves to the turn's final text.
    pub async fn send(&self, text: impl Into<String>) -> Result<String, Error> {
        self.stream(text).await
    }

    /// Send a message as a streaming `Run`.
    pub fn stream(&self, text: impl Into<String>) -> Run {
        Run::new(self.agent.clone(), Target::Open(self.id.clone()), text.into())
    }

    /// Send a control to the session (e.g. `Control::SwitchModel` between
    /// turns, `Control::ClearTaint` after review).
    pub async fn control(&self, c: Control) -> Result<(), Error> {
        let h = self.agent.open(&self.id).await?;
        h.send(Input::Control(c)).await?;
        Ok(())
    }

    /// Set a state value ("what is true now": the mode, the environment).
    /// The kernel appends it as a snapshot at the next safe point when it
    /// changed (rate-limited by `[[snapshots]]`); an empty value clears it.
    pub async fn set_state(&self, key: impl Into<String>, value: impl Into<String>) -> Result<(), Error> {
        let h = self.agent.open(&self.id).await?;
        h.send(Input::Signal(Signal::Silent { key: key.into(), value: value.into() })).await?;
        Ok(())
    }

    /// Summarise the whole conversation now (between turns): the history is
    /// replaced by a summary that keeps its taint, and later turns continue
    /// from it. `focus` is added to the summary instruction. The run ends
    /// with `Done` ("Conversation compacted." / "Nothing to compact.") or
    /// `Failed`.
    pub fn compact(&self, focus: Option<String>) -> Run {
        Run::with_input(self.agent.clone(), Target::Open(self.id.clone()), Input::Control(Control::Compact { focus }))
    }

    /// Next seq (useful as a rewind point).
    pub async fn next_seq(&self) -> Result<Seq, Error> {
        Ok(self.agent.open(&self.id).await?.next_seq())
    }

    /// All events so far.
    pub async fn events(&self) -> Result<Vec<Envelope<Event>>, Error> {
        let b = self.agent.built().await?;
        b.rt.env().journal.load(&self.id, 0).await.map_err(|e| Error::Failed(e.to_string()))
    }

    /// Rewind the conversation to the event at `seq`, restoring the workspace
    /// (only agent-attributed changes). Returns the rewind report.
    pub async fn rewind(&self, seq: Seq) -> Result<RestoreReport, Error> {
        let h = self.agent.open(&self.id).await?;
        let events = self.events().await?;
        let target = events
            .iter()
            .find(|e| e.seq == seq)
            .map(|e| e.id.clone())
            .ok_or_else(|| Error::Failed(format!("no event at seq {seq}")))?;
        let mut sub = h.subscribe(h.next_seq());
        h.send(Input::Control(Control::Rewind { to: target })).await?;
        while let Some(ev) = sub.next().await {
            match ev.body {
                Event::RewindCompleted { report, .. } => return Ok(report),
                Event::TurnEnded { outcome: TurnOutcome::Failed { error } } => return Err(Error::Failed(error)),
                _ => {}
            }
        }
        Err(Error::Ended)
    }
}
