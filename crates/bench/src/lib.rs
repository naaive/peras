//! `agent-bench`: workloads for the performance budgets of `docs/design.md`
//! ("Performance budget"), shared by the criterion benches (`benches/`) and the
//! budget test (`tests/budgets.rs`). Results: `docs/perf.md`.
//!
//! | Budget | Workload |
//! | --- | --- |
//! | kernel decide + evolve, p99 < 1 ms | [`SessionDriver`] (realistic session, hundreds of turns with tool calls) |
//! | journal append incl. fsync, p99 < 10 ms | [`JournalBench`] (`agent_adapters::Sqlite` on disk) |
//! | safe-point snapshot, p99 < 200 ms | [`make_workspace`] + `agent_runtime::ShadowCheckpointer` |
//! | recover a 10k-event session, < 1 s | [`build_session`] + [`fold`] / [`JournalBench::load_and_fold`] |
//! | framework overhead to first token, p99 < 50 ms | [`FirstToken`] (full runtime + `agent_sim::Script`) |
//!
//! Environment variables:
//! - `AGENT_BENCH_FILES`: files in the synthetic workspace (default 20000; the
//!   design budget is stated for 100000).
//! - `AGENT_BENCH_DIR`: directory for SQLite databases and workspaces (default:
//!   the system temp dir). Point it at a real disk: fsync on tmpfs is free.

use agent_kernel::{start_session, Decider, Decision, Kernel, State};
use agent_proto::*;
use agent_runtime::{
    Encoder, JournalStore, ModelPort, Request, Runtime, RuntimeOptions, SeqIdGen, SessionHandle, ToolRegistry,
};
use agent_sim::Script;
use futures::stream::BoxStream;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- stats

/// Latency summary of a sample set.
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub n: usize,
    pub mean: Duration,
    pub p50: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Summary {
    pub fn of(samples: &[Duration]) -> Summary {
        assert!(!samples.is_empty(), "no samples");
        let mut v = samples.to_vec();
        v.sort();
        let pct = |p: f64| v[(((v.len() - 1) as f64) * p).round() as usize];
        let total: Duration = v.iter().sum();
        Summary { n: v.len(), mean: total / v.len() as u32, p50: pct(0.50), p99: pct(0.99), max: *v.last().unwrap() }
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "n={} mean={:?} p50={:?} p99={:?} max={:?}", self.n, self.mean, self.p50, self.p99, self.max)
    }
}

/// Base directory for on-disk fixtures (`AGENT_BENCH_DIR` or the temp dir).
pub fn bench_dir() -> PathBuf {
    std::env::var_os("AGENT_BENCH_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn tempdir() -> tempfile::TempDir {
    let base = bench_dir();
    std::fs::create_dir_all(&base).expect("bench dir");
    tempfile::Builder::new().prefix("agent-bench-").tempdir_in(base).expect("tempdir")
}

// ---------------------------------------------------------------- kernel session

pub const WS: &str = "/ws";

/// A realistic coding-agent configuration: default 200k window, read / grep /
/// edit tools, workspace writes allowed by a rule (checkpoints still run),
/// per-turn call budgets off so long sessions never stop early.
pub fn kernel_config() -> KernelConfig {
    let mut c = KernelConfig::default();
    c.security.workspace_root = WS.into();
    c.security.workspace_trusted = true;
    c.system = vec!["You are a coding agent working in /ws.".into(), "Prefer small, verified edits.".into()];
    let tool = |name: &str, description: &str, class| ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":{"file":{"type":"string"}}}),
        class,
        subagent: false,
    };
    c.tools = vec![
        tool("read", "Read a file.", EffectClass::Pure),
        tool("grep", "Search the workspace.", EffectClass::Pure),
        tool("edit", "Replace text in a file.", EffectClass::LocalWrite),
    ];
    c.rules.push(PolicyRule {
        name: "allow-workspace".into(),
        resource: Some("fs:///ws/**".into()),
        tool: None,
        mode: None,
        action: PolicyAction::Allow,
        layer: Layer::Cli,
    });
    c.budgets.max_calls_per_turn = 0;
    c.budgets.max_repeat_calls = 0;
    c
}

fn read_call(id: String, path: String) -> ToolCall {
    ToolCall {
        id: CallId(id),
        name: "read".into(),
        input: json!({ "file": path }),
        access: vec![Access::read(ResourceUri::fs(&path))],
        class: EffectClass::Pure,
    }
}

fn edit_call(id: String, path: String) -> ToolCall {
    ToolCall {
        id: CallId(id),
        name: "edit".into(),
        input: json!({ "file": path, "old": "a", "new": "b" }),
        access: vec![Access::write(ResourceUri::fs(&path))],
        class: EffectClass::LocalWrite,
    }
}

fn message(text: &str, calls: Vec<ToolCall>, input_tokens: u32) -> AssistantMessage {
    let mut content = vec![];
    if !text.is_empty() {
        content.push(ContentBlock::Text { text: text.into() });
    }
    let stop = if calls.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse };
    content.extend(calls.into_iter().map(ContentBlock::ToolUse));
    AssistantMessage { content, stop, usage: Usage { input_tokens, output_tokens: 40, ..Default::default() } }
}

/// A synthetic source file of roughly `bytes` bytes.
fn file_text(n: u64, bytes: usize) -> String {
    let mut s = String::with_capacity(bytes + 64);
    let mut i = 0;
    while s.len() < bytes {
        s.push_str(&format!("fn item_{n}_{i}(x: u32) -> u32 {{ x.wrapping_mul({i}) }}\n"));
        i += 1;
    }
    s
}

fn input_kind(i: &Input) -> &'static str {
    match i {
        Input::Signal(Signal::Submit { .. }) => "submit",
        Input::Completed(_, r) => match r {
            EffectResult::Sampled(_) => "sampled",
            EffectResult::Executed(_) => "executed",
            EffectResult::Gated { .. } => "gated",
            EffectResult::Checkpointed(_) => "checkpointed",
            EffectResult::Compacted { .. } => "compacted",
            _ => "other_result",
        },
        _ => "other",
    }
}

/// Drives the real kernel through a realistic session the way the runtime
/// driver does (decide -> envelopes -> evolve), resolving every effect with a
/// deterministic world. Each input is timed (decide + envelope + evolve).
pub struct SessionDriver {
    pub state: State,
    pub log: Vec<Envelope<Event>>,
    /// Event count of each decision, in order (journal append batches).
    pub batches: Vec<usize>,
    /// decide + evolve time of every input fed so far.
    pub timings: Vec<Duration>,
    /// Kind of each timed input (`submit`, `sampled`, `executed`, ...).
    pub kinds: Vec<&'static str>,
    pending: Vec<(EffectId, Effect)>,
    at: u64,
    calls: u64,
    samples_in_turn: u32,
    turns: u64,
}

impl Default for SessionDriver {
    fn default() -> Self {
        Self::new(kernel_config())
    }
}

impl SessionDriver {
    pub fn new(config: KernelConfig) -> SessionDriver {
        let mut d = SessionDriver {
            state: State::default(),
            log: vec![],
            batches: vec![],
            timings: vec![],
            kinds: vec![],
            pending: vec![],
            at: 1_750_000_000_000,
            calls: 0,
            samples_in_turn: 0,
            turns: 0,
        };
        d.apply(start_session(SessionId::new("bench"), "bench-profile".into(), config));
        d
    }

    fn apply(&mut self, decision: Decision) {
        self.batches.push(decision.events.len());
        for draft in decision.events {
            let seq = self.log.len() as u64;
            let parent = match draft.parent {
                Parent::Head => self.log.last().map(|e| e.id.clone()),
                Parent::Explicit(p) => Some(p),
            };
            let env = Envelope {
                id: EventId(format!("ev{seq:08}")),
                parent,
                seq,
                at: Timestamp(self.at),
                origin: draft.origin,
                trust: draft.trust,
                audience: draft.audience,
                schema: EVENT_SCHEMA,
                body: draft.body,
                rendered: draft.rendered,
            };
            Kernel::evolve(&mut self.state, &env);
            self.log.push(env);
        }
        self.pending.extend(decision.effects);
    }

    /// Feed one input; returns the time spent in decide + evolve (or `None`
    /// if the kernel rejected it).
    pub fn input(&mut self, input: Input) -> Option<Duration> {
        self.at += 7;
        let kind = input_kind(&input);
        let t = Instant::now();
        let decision = Kernel::decide(&self.state, Timestamp(self.at), input).ok()?;
        self.apply(decision);
        let dt = t.elapsed();
        self.timings.push(dt);
        self.kinds.push(kind);
        Some(dt)
    }

    /// The scripted model: `tool_steps` tool-calling replies (reads, parallel
    /// reads and edits), then a final answer. Reports input tokens like a
    /// vendor does (estimated from the prompt).
    fn model_reply(&mut self, prompt: &Prompt, tool_steps: u32) -> AssistantMessage {
        self.samples_in_turn += 1;
        let input_tokens = 600 + prompt.body.iter().map(|r| r.tokens).sum::<u32>();
        if self.samples_in_turn > tool_steps {
            return message("Done: the change is in place and the tests pass.", vec![], input_tokens);
        }
        let n = self.calls;
        let file = |k: u64| format!("{WS}/src/mod_{}/file_{}.rs", k % 17, k % 101);
        let calls = match self.samples_in_turn % 3 {
            1 => vec![read_call(format!("c{n}"), file(n))],
            2 => vec![read_call(format!("c{n}"), file(n)), read_call(format!("c{}", n + 1), file(n + 1))],
            _ => vec![edit_call(format!("c{n}"), file(n))],
        };
        self.calls += calls.len() as u64;
        message("", calls, input_tokens)
    }

    fn resolve(&mut self, effect: &Effect, tool_steps: u32) -> Option<EffectResult> {
        Some(match effect {
            Effect::Sample(p) => EffectResult::Sampled(self.model_reply(p, tool_steps)),
            Effect::Execute(b) => EffectResult::Executed(
                b.calls
                    .iter()
                    .map(|c| {
                        let text = if c.name == "read" { file_text(self.calls, 4_000) } else { "ok: 1 replacement".into() };
                        ToolResult::text(c.id.clone(), text, false)
                    })
                    .collect(),
            ),
            Effect::Gate(_) => {
                EffectResult::Gated { verdict: Verdict::Allow, responder: Responder::Human("bench".into()), remember: false }
            }
            Effect::Checkpoint(_) => EffectResult::Checkpointed(CheckpointInfo {
                id: CheckpointId::new(format!("cp{}", self.log.len())),
                agent_changes: vec![],
                external_changes: vec![],
            }),
            Effect::Compact(_) => EffectResult::Compacted {
                summary: "Summary: the user is refactoring src/mod_*/ and all edits so far are verified.".into(),
                trust: Trust::Internal,
            },
            Effect::Restore(_) => EffectResult::Restored(RestoreReport::default()),
            Effect::Finish(_) => return None,
            Effect::SampleRef(_) | Effect::CompactRef(_) => panic!("journal reference dispatched: {effect:?}"),
        })
    }

    /// One user turn with `tool_steps` tool-calling model replies. Returns the
    /// number of inputs fed.
    pub fn turn(&mut self, tool_steps: u32) -> usize {
        self.turns += 1;
        self.samples_in_turn = 0;
        let before = self.timings.len();
        let text = format!("Task {}: refactor the next module and run the tests.", self.turns);
        self.input(Input::Signal(Signal::Submit { text, attachments: vec![] })).expect("submit accepted");
        let mut guard = 0;
        while !self.pending.is_empty() {
            guard += 1;
            assert!(guard < 10_000, "turn does not terminate");
            let (id, effect) = self.pending.remove(0);
            if let Some(r) = self.resolve(&effect, tool_steps) {
                self.input(Input::Completed(id, r));
            }
        }
        self.timings.len() - before
    }

    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// Timings of one input kind.
    pub fn timings_of(&self, kind: &str) -> Vec<Duration> {
        self.timings.iter().zip(&self.kinds).filter(|(_, k)| **k == kind).map(|(t, _)| *t).collect()
    }
}

/// Turn shape used everywhere: 1..=6 tool-calling steps.
pub fn tool_steps_for(turn: u64) -> u32 {
    1 + (turn % 6) as u32
}

/// A realistic session of at least `min_events` events.
pub fn build_session(min_events: usize) -> SessionDriver {
    let mut d = SessionDriver::default();
    while d.log.len() < min_events {
        let t = d.turns();
        d.turn(tool_steps_for(t));
    }
    d
}

/// Recovery: fold a journal into a fresh state.
pub fn fold(events: &[Envelope<Event>]) -> State {
    let mut s = State::default();
    for e in events {
        Kernel::evolve(&mut s, e);
    }
    s
}

/// Serialized size of a journal in bytes (JSON, as stored).
pub fn journal_bytes(events: &[Envelope<Event>]) -> usize {
    events.iter().map(|e| serde_json::to_string(e).map(|s| s.len()).unwrap_or(0)).sum()
}

/// Bytes of the `EffectIssued` events of samples and compactions (journaled by
/// reference since event schema 2; before that each carried its full prompt).
pub fn sample_effect_bytes(events: &[Envelope<Event>]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                Event::EffectIssued {
                    effect: Effect::Sample(_) | Effect::SampleRef(_) | Effect::Compact(_) | Effect::CompactRef(_),
                    ..
                }
            )
        })
        .map(|e| serde_json::to_string(e).map(|s| s.len()).unwrap_or(0))
        .sum()
}

// ---------------------------------------------------------------- journal

/// SQLite journal on disk (WAL, synchronous=FULL), appending the decision
/// batches of a recorded session in order, like the driver does.
pub struct JournalBench {
    pub store: agent_adapters::Sqlite,
    events: Vec<Envelope<Event>>,
    batches: Vec<usize>,
    session: u32,
    lease: LeaseGen,
    batch: usize,
    offset: usize,
    _dir: tempfile::TempDir,
}

impl JournalBench {
    pub async fn new(recorded: &SessionDriver) -> JournalBench {
        let dir = tempdir();
        let store = agent_adapters::Sqlite::open(dir.path().join("journal.db")).expect("open sqlite");
        let mut b = JournalBench {
            store,
            events: recorded.log.clone(),
            batches: recorded.batches.iter().copied().filter(|n| *n > 0).collect(),
            session: 0,
            lease: LeaseGen(0),
            batch: 0,
            offset: 0,
            _dir: dir,
        };
        b.next_session().await;
        b
    }

    fn sid(&self) -> SessionId {
        SessionId::new(format!("bench-{}", self.session))
    }

    async fn next_session(&mut self) {
        self.session += 1;
        self.batch = 0;
        self.offset = 0;
        self.lease = self.store.journal.acquire_lease(&self.sid()).await.expect("lease");
    }

    /// Append the next decision batch; returns the append latency.
    pub async fn append_next(&mut self) -> Duration {
        if self.batch >= self.batches.len() {
            self.next_session().await;
        }
        let n = self.batches[self.batch];
        let slice = &self.events[self.offset..self.offset + n];
        let sid = self.sid();
        let t = Instant::now();
        self.store.journal.append(&sid, self.lease, self.offset as Seq, slice).await.expect("append");
        let dt = t.elapsed();
        self.batch += 1;
        self.offset += n;
        dt
    }

    /// Write a whole session in one go (fixture for the resume benchmark).
    pub async fn write_session(&self, sid: &SessionId, events: &[Envelope<Event>]) {
        let lease = self.store.journal.acquire_lease(sid).await.expect("lease");
        for (i, chunk) in events.chunks(500).enumerate() {
            self.store.journal.append(sid, lease, (i * 500) as Seq, chunk).await.expect("append");
        }
    }

    /// What `Runtime::resume_session` does before dispatching: load (JSON
    /// decode + read-time upgrade) and fold.
    pub async fn load_and_fold(&self, sid: &SessionId) -> (usize, State) {
        let events = self.store.journal.load(sid, 0).await.expect("load");
        let s = fold(&events);
        (events.len(), s)
    }

    /// Only the load part of [`JournalBench::load_and_fold`].
    pub async fn load(&self, sid: &SessionId) -> usize {
        self.store.journal.load(sid, 0).await.expect("load").len()
    }
}

// ---------------------------------------------------------------- workspace

/// Files in the synthetic workspace (`AGENT_BENCH_FILES`, default 20000).
pub fn workspace_files() -> usize {
    std::env::var("AGENT_BENCH_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000)
}

fn ws_file(root: &Path, i: usize) -> PathBuf {
    root.join(format!("pkg{:03}", i / 2_000)).join(format!("mod{:03}", (i / 200) % 10)).join(format!("f{i:06}.rs"))
}

/// Create `files` small (~300 byte) source files under `root` (200 per
/// directory), a `.gitignore` and an ignored `target/` with a few files.
pub fn make_workspace(root: &Path, files: usize) {
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    for i in 0..20 {
        std::fs::write(root.join("target").join(format!("obj{i}.o")), vec![0u8; 512]).unwrap();
    }
    for i in 0..files {
        let path = ws_file(root, i);
        if i % 200 == 0 {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(path, file_text(i as u64, 300)).unwrap();
    }
}

/// Rewrite `k` files (content and size change) to simulate agent edits.
pub fn touch_files(root: &Path, files: usize, k: usize, round: u64) {
    for j in 0..k {
        let i = (round as usize * 7_919 + j * 104_729) % files.max(1);
        let text = format!("// round {round}\n{}", file_text(i as u64 + round, 300 + (round % 50) as usize));
        std::fs::write(ws_file(root, i), text).unwrap();
    }
}

pub fn safe_point() -> CheckpointScope {
    CheckpointScope { declared_writes: vec![], safe_point: true }
}

// ---------------------------------------------------------------- first token

/// Wraps the scripted model and records when a request reaches the port.
struct Stamped {
    inner: Script,
    hit: Arc<Mutex<Option<Instant>>>,
}

impl ModelPort for Stamped {
    fn caps(&self) -> &ModelCaps {
        self.inner.caps()
    }
    fn encoder(&self) -> &dyn Encoder {
        self.inner.encoder()
    }
    fn stream(&self, req: Request) -> BoxStream<'_, Result<agent_runtime::Delta, ModelError>> {
        let now = Instant::now();
        self.hit.lock().unwrap().get_or_insert(now);
        self.inner.stream(req)
    }
}

/// Which journal the first-token harness writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalKind {
    Memory,
    /// SQLite on disk (WAL + fsync per append), like a real deployment.
    Sqlite,
}

/// Full runtime (`Runtime<Kernel>`, default gate chain) with a scripted model:
/// measures Submit -> the `Sample` request reaching the model port.
pub struct FirstToken {
    _rt: Runtime<Kernel>,
    handle: SessionHandle<Kernel>,
    hit: Arc<Mutex<Option<Instant>>>,
    _dir: Option<tempfile::TempDir>,
    turns: usize,
}

impl FirstToken {
    /// A fresh session able to run `turns` single-reply turns.
    pub async fn new(kind: JournalKind, turns: usize) -> FirstToken {
        let mut script = Script::new();
        for i in 0..turns {
            script = script.say(format!("Answer {i}: nothing to change."));
        }
        let hit = Arc::new(Mutex::new(None));
        let caps = script.caps().clone();
        let model = Arc::new(Stamped { inner: script, hit: hit.clone() });
        let (journal, dir): (Arc<dyn JournalStore>, _) = match kind {
            JournalKind::Memory => (Arc::new(agent_runtime::MemJournal::new()), None),
            JournalKind::Sqlite => {
                let dir = tempdir();
                let s = agent_adapters::Sqlite::open(dir.path().join("journal.db")).expect("sqlite");
                (s.journal, Some(dir))
            }
        };
        let rt = Runtime::<Kernel>::builder()
            .journal(journal)
            .model(model)
            .tools(ToolRegistry::new(WS))
            .ids(Arc::new(SeqIdGen::new()))
            .options(RuntimeOptions { workspace: WS.into(), ..RuntimeOptions::default() })
            .build();
        let mut cfg = kernel_config();
        cfg.caps = caps;
        cfg.tools.clear();
        let sid = SessionId::new("first-token");
        let handle =
            rt.create_session(sid.clone(), start_session(sid, "bench".into(), cfg)).await.expect("create session");
        FirstToken { _rt: rt, handle, hit, _dir: dir, turns }
    }

    /// Turns left in the script.
    pub fn remaining(&self) -> usize {
        self.turns
    }

    /// Run one turn; returns Submit -> request at the model port.
    pub async fn measure(&mut self) -> Duration {
        assert!(self.turns > 0, "script exhausted");
        self.turns -= 1;
        *self.hit.lock().unwrap() = None;
        let t0 = Instant::now();
        let out = self
            .handle
            .run(Input::Signal(Signal::Submit { text: "What does main.rs do?".into(), attachments: vec![] }))
            .await
            .expect("turn");
        assert!(matches!(out, TurnOutcome::Done { .. }), "{out:?}");
        let hit = self.hit.lock().unwrap().expect("model was called");
        hit.duration_since(t0)
    }
}
