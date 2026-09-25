//! Performance budgets of `docs/design.md` ("Performance budget"), measured
//! with a few hundred iterations each. Budgets are asserted in release builds
//! only (`cargo test -p agent-bench --release --test budgets -- --nocapture
//! --test-threads=1`); debug builds just print the numbers.
//!
//! Set `AGENT_BENCH_REPORT_ONLY=1` to print without asserting in release.
//!
//! The shadow-snapshot budget is covered by the criterion bench
//! (`cargo bench -p agent-bench`) since building a large workspace is slow.

use agent_bench::*;
use agent_proto::SessionId;
use std::time::{Duration, Instant};

fn check(name: &str, s: Summary, stat: Duration, budget: Duration) {
    println!("{name}: {s} (budget {budget:?})");
    #[cfg(not(debug_assertions))]
    if std::env::var_os("AGENT_BENCH_REPORT_ONLY").is_none() {
        assert!(stat < budget, "{name}: {stat:?} exceeds the budget {budget:?} ({s})");
    }
    #[cfg(debug_assertions)]
    let _ = (stat, budget);
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
}

#[test]
fn kernel_decide_evolve_p99_under_1ms() {
    // Hundreds of turns with tool calls; every input is timed.
    let mut d = SessionDriver::default();
    for t in 0..300 {
        d.turn(tool_steps_for(t));
    }
    println!("  session: {} turns, {} events, {} inputs", d.turns(), d.log.len(), d.timings.len());
    let mut kinds: Vec<&str> = d.kinds.clone();
    kinds.sort();
    kinds.dedup();
    for k in kinds {
        println!("    {k:>12}: {}", Summary::of(&d.timings_of(k)));
    }
    let s = Summary::of(&d.timings);
    check("kernel decide+evolve per input", s, s.p99, Duration::from_millis(1));
}

#[test]
fn journal_append_p99_under_10ms() {
    let session = build_session(2_000);
    rt().block_on(async {
        let mut j = JournalBench::new(&session).await;
        for _ in 0..20 {
            j.append_next().await; // warm up
        }
        let mut v = vec![];
        for _ in 0..400 {
            v.push(j.append_next().await);
        }
        let s = Summary::of(&v);
        check("sqlite journal append (WAL, fsync)", s, s.p99, Duration::from_millis(10));
    });
}

#[test]
fn resume_10k_events_under_1s() {
    let session = build_session(10_000);
    let events = &session.log;
    println!(
        "  session: {} events, {} JSON bytes ({} in EffectIssued(Sample) events)",
        events.len(),
        journal_bytes(events),
        sample_effect_bytes(events)
    );
    // Pure fold.
    let mut folds = vec![];
    for _ in 0..20 {
        let t = Instant::now();
        std::hint::black_box(fold(events));
        folds.push(t.elapsed());
    }
    let s = Summary::of(&folds);
    check("fold 10k events", s, s.max, Duration::from_secs(1));
    // Load from SQLite (JSON decode + read-time upgrade) + fold.
    rt().block_on(async {
        let j = JournalBench::new(&session).await;
        let sid = SessionId::new("resume");
        j.write_session(&sid, events).await;
        let runs = if cfg!(debug_assertions) { 2 } else { 10 };
        let (mut loads, mut v) = (vec![], vec![]);
        for _ in 0..runs {
            let t = Instant::now();
            std::hint::black_box(j.load(&sid).await);
            loads.push(t.elapsed());
            let t = Instant::now();
            let (n, state) = j.load_and_fold(&sid).await;
            v.push(t.elapsed());
            assert_eq!(n, events.len());
            std::hint::black_box(state);
        }
        println!("  sqlite load only: {}", Summary::of(&loads));
        let s = Summary::of(&v);
        check("load (sqlite) + fold 10k events", s, s.max, Duration::from_secs(1));
    });
}

#[test]
fn first_token_overhead_p99_under_50ms() {
    rt().block_on(async {
        for kind in [JournalKind::Memory, JournalKind::Sqlite] {
            let mut ft = FirstToken::new(kind, 320).await;
            for _ in 0..20 {
                ft.measure().await;
            }
            let mut v = vec![];
            while ft.remaining() > 0 {
                v.push(ft.measure().await);
            }
            let s = Summary::of(&v);
            check(&format!("first-token overhead ({kind:?} journal)"), s, s.p99, Duration::from_millis(50));
        }
    });
}
