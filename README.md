# peras — Rust Agent Framework

A Rust implementation of **Rust Agent Framework — Architecture Design v2** ([docs/design.md](docs/design.md)): a coding agent
in which the model calls tools in a loop to read and write the workspace, run commands and, when needed, ask a
human for approval. All state is a projection of an append-only event journal, and every decision is made by a
pure kernel that performs no IO.

## Workspace

| Crate | Package | Role | IO |
|---|---|---|---|
| `crates/proto` | `agent-proto` | Events, envelopes & trust, signals/controls, effects, verdicts, resource URIs, protocol messages, JSON Schema export, read-time schema upgrades | none |
| `crates/kernel` | `agent-kernel` | Pure `Decider`: state machine, conflict-graph scheduling, gate rings 1–3, taint derivation, write-time rendering, snapshots, pressure relief, rewind | none |
| `crates/runtime` | `agent-runtime` | Port traits, the driver (log first, then act), stream assembly, effect dispatch, gate chain (hooks, auto rules, CAS asks), shadow checkpointer, recovery, observers, task registry | yes |
| `crates/profile` | `agent-profile` | Five-layer config discovery + pure `compile` into an immutable `Profile` (deny-wins, sensitive fields only tighten from project layers, explain) | reads files |
| `crates/adapters` | `agent-adapters` | Anthropic + OpenAI-compatible model ports with frozen encoders, retry/rate-limit/meter layers, SQLite journal & blobs, memory store, sandbox probe (bubblewrap / seatbelt / direct / container) | yes |
| `crates/tools` | `agent-tools` | Capability handles (`Read<File>`, `Write<File>`, `Get<Url>`…), built-in tools, `bash` with shell analysis + semantic table, MCP client | yes |
| `crates/macros` | `agent-macros` | `#[tool]`, `#[agent::test]` | none |
| `crates/server` | `agent-server` | Session service: multi-client fan-out over in-process / JSON Lines / WebSocket transports | yes |
| `crates/sdk` | `agent` | Application facade (`use agent::prelude::*`) | yes |
| `crates/sim` | `agent-sim` | Virtual clock, scripted model, controllable scheduler, kernel simulation with crash injection | none |
| `crates/cli` | `agent-cli` | `agent` binary: headless JSONL runs, `schema`, `replay`, `context explain`, `doctor`, `config explain` | yes |

Dependencies only point inwards (`proto` ← `kernel` ← `runtime` ← adapters/tools/sdk). CI enforces the
kernel boundary (`scripts/check-kernel-purity.sh`): no tokio/reqwest in its dependency tree, no filesystem,
clock or hash-ordered containers in its sources, and no closures in its public API.

## Usage

```rust
use agent::prelude::*;

#[tool]
/// Read a file
async fn read_file(file: Read<File>) -> Result<String> { file.text().await }

let agent = Agent::new(Claude::default().retry(3))
    .tools((read, edit, Bash))
    .policy("agent.toml")
    .journal(Sqlite("runs.db"))
    .gate(|p: &Proposal| {                         // in-process hook, ring 4
        if p.writes(".github/**") { Verdict::ask("CI config change") } else { Verdict::Allow }
    })
    .observe(|e: &ToolFailed| tracing::warn!(%e)); // the parameter type is the filter

let summary = agent.run("Summarize this repository").await?;
let plan: Plan = agent.run("Break it into tasks").json().await?;

let mut run = agent.run("Fix the failing tests");
run.control().steer("Don't touch the public API");
while let Some(u) = run.next().await {
    match u { Update::Text(t) => print!("{t}"), Update::Ask(ask) => ask.allow(), _ => {} }
}

let chat = agent.session("pr-1234");               // created if missing, resumed otherwise
chat.send("Read the diff first").await?;
let report = chat.rewind(seq).await?;              // workspace rewind report

let reviewer = Agent::new(Claude::default()).tools((read,)).named("reviewer").describe("Review the diff");
let lead = Agent::new(Claude::default()).tools((read, edit, reviewer)); // a sub-agent is a tool

let ci = Agent::discover(".").unattended(OnAsk::Defer).sandbox(Container::ephemeral());
```

Tests use a scripted model and a temporary workspace:

```rust
#[agent::test]
async fn edits_readme() -> anyhow::Result<()> {
    let model = Script::new()
        .call(edit, json!({ "file": "README.md", "old": "foo", "new": "bar" }))
        .say("Done");
    let out = Agent::new(model).tools((edit,)).run("Edit README").await?;
    assert_eq!(out, "Done");
    Ok(())
}
```

## CLI

```sh
cargo run -p agent-cli -- run "fix the failing test" --on-ask defer   # JSON Lines; exit 20 = suspended
cargo run -p agent-cli -- run --resume <session> "continue"
cargo run -p agent-cli -- replay <session> --until 42                 # offline state + next request
cargo run -p agent-cli -- context explain <session>                   # where every part of the request came from
cargo run -p agent-cli -- doctor                                      # sandbox probe, credentials, config warnings
cargo run -p agent-cli -- config explain model.id                     # final value + source layer
cargo run -p agent-cli -- schema                                      # protocol JSON Schema
```

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/check-kernel-purity.sh
```
