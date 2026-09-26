//! Runtime metrics: lock-free counters and fixed-bucket histograms, read with
//! [`Metrics::snapshot`].
//!
//! | Metric | Source |
//! | --- | --- |
//! | First-token latency | time from opening the model stream to the first content delta |
//! | Cache hit ratio | `Usage::cache_read_tokens / (cache_read_tokens + input_tokens)` of every sample |
//! | Sequence switches / replacements | `SequenceOpened` / `Replaced` events appended |
//! | Tool / checkpoint durations | wall time of each tool call / checkpoint effect |
//! | Per-rule approvals | `QuestionAnswered` (rules from the matching `QuestionAsked`) and non-human `VerdictRecorded` |
//!
//! Metrics describe this process only: events folded during a resume are not
//! counted again.

use agent_proto::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Upper bounds (milliseconds, inclusive) of the histogram buckets; one more
/// overflow bucket follows.
pub const BUCKETS_MS: [u64; 14] = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 60_000];

/// Fixed-bucket latency histogram in microseconds.
#[derive(Debug, Default)]
pub struct Histogram {
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
    buckets: [AtomicU64; BUCKETS_MS.len() + 1],
}

/// Read view of a [`Histogram`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    /// Counts per bucket of [`BUCKETS_MS`] plus the overflow bucket.
    pub buckets: Vec<u64>,
}

impl HistogramSnapshot {
    pub fn mean_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum_us as f64 / self.count as f64 / 1000.0
        }
    }

    /// Upper bound (ms) of the bucket holding quantile `q` (0..=1); the
    /// maximum for the overflow bucket. `None` when empty.
    pub fn quantile_ms(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let rank = ((q.clamp(0.0, 1.0) * self.count as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Some(match BUCKETS_MS.get(i) {
                    Some(b) => *b as f64,
                    None => self.max_us as f64 / 1000.0,
                });
            }
        }
        Some(self.max_us as f64 / 1000.0)
    }
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
        let ms_ceil = us.div_ceil(1000);
        let i = BUCKETS_MS.iter().position(|b| ms_ceil <= *b).unwrap_or(BUCKETS_MS.len());
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
            buckets: self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect(),
        }
    }
}

/// Decisions and approvals attributed to one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuleStats {
    pub decisions: u64,
    pub approvals: u64,
}

impl RuleStats {
    /// `approvals / decisions` (0 when no decisions).
    pub fn approval_rate(&self) -> f64 {
        if self.decisions == 0 {
            0.0
        } else {
            self.approvals as f64 / self.decisions as f64
        }
    }
}

/// Key used for questions that name no rule.
pub const UNLABELED_RULE: &str = "(unlabeled)";

/// Read view of [`Metrics`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetricsSnapshot {
    pub first_token: HistogramSnapshot,
    pub samples: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    /// `cache_read / (cache_read + input)`; 0 when nothing was read.
    pub cache_hit_ratio: f64,
    /// `SequenceOpened` events (sequence starts and switches).
    pub sequences_opened: u64,
    /// `Replaced` events (cache invalidations by pressure relief / rerender).
    pub replacements: u64,
    pub tool_calls: HistogramSnapshot,
    pub checkpoints: HistogramSnapshot,
    pub effects_dispatched: u64,
    pub request_mismatches: u64,
    pub rules: BTreeMap<String, RuleStats>,
}

impl MetricsSnapshot {
    /// Overall approval rate across rules.
    pub fn approval_rate(&self) -> f64 {
        let (d, a) = self.rules.values().fold((0, 0), |(d, a), r| (d + r.decisions, a + r.approvals));
        if d == 0 {
            0.0
        } else {
            a as f64 / d as f64
        }
    }
}

const MAX_OPEN_QUESTIONS: usize = 16 * 1024;

/// Shared by every session of a runtime (see `RuntimeBuilder::metrics`).
#[derive(Debug, Default)]
pub struct Metrics {
    pub first_token: Histogram,
    pub tool_calls: Histogram,
    pub checkpoints: Histogram,
    samples: AtomicU64,
    input_tokens: AtomicU64,
    cache_read_tokens: AtomicU64,
    cache_write_tokens: AtomicU64,
    output_tokens: AtomicU64,
    sequences_opened: AtomicU64,
    replacements: AtomicU64,
    effects_dispatched: AtomicU64,
    request_mismatches: AtomicU64,
    rules: Mutex<BTreeMap<String, RuleStats>>,
    /// Rules of questions asked but not yet answered.
    open_questions: Mutex<HashMap<(SessionId, QuestionId), Vec<String>>>,
}

fn approved(v: &Verdict) -> bool {
    matches!(v, Verdict::Allow | Verdict::Annotate(_) | Verdict::Rewrite(_) | Verdict::Continue(_))
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Usage of a finished sample.
    pub fn observe_usage(&self, u: &Usage) {
        self.samples.fetch_add(1, Ordering::Relaxed);
        self.input_tokens.fetch_add(u.input_tokens as u64, Ordering::Relaxed);
        self.cache_read_tokens.fetch_add(u.cache_read_tokens as u64, Ordering::Relaxed);
        self.cache_write_tokens.fetch_add(u.cache_write_tokens as u64, Ordering::Relaxed);
        self.output_tokens.fetch_add(u.output_tokens as u64, Ordering::Relaxed);
    }

    pub fn effect_dispatched(&self) {
        self.effects_dispatched.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request_mismatch(&self) {
        self.request_mismatches.fetch_add(1, Ordering::Relaxed);
    }

    fn count_rule(&self, rule: &str, ok: bool) {
        let mut rules = self.rules.lock().unwrap();
        let r = rules.entry(rule.to_string()).or_default();
        r.decisions += 1;
        if ok {
            r.approvals += 1;
        }
    }

    /// Remember the rules of a question asked before this process started
    /// (called while folding the journal on resume; counts nothing).
    pub fn prime(&self, session: &SessionId, ev: &Envelope<Event>) {
        match &ev.body {
            Event::QuestionAsked { question, .. } => self.open_question(session, question),
            Event::QuestionAnswered { question, .. } => {
                self.open_questions.lock().unwrap().remove(&(session.clone(), question.clone()));
            }
            _ => {}
        }
    }

    fn open_question(&self, session: &SessionId, q: &Question) {
        let mut open = self.open_questions.lock().unwrap();
        if open.len() >= MAX_OPEN_QUESTIONS {
            open.clear();
        }
        open.insert((session.clone(), q.id.clone()), q.rules.clone());
    }

    /// An event was appended to `session`'s journal.
    pub fn observe_event(&self, session: &SessionId, ev: &Envelope<Event>) {
        match &ev.body {
            Event::SequenceOpened { .. } => {
                self.sequences_opened.fetch_add(1, Ordering::Relaxed);
            }
            Event::Replaced(_) => {
                self.replacements.fetch_add(1, Ordering::Relaxed);
            }
            Event::QuestionAsked { question, .. } => self.open_question(session, question),
            Event::QuestionAnswered { question, answer, .. } => {
                let rules = self.open_questions.lock().unwrap().remove(&(session.clone(), question.clone()));
                let ok = matches!(answer, Answer::Allow { .. } | Answer::AllowWith(_));
                match rules {
                    Some(rules) if !rules.is_empty() => {
                        for r in &rules {
                            self.count_rule(r, ok);
                        }
                    }
                    _ => self.count_rule(UNLABELED_RULE, ok),
                }
            }
            // Human-ring verdicts are counted through `QuestionAnswered`.
            Event::VerdictRecorded { ring, verdict, responder, .. } if *ring != Ring::Human => {
                let name = match responder {
                    Responder::Policy(n) => format!("policy:{n}"),
                    Responder::Hook(n) => format!("hook:{n}"),
                    Responder::AutoRule(n) => format!("auto:{n}"),
                    _ => return,
                };
                self.count_rule(&name, approved(verdict));
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let input = self.input_tokens.load(Ordering::Relaxed);
        let read = self.cache_read_tokens.load(Ordering::Relaxed);
        MetricsSnapshot {
            first_token: self.first_token.snapshot(),
            samples: self.samples.load(Ordering::Relaxed),
            input_tokens: input,
            cache_read_tokens: read,
            cache_write_tokens: self.cache_write_tokens.load(Ordering::Relaxed),
            output_tokens: self.output_tokens.load(Ordering::Relaxed),
            cache_hit_ratio: if input + read == 0 { 0.0 } else { read as f64 / (input + read) as f64 },
            sequences_opened: self.sequences_opened.load(Ordering::Relaxed),
            replacements: self.replacements.load(Ordering::Relaxed),
            tool_calls: self.tool_calls.snapshot(),
            checkpoints: self.checkpoints.snapshot(),
            effects_dispatched: self.effects_dispatched.load(Ordering::Relaxed),
            request_mismatches: self.request_mismatches.load(Ordering::Relaxed),
            rules: self.rules.lock().unwrap().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_and_quantiles() {
        let h = Histogram::default();
        for ms in [1, 3, 3, 40, 900] {
            h.observe(Duration::from_millis(ms));
        }
        h.observe(Duration::from_secs(120));
        let s = h.snapshot();
        assert_eq!(s.count, 6);
        assert_eq!(s.buckets.iter().sum::<u64>(), 6);
        assert_eq!(s.quantile_ms(0.5), Some(5.0));
        assert_eq!(s.quantile_ms(1.0), Some(120_000.0));
        assert!(HistogramSnapshot::default().quantile_ms(0.5).is_none());
    }
}
