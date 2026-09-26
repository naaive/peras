//! Criterion benches for the performance budgets of `docs/design.md`.
//!
//! `cargo bench -p agent-bench` (all groups) or e.g.
//! `cargo bench -p agent-bench -- shadow`. `AGENT_BENCH_FILES=100000` sizes the
//! shadow workspace like the design budget; `AGENT_BENCH_DIR` picks the disk.
//!
//! Criterion reports means; since the budgets are stated as p99, every group
//! also prints `[budget] ... p99=...` over its own samples to stderr.

use agent_bench::*;
use agent_proto::SessionId;
use agent_runtime::{Checkpointer, ShadowCheckpointer};
use criterion::{criterion_group, criterion_main, Criterion};
use std::time::{Duration, Instant};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
}

fn report(name: &str, samples: &[Duration], budget: &str) {
    if !samples.is_empty() {
        eprintln!("[budget] {name}: {} ({budget})", Summary::of(samples));
    }
}

/// (a) kernel decide + evolve per input over a realistic session. The session
/// continues across iterations and restarts every 400 turns.
fn kernel(c: &mut Criterion) {
    let mut g = c.benchmark_group("kernel");
    let mut d = SessionDriver::default();
    let mut all = vec![];
    g.bench_function("decide_evolve_per_input", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut n = 0u64;
            while n < iters {
                if d.turns() >= 400 {
                    all.append(&mut d.timings);
                    d = SessionDriver::default();
                }
                let start = d.timings.len();
                d.turn(tool_steps_for(d.turns()));
                for t in &d.timings[start..] {
                    if n < iters {
                        total += *t;
                        n += 1;
                    }
                }
            }
            total
        })
    });
    g.finish();
    all.append(&mut d.timings);
    report("kernel decide+evolve per input", &all, "budget p99 < 1 ms");
}

/// (b) SQLite journal append (WAL, synchronous=FULL) of real decision batches.
fn journal(c: &mut Criterion) {
    let rt = rt();
    let session = build_session(3_000);
    let mut j = rt.block_on(JournalBench::new(&session));
    let mut all = vec![];
    let mut g = c.benchmark_group("journal");
    g.sample_size(30).measurement_time(Duration::from_secs(8));
    g.bench_function("sqlite_append_fsync", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let dt = rt.block_on(j.append_next());
                all.push(dt);
                total += dt;
            }
            total
        })
    });
    g.finish();
    report("sqlite journal append", &all, "budget p99 < 10 ms");
}

/// (c) shadow checkpoint of a large synthetic workspace: first full scan and
/// incremental safe-point checkpoint after touching a few files.
fn shadow(c: &mut Criterion) {
    let rt = rt();
    let files = workspace_files();
    let ws = tempdir();
    let t = Instant::now();
    make_workspace(ws.path(), files);
    eprintln!("[setup] workspace with {files} files in {:?}", t.elapsed());

    let mut g = c.benchmark_group(format!("shadow_{files}"));
    g.sample_size(10).measurement_time(Duration::from_secs(20));
    let mut full = vec![];
    g.bench_function("first_full_scan", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let store = tempdir();
                let cp = ShadowCheckpointer::new(ws.path(), store.path()).unwrap();
                let t = Instant::now();
                rt.block_on(cp.checkpoint(&safe_point())).unwrap();
                let dt = t.elapsed();
                full.push(dt);
                total += dt;
            }
            total
        })
    });
    report(&format!("shadow first full scan ({files} files)"), &full, "no budget; once per session");

    let store = tempdir();
    let cp = ShadowCheckpointer::new(ws.path(), store.path()).unwrap();
    rt.block_on(cp.checkpoint(&safe_point())).unwrap();
    let mut round = 0u64;
    let mut inc = vec![];
    g.sample_size(30);
    g.bench_function("incremental_safe_point_3_files", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                round += 1;
                touch_files(ws.path(), files, 3, round);
                let t = Instant::now();
                rt.block_on(cp.checkpoint(&safe_point())).unwrap();
                let dt = t.elapsed();
                inc.push(dt);
                total += dt;
            }
            total
        })
    });
    g.finish();
    report(&format!("shadow incremental safe point ({files} files, 3 touched)"), &inc, "budget p99 < 200 ms");
}

/// (d) resume a 10k-event session: pure fold, and SQLite load + fold.
fn resume(c: &mut Criterion) {
    let rt = rt();
    let session = build_session(10_000);
    eprintln!(
        "[setup] session with {} events, {} turns, {} JSON bytes",
        session.log.len(),
        session.turns(),
        journal_bytes(&session.log)
    );
    let j = rt.block_on(JournalBench::new(&session));
    let sid = SessionId::new("resume");
    rt.block_on(j.write_session(&sid, &session.log));
    let mut g = c.benchmark_group("resume_10k");
    g.sample_size(10);
    g.bench_function("fold", |b| b.iter(|| fold(&session.log)));
    g.bench_function("sqlite_load_and_fold", |b| b.iter(|| rt.block_on(j.load_and_fold(&sid))));
    g.finish();
}

/// (e) framework overhead to first token: Submit -> Sample request at the
/// scripted model port, through the full runtime.
fn first_token(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("first_token");
    g.sample_size(30);
    for kind in [JournalKind::Memory, JournalKind::Sqlite] {
        let mut all = vec![];
        let mut ft = rt.block_on(FirstToken::new(kind, 300));
        g.bench_function(format!("{kind:?}").to_lowercase(), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    if ft.remaining() == 0 {
                        ft = rt.block_on(FirstToken::new(kind, 300));
                    }
                    let dt = rt.block_on(ft.measure());
                    all.push(dt);
                    total += dt;
                }
                total
            })
        });
        report(&format!("first-token overhead ({kind:?} journal)"), &all, "budget p99 < 50 ms");
    }
    g.finish();
}

criterion_group!(benches, kernel, journal, shadow, resume, first_token);
criterion_main!(benches);
