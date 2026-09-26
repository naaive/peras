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

## Results

### After: requests journaled by reference (event schema 2)

`EffectIssued` for a sample now stores `SampleRef { seq_no, entries,
max_tokens }` and a compaction stores `CompactRef` (the same plus the
instruction text), instead of the whole prompt. The kernel rebuilds the
request from the fold (see [design.md](design.md), "Journal"). The kernel also
stopped deep-copying session-long lists on every `decide` (context
operations, checkpoints, irreversible calls and the event index are now
chunked and shared), and the trim planner is linear in the context length.

Criterion (`cargo bench -p agent-bench`), same machine:

| Metric | Budget | Before | After | Verdict |
| --- | --- | --- | --- | --- |
| Kernel decide + evolve per input | p99 < 1 ms | mean 0.52 ms, p99 1.69 ms, max 8.5 ms | mean 0.092 ms, p50 0.066 ms, **p99 0.33 ms**, max 7.6 ms (n = 79101) | within budget |
| Journal append, SQLite WAL + fsync | p99 < 10 ms | mean 1.52 ms, p99 14.2 ms, max 22 ms | mean 0.45 ms, p50 0.39 ms, **p99 1.5 ms**, max 25 ms (n = 27256) | within budget |
| Resume a 10k-event session: fold only | < 1 s | 182 ms | **21.7 ms** | within budget |
| Resume a 10k-event session: SQLite load + fold | < 1 s | 5.0 s | **178 ms** | within budget |
| Framework overhead to first token, in-memory journal | p99 < 50 ms | mean 5.3 ms, p99 18.9 ms | mean 0.70 ms, **p99 2.7 ms** | within budget |
| Framework overhead to first token, SQLite journal | p99 < 50 ms | mean 1.5 ms, p99 7.9 ms | mean 1.2 ms, **p99 4.0 ms** | within budget |
| Journal size, 10,059-event session | (none) | 362 MB (348 MB in sample effects) | **10.5 MB** (0.28 MB in sample/compact effects) | linear |

The shadow snapshot is unaffected by this change (see below).

The budget test (`tests/budgets.rs`, release, `AGENT_BENCH_REPORT_ONLY=1`)
agrees: kernel p99 0.34 ms (0.46 ms before the chunked lists, 1.7 to 1.9 ms
before the change), append p99 1.1 ms (was 15.4 ms), fold 21 ms (was 188 ms),
load + fold 182 ms (was 5.3 s), of which 165 ms is the SQLite load and JSON
decode, first token p99 1.4 ms (memory) and 2.7 ms (SQLite).

The kernel max (a few ms, on a handful of inputs out of tens of thousands)
has not been investigated; this VM is shared, see "Machine". Per input kind,
`Completed(Executed)` has the highest p99 (0.44 ms): it appends 4 KB tool
results and runs the pressure checks.

### Before (2026-09-25, event schema 1)

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

1. **The journal grew quadratically (fixed).** Each `EffectIssued(Sample)`
   event stored the whole prompt inline: 348 MB of the 10k-event session's
   362 MB of JSON (96%). That explained most of the misses: 4.9 s of the 5 s
   resume was reading and decoding the JSON, the append tail came from batches
   carrying a several-hundred-KB prompt, and `decide` deep-copied the prompt
   into the event and `evolve` copied it again into the state. Samples and
   compactions are now journaled by reference (event schema 2); the read-time
   upgrader converts old `effect_issued` events, so old journals still load.
   The dispatched request, the one `outstanding` re-dispatches after a crash
   and the one the debug consistency check derives are all rebuilt from the
   fold and compared byte for byte in the kernel tests, the fault-injection
   test and the golden replay.
2. **Kernel per-input cost (fixed).** Besides the prompt copies, `decide`
   clones the state for every input, and some lists in it grew with the whole
   session (context operations, checkpoints, the event-id index, which was
   also re-copied in full every 512 events). They are now split into shared
   chunks, so a clone is proportional to the live context, not to the session.
   The fold of 10k events went from 182 ms to 21 ms for the same reasons.
3. **The shadow scan is linear in workspace size** because it does not use a
   file watcher. At 20k files it takes 78 ms, within budget. At 100k files it
   takes 450 ms, over budget. The design's 100k budget assumes that file
   watching is available, and the watcher has not been implemented yet.
4. **First-token overhead is well within budget.** The SQLite journal was
   faster here than `MemJournal` before the change, because `MemJournal`
   round-trips every appended envelope through JSON by default and the
   envelopes carried the prompt.

## Golden replay

The fixtures for the golden replay tests are in `crates/sim/tests/golden/`:

- `session.jsonl`: the recorded session, as JSON Lines of `Envelope<Event>`.
  It was produced once by a deterministic `KernelSim` + `Script` scenario that
  includes tools, an approved edit and a denied edit, trims, a summary and a
  rewind. It is an old recording (event schema 1, every sample and compaction
  with its full prompt) and is never regenerated: replay goes through the
  read-time upgrader. The first `user_message` is stored in the schema-0
  shape.
- `prompt.json`: the final `agent_kernel::current_prompt`.
- `requests.anthropic.jsonl`, `requests.openai.jsonl`: the byte-exact output
  of `AnthropicEncoderV1` and `OpenAiEncoderV1` for every `Sample` in the
  journal.

The checks run in two test files:

- `crates/sim/tests/golden_replay.rs` re-folds the fixture through
  `agent_proto::upgrade::read_envelope`. It checks that every sample and
  compaction, upgraded to a `SampleRef` / `CompactRef`, rebuilds from the fold
  (and from `Kernel::outstanding`) to exactly the prompt stored in the old
  recording, that running the scenario today journals exactly the upgraded
  fixture, and that the final prompt matches byte for byte.
- `crates/bench/tests/golden_replay.rs` checks the vendor encodings. It lives
  in `crates/bench` because agent-sim does not depend on the adapters.

To regenerate the expected outputs after an intentional change
(`session.jsonl` is only created when missing):

```sh
UPDATE_GOLDEN=1 cargo test -p agent-sim --test golden_replay
UPDATE_GOLDEN=1 cargo test -p agent-bench --test golden_replay
```
