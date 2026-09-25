//! Typed observers: the closure's parameter type is the filter.

use agent_proto::*;
use agent_runtime::Observer;
use async_trait::async_trait;
use std::fmt;
use std::marker::PhantomData;

/// An event view an observer can subscribe to.
pub trait Observed: Sized + Send + Sync {
    fn from_event(ev: &Envelope<Event>) -> Option<Self>;
}

impl Observed for Envelope<Event> {
    fn from_event(ev: &Envelope<Event>) -> Option<Self> {
        Some(ev.clone())
    }
}

/// A tool call finished with an error result.
#[derive(Debug, Clone)]
pub struct ToolFailed {
    pub call: ToolCall,
    pub result: ToolResult,
}

impl fmt::Display for ToolFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text: Vec<&str> = self
            .result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolContent::Text { text } => Some(text.as_str()),
                ToolContent::Blob { preview, .. } => Some(preview.as_str()),
                _ => None,
            })
            .collect();
        write!(f, "tool `{}` ({}) failed: {}", self.call.name, self.call.id, text.join(" "))
    }
}

impl Observed for ToolFailed {
    fn from_event(ev: &Envelope<Event>) -> Option<Self> {
        match &ev.body {
            Event::ToolResulted { call, result } if result.is_error => {
                Some(ToolFailed { call: call.clone(), result: result.clone() })
            }
            _ => None,
        }
    }
}

/// Any tool call finished.
#[derive(Debug, Clone)]
pub struct ToolFinished {
    pub call: ToolCall,
    pub result: ToolResult,
}

impl Observed for ToolFinished {
    fn from_event(ev: &Envelope<Event>) -> Option<Self> {
        match &ev.body {
            Event::ToolResulted { call, result } => Some(ToolFinished { call: call.clone(), result: result.clone() }),
            _ => None,
        }
    }
}

/// A turn ended.
#[derive(Debug, Clone)]
pub struct TurnFinished {
    pub outcome: TurnOutcome,
}

impl Observed for TurnFinished {
    fn from_event(ev: &Envelope<Event>) -> Option<Self> {
        match &ev.body {
            Event::TurnEnded { outcome } => Some(TurnFinished { outcome: outcome.clone() }),
            _ => None,
        }
    }
}

pub(crate) struct FnObserver<E, F> {
    name: String,
    f: F,
    _e: PhantomData<fn(&E)>,
}

impl<E: Observed, F: Fn(&E) + Send + Sync> FnObserver<E, F> {
    pub fn new(f: F) -> Self {
        FnObserver { name: format!("fn:{}", std::any::type_name::<E>()), f, _e: PhantomData }
    }
}

#[async_trait]
impl<E: Observed + 'static, F: Fn(&E) + Send + Sync + 'static> Observer for FnObserver<E, F> {
    fn name(&self) -> &str {
        &self.name
    }
    async fn on_event(&self, _session: &SessionId, ev: &Envelope<Event>) -> Result<(), String> {
        if let Some(e) = E::from_event(ev) {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.f)(&e)))
                .map_err(|_| format!("observer {} panicked", self.name))?;
        }
        Ok(())
    }
}
