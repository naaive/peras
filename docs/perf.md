# Performance measurements

These are the first measurements against the initial budgets in
[design.md](design.md) ("Testing and observability" / "Performance budget").
The workloads live in `crates/bench` (package `agent-bench`, not published).

## How to run

```sh
# Criterion benches, all five groups (about 6 minutes)
cargo bench -p agent-bench
# one group, e.g. the shadow snapshot at the design's 100k-file scale
AGENT_BENCH_FILES=100000 cargo bench -p agent-bench -- shadow
# Budget test: a few hundred iterations each, asserts p99 budgets in release only
cargo test -p agent-bench --release --test budgets -- --nocapture --test-threads=1
# ...print the numbers without asserting
AGENT_BENCH_REPORT_ONLY=1 cargo test -p agent-bench --release --test budgets -- --nocapture --test-threads=1
```

- `AGENT_BENCH_DIR` sets where SQLite databases and workspaces go (default: the
  system temp dir). Use a real disk: fsync on tmpfs costs nothing.
- `AGENT_BENCH_FILES` sets the size of the synthetic workspace (default 20000).
- Criterion reports means. Each group also prints a `[budget] ... p99=...`
  line to stderr, computed over its own samples.

## Machine

| | |
| --- | --- |
| CPU | Intel(R) Xeon(R) Processor @ 2.80GHz (AVX2, AVX-512), `nproc` = 4 (VM) |
| Memory | 15 GiB |
| Disk | virtio block device, ext4 (fsync goes to the host) |
| OS | Linux 6.18 |
| Toolchain | rustc 1.94.1, release profile (`opt-level = 3`, no LTO) |

This is a shared cloud VM, and other builds were running while it was measured,
so treat tail numbers (p99, max) as upper bounds.

## Results (2026-09-25)

| Metric | Budget | Measured | Verdict |
| --- | --- | --- | --- |
| Kernel decide + evolve per input | p99 < 1 ms | mean 0.52 ms, p50 0.45 ms, **p99 1.69 ms**, max 8.5 ms (n = 15031) | **over budget** |
| Journal append, SQLite WAL + fsync | p99 < 10 ms | mean 1.52 ms, p50 0.84 ms, **p99 14.2 ms**, max 22 ms (n = 7068) | **over budget** |
| Safe-point snapshot, incremental, 20k files | p99 < 200 ms | mean 78 ms, **p99 116 ms** | within budget |
| Safe-point snapshot, incremental, 100k files | p99 < 200 ms | mean 452 ms, **p99 540 ms** | **over budget** (no file watcher) |
| First full scan, 20k / 100k files | (none) | 1.6 s / 13.5 s mean | one-off per store |
| Resume a 10k-event session: fold only | < 1 s | **182 ms** | within budget |
| Resume a 10k-event session: SQLite load + fold | < 1 s | **5.0 s** | **over budget** |
| Framework overhead to first token, in-memory journal | p99 < 50 ms | mean 5.3 ms, **p99 18.9 ms** (n = 1906) | within budget |
| Framework overhead to first token, SQLite journal | p99 < 50 ms | mean 1.5 ms, **p99 7.9 ms** (n = 2836) | within budget |

The budget test (`tests/budgets.rs`, release, 300 to 4000 iterations) gave
the same picture: kernel p99 1.7 to 1.9 ms, append p99 15.4 ms, fold 188 ms,
load + fold 5.3 s, first token p99 7.6 ms (memory) and 11.2 ms (SQLite).

## Workloads

- **Kernel** (`SessionDriver`): the real `Kernel` driven the same way the
  runtime driver does it (decide, then wrap drafts into envelopes, then
  evolve). The session runs 300 to 400 turns of 1 to 6 tool-calling steps
  (single reads, parallel reads and edits, 4 KB read results). It uses the
  default 200k window, so level-2 trims and level-4 summaries happen along the
  way, and it has pre-batch and safe-point checkpoints. Each input is timed
  separately. The p99 comes from `Completed(Sampled)`, `Completed(Executed)`
  and `Submit` inputs late in a context window.
- **Journal** (`JournalBench`): `agent_adapters::Sqlite` on disk
  (`journal_mode=WAL`, `synchronous=FULL`). It appends the recorded session's
  decision batches in order, one `append` per decision, the way the driver
  does.
- **Shadow snapshot**: `agent_runtime::ShadowCheckpointer` on a synthetic
  workspace (about 300-byte files, 200 per directory, plus a `.gitignore`d
  `target/`). The incremental case rewrites 3 files and then takes a
  safe-point checkpoint.
- **Resume**: a 10,059-event session (215 turns). "fold" means
  `Kernel::evolve` over the events already in memory. "load + fold" means
  `SqliteJournal::load` (JSON decode plus read-time upgrade) followed by the
  fold, which is what `Runtime::resume_session` does before dispatching.
- **First token**: the full `Runtime<Kernel>` with the default gate chain and
  a scripted model (`agent_sim::Script`) wrapped so that it stamps the moment
  a request reaches `ModelPort::stream`. The time runs from `Submit` to that
  stamp, which includes decide, the journal append, dispatch and request
  encoding. Each turn is one reply, and the session grows across 300 turns.

## Findings

1. **The journal grows quadratically.** Each `EffectIssued(Sample)` event
   stores the whole prompt inline. In the 10k-event session this adds up to
   348 MB of the journal's 362 MB of JSON (96%). This one cause explains most
   of the misses:
   - SQLite load + fold takes 5 s. Of that, 4.9 s is the load, which reads
     and JSON-decodes about 362 MB.
   - The append tail (p99 14 ms) comes from batches that carry a
     several-hundred-KB prompt.
   - Building and cloning that prompt in `decide` is probably part of the
     kernel's p99 as well. This has not been profiled.

   Storing the sample by reference (sequence head plus the seq range of
   context entries, or a content hash) would make the journal linear and bring
   resume well under 1 s: the fold alone takes 182 ms.
2. **Kernel p99 is 1.7x the budget.** The mean (0.52 ms) is fine. The tail
   comes from inputs late in a context window. It looks linear in the number
   of context entries (prompt assembly, token estimation, cloning
   `Rendered`). This has not been profiled yet.
3. **The shadow scan is linear in workspace size** because it does not use a
   file watcher. At 20k files it takes 78 ms, within budget. At 100k files it
   takes 450 ms, over budget. The design's 100k budget assumes that file
   watching is available, and the watcher has not been implemented yet.
4. **First-token overhead is well within budget.** The SQLite journal is
   faster here than `MemJournal`, because `MemJournal` round-trips every
   appended envelope through JSON by default.

## Golden replay

The fixtures for the golden replay tests are in `crates/sim/tests/golden/`:

- `session.jsonl`: the recorded session, as JSON Lines of `Envelope<Event>`.
  It is produced once by a deterministic `KernelSim` + `Script` scenario that
  includes tools, an approved edit and a denied edit, trims, a summary and a
  rewind. The first `user_message` is stored in the old schema-0 shape, so
  that replay goes through the read-time upgrader.
- `prompt.json`: the final `agent_kernel::current_prompt`.
- `requests.anthropic.jsonl`, `requests.openai.jsonl`: the byte-exact output
  of `AnthropicEncoderV1` and `OpenAiEncoderV1` for every `Sample` in the
  journal.

The checks run in two test files:

- `crates/sim/tests/golden_replay.rs` re-folds the fixture through
  `agent_proto::upgrade::read_envelope`. It checks that every recorded Sample
  prompt equals the prompt rebuilt from the fold, and that the final prompt
  matches byte for byte.
- `crates/bench/tests/golden_replay.rs` checks the vendor encodings. It lives
  in `crates/bench` because agent-sim does not depend on the adapters.

To regenerate the fixtures after an intentional change:

```sh
UPDATE_GOLDEN=1 cargo test -p agent-sim --test golden_replay
UPDATE_GOLDEN=1 cargo test -p agent-bench --test golden_replay
```
