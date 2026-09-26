//! Composable model-port layers: retry, shared rate limit, metering.
//!
//! Layers act within one model only and affect metrics, never the journal:
//! the deltas they pass through are unchanged.

use agent_proto::{ModelCaps, ModelError, Usage};
use agent_runtime::{Delta, Encoder, ModelPort, Request};
use futures::stream::{BoxStream, StreamExt};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// `Claude::default().retry(3).rate_limit(q).meter()`.
pub trait ModelPortExt: ModelPort + Sized {
    /// Retry retryable errors up to `n` extra times with exponential backoff.
    fn retry(self, n: u32) -> Retry<Self> {
        Retry::new(self, RetryPolicy { max_retries: n, ..RetryPolicy::default() })
    }
    fn retry_with(self, policy: RetryPolicy) -> Retry<Self> {
        Retry::new(self, policy)
    }
    /// Share a quota (concurrency + optional request rate) across ports/sessions.
    fn rate_limit(self, q: Arc<Quota>) -> RateLimited<Self> {
        RateLimited { inner: self, quota: q }
    }
    /// Meter usage and cost into a fresh [`Meter`] (see [`Metered::shared_meter`]).
    fn meter(self) -> Metered<Self> {
        Metered { inner: self, meter: Arc::new(Meter::default()) }
    }
    /// Meter into a shared [`Meter`].
    fn meter_into(self, m: Arc<Meter>) -> Metered<Self> {
        Metered { inner: self, meter: m }
    }
}

impl<T: ModelPort + Sized> ModelPortExt for T {}

// ------------------------------------------------------------------ retry

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_retries: 3, base_delay: Duration::from_millis(500), max_delay: Duration::from_secs(30) }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (0-based): exponential, capped,
    /// never shorter than the server's `retry_after`.
    pub fn delay(&self, attempt: u32, err: &ModelError) -> Duration {
        let exp = self.base_delay.saturating_mul(1u32 << attempt.min(16)).min(self.max_delay);
        match err {
            ModelError::RateLimited { retry_after_ms: Some(ms) } => exp.max(Duration::from_millis(*ms)),
            _ => exp,
        }
    }
}

/// Retries retryable errors that happen before any delta was emitted.
pub struct Retry<M> {
    inner: M,
    policy: RetryPolicy,
    retries: Arc<AtomicU64>,
}

impl<M> Retry<M> {
    pub fn new(inner: M, policy: RetryPolicy) -> Self {
        Retry { inner, policy, retries: Arc::default() }
    }
    /// Total retries performed (metric).
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }
    pub fn inner(&self) -> &M {
        &self.inner
    }
}

impl<M: ModelPort> ModelPort for Retry<M> {
    fn caps(&self) -> &ModelCaps {
        self.inner.caps()
    }
    fn encoder(&self) -> &dyn Encoder {
        self.inner.encoder()
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        struct St<'a> {
            inner: &'a dyn ModelPort,
            req: Request,
            policy: RetryPolicy,
            retries: &'a AtomicU64,
            attempt: u32,
            cur: Option<BoxStream<'a, Result<Delta, ModelError>>>,
            emitted: bool,
            done: bool,
        }
        let st = St {
            inner: &self.inner,
            req,
            policy: self.policy,
            retries: &self.retries,
            attempt: 0,
            cur: None,
            emitted: false,
            done: false,
        };
        futures::stream::unfold(st, |mut st| async move {
            if st.done {
                return None;
            }
            loop {
                if st.cur.is_none() {
                    st.cur = Some(st.inner.stream(st.req.clone()));
                }
                match st.cur.as_mut().expect("stream").next().await {
                    Some(Ok(d)) => {
                        st.emitted = true;
                        return Some((Ok(d), st));
                    }
                    Some(Err(e)) => {
                        if !st.emitted && e.retryable() && st.attempt < st.policy.max_retries {
                            let d = st.policy.delay(st.attempt, &e);
                            tracing::debug!(attempt = st.attempt, error = %e, delay_ms = d.as_millis() as u64, "model retry");
                            st.cur = None;
                            st.attempt += 1;
                            st.retries.fetch_add(1, Ordering::Relaxed);
                            tokio::time::sleep(d).await;
                            continue;
                        }
                        st.done = true;
                        return Some((Err(e), st));
                    }
                    None => return None,
                }
            }
        })
        .boxed()
    }
}

// ------------------------------------------------------------------ rate limit

/// Shared quota: bounded concurrency plus an optional token bucket of
/// requests per minute. Share one `Arc<Quota>` across sessions.
pub struct Quota {
    sem: Arc<Semaphore>,
    bucket: Option<Mutex<Bucket>>,
}

struct Bucket {
    capacity: f64,
    tokens: f64,
    per_sec: f64,
    last: Instant,
}

impl Quota {
    /// At most `max_concurrent` streams in flight.
    pub fn new(max_concurrent: usize) -> Self {
        Quota { sem: Arc::new(Semaphore::new(max_concurrent.max(1))), bucket: None }
    }
    /// Additionally limit request starts to `rpm` per minute (burst = `rpm`).
    pub fn per_minute(mut self, rpm: u32) -> Self {
        let cap = rpm.max(1) as f64;
        self.bucket = Some(Mutex::new(Bucket { capacity: cap, tokens: cap, per_sec: cap / 60.0, last: Instant::now() }));
        self
    }
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// Wait for a slot; the permit is held until the stream ends.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        let permit = self.sem.clone().acquire_owned().await.expect("semaphore closed");
        if let Some(b) = &self.bucket {
            loop {
                let wait = {
                    let mut b = b.lock().expect("bucket");
                    let now = Instant::now();
                    b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * b.per_sec).min(b.capacity);
                    b.last = now;
                    if b.tokens >= 1.0 {
                        b.tokens -= 1.0;
                        None
                    } else {
                        Some(Duration::from_secs_f64((1.0 - b.tokens) / b.per_sec))
                    }
                };
                match wait {
                    None => break,
                    Some(d) => tokio::time::sleep(d).await,
                }
            }
        }
        permit
    }
}

pub struct RateLimited<M> {
    inner: M,
    quota: Arc<Quota>,
}

impl<M: ModelPort> ModelPort for RateLimited<M> {
    fn caps(&self) -> &ModelCaps {
        self.inner.caps()
    }
    fn encoder(&self) -> &dyn Encoder {
        self.inner.encoder()
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        let quota = self.quota.clone();
        futures::stream::once(async move { quota.acquire().await })
            .flat_map(move |permit| {
                // Keep the permit alive for as long as the inner stream runs.
                self.inner.stream(req.clone()).map(move |d| {
                    let _p = &permit;
                    d
                })
            })
            .boxed()
    }
}

// ------------------------------------------------------------------ meter

/// Prices in micro-dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Price {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Price {
    /// From dollars per Mtok for input/output; cache read = 10%, write = 125%.
    pub const fn usd_per_mtok(input_usd: u64, output_usd: u64) -> Price {
        Price {
            input: input_usd * 1_000_000,
            output: output_usd * 1_000_000,
            cache_read: input_usd * 100_000,
            cache_write: input_usd * 1_250_000,
        }
    }
    pub fn cost_micros(&self, u: &Usage) -> u64 {
        let t = |n: u32, p: u64| n as u128 * p as u128;
        ((t(u.input_tokens, self.input)
            + t(u.output_tokens, self.output)
            + t(u.cache_read_tokens, self.cache_read)
            + t(u.cache_write_tokens, self.cache_write))
            / 1_000_000) as u64
    }
}

/// Default price table (model id -> price). Unknown ids cost 0.
pub fn default_prices() -> BTreeMap<String, Price> {
    let mut m = BTreeMap::new();
    m.insert("claude-sonnet-5".into(), Price::usd_per_mtok(2, 10));
    // Opus 5.5 cache reads are $0.20/Mtok (5%).
    m.insert("claude-opus-5-5".into(), Price { cache_read: 200_000, ..Price::usd_per_mtok(4, 20) });
    m.insert("claude-opus-5".into(), Price::usd_per_mtok(5, 25));
    m.insert("claude-haiku-4-5".into(), Price::usd_per_mtok(1, 5));
    m.insert("claude-haiku-4-5-20251001".into(), Price::usd_per_mtok(1, 5));
    m
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeterTotals {
    pub requests: u64,
    pub errors: u64,
    pub usage_input: u64,
    pub usage_output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_micros: u64,
}

impl MeterTotals {
    fn add(&mut self, u: &Usage, cost: u64) {
        self.usage_input += u.input_tokens as u64;
        self.usage_output += u.output_tokens as u64;
        self.cache_read += u.cache_read_tokens as u64;
        self.cache_write += u.cache_write_tokens as u64;
        self.cost_micros += cost;
    }
}

/// Shared usage/cost accumulator.
pub struct Meter {
    prices: BTreeMap<String, Price>,
    totals: Mutex<BTreeMap<String, MeterTotals>>,
}

impl Default for Meter {
    fn default() -> Self {
        Meter::with_prices(default_prices())
    }
}

impl Meter {
    pub fn with_prices(prices: BTreeMap<String, Price>) -> Self {
        Meter { prices, totals: Mutex::default() }
    }
    pub fn price(&self, model: &str) -> Price {
        self.prices.get(model).copied().unwrap_or_default()
    }
    pub fn cost_of(&self, model: &str, u: &Usage) -> u64 {
        self.price(model).cost_micros(u)
    }
    fn with<R>(&self, model: &str, f: impl FnOnce(&mut MeterTotals) -> R) -> R {
        let mut t = self.totals.lock().expect("meter");
        f(t.entry(model.to_string()).or_default())
    }
    pub fn record(&self, model: &str, u: &Usage) {
        let cost = self.cost_of(model, u);
        self.with(model, |t| t.add(u, cost));
    }
    /// Totals for one model.
    pub fn model_totals(&self, model: &str) -> MeterTotals {
        self.totals.lock().expect("meter").get(model).copied().unwrap_or_default()
    }
    /// Totals over all models.
    pub fn totals(&self) -> MeterTotals {
        let t = self.totals.lock().expect("meter");
        t.values().fold(MeterTotals::default(), |mut a, b| {
            a.requests += b.requests;
            a.errors += b.errors;
            a.usage_input += b.usage_input;
            a.usage_output += b.usage_output;
            a.cache_read += b.cache_read;
            a.cache_write += b.cache_write;
            a.cost_micros += b.cost_micros;
            a
        })
    }
}

pub struct Metered<M> {
    inner: M,
    meter: Arc<Meter>,
}

impl<M> Metered<M> {
    /// The shared meter (clone the `Arc` to read totals elsewhere).
    pub fn shared_meter(&self) -> Arc<Meter> {
        self.meter.clone()
    }
    pub fn totals(&self) -> MeterTotals {
        self.meter.totals()
    }
    pub fn inner(&self) -> &M {
        &self.inner
    }
}

impl<M: ModelPort> ModelPort for Metered<M> {
    fn caps(&self) -> &ModelCaps {
        self.inner.caps()
    }
    fn encoder(&self) -> &dyn Encoder {
        self.inner.encoder()
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<Delta, ModelError>> {
        let model = self.inner.caps().model.0.clone();
        let meter = self.meter.clone();
        meter.with(&model, |t| t.requests += 1);
        self.inner
            .stream(req)
            .inspect(move |d| match d {
                Ok(Delta::Usage(u)) => meter.record(&model, u),
                Err(_) => meter.with(&model, |t| t.errors += 1),
                _ => {}
            })
            .boxed()
    }
}
