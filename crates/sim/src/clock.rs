//! Virtual time and deterministic ids.

use agent_proto::{EventId, Timestamp};
use agent_runtime::{Clock, IdGen};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A manually advanced clock. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct VirtualClock(Arc<AtomicU64>);

impl VirtualClock {
    pub fn new(start_ms: u64) -> Self {
        VirtualClock(Arc::new(AtomicU64::new(start_ms)))
    }
    pub fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
    /// Advance by `ms` and return the new time.
    pub fn advance(&self, ms: u64) -> Timestamp {
        Timestamp(self.0.fetch_add(ms, Ordering::SeqCst) + ms)
    }
    /// Set the time (never goes backwards).
    pub fn set(&self, ms: u64) {
        self.0.fetch_max(ms, Ordering::SeqCst);
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Timestamp {
        VirtualClock::now(self)
    }
}

/// Deterministic event ids: 26-digit zero-padded counters (same length as a
/// ULID, and they sort in issue order). Clones share the counter.
#[derive(Debug, Clone, Default)]
pub struct SeqIds(Arc<AtomicU64>);

impl SeqIds {
    pub fn new() -> Self {
        Self::default()
    }
    /// Start counting at `n` (e.g. to give a second generator a disjoint range).
    pub fn starting_at(n: u64) -> Self {
        SeqIds(Arc::new(AtomicU64::new(n)))
    }
    pub fn next_id(&self) -> EventId {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        EventId(format!("{n:026}"))
    }
    /// How many ids were issued so far.
    pub fn issued(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl IdGen for SeqIds {
    fn event_id(&self, _at: Timestamp) -> EventId {
        self.next_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_shared_and_monotonic() {
        let c = VirtualClock::new(10);
        let c2 = c.clone();
        assert_eq!(c2.advance(5), Timestamp(15));
        assert_eq!(Clock::now(&c), Timestamp(15));
        c.set(3);
        assert_eq!(c.now(), Timestamp(15));
        c.set(100);
        assert_eq!(c2.now(), Timestamp(100));
    }

    #[test]
    fn ids_sort_in_issue_order() {
        let ids = SeqIds::new();
        let a = ids.event_id(Timestamp(5));
        let b = ids.clone().event_id(Timestamp(1));
        assert_eq!(a.0.len(), 26);
        assert!(a < b);
        assert_eq!(ids.issued(), 2);
    }
}
