//! `agent`: headless client and developer commands.
//!
//! - `agent run <prompt>`: runs a turn and prints the user-visible event stream
//!   as JSON Lines (for CI). A suspended approval exits with code 20; continue
//!   later with `agent run --resume <session> [prompt]`.
//! - `agent schema`: JSON Schema of every protocol type.
//! - `agent replay <session> [--until <seq>]`: fold the journal offline and print
//!   the state and the request the next sample would send.
//! - `agent context explain <session> [--until <seq>]`: where each part of the
//!   request came from.
//! - `agent doctor`: sandbox probe, credentials, configuration warnings.
//! - `agent config explain <key>`: final value and the layer it came from.

use agent::kernel::{Decider, Kernel};
use agent::prelude::*;
use agent::proto::{exit_code, Envelope, Event, SessionId};
use agent::runtime::JournalStore;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "agent", version, about = "Rust coding-agent framework CLI")]
struct Cli {
    /// Journal database.
    #[arg(long, global = true, default_value = ".agent/runs.db")]
    db: PathBuf,
    /// Workspace directory.
    #[arg(long, short = 'C', global = true, default_value = ".")]
    dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a prompt headless; prints JSON Lines.
    Run {
        prompt: Option<String>,
        /// Continue an existing session (e.g. after a suspension).
        #[arg(long)]
        resume: Option<String>,
        /// Unattended: what to do with asks (allow|deny|defer).
        #[arg(long, default_value = "defer")]
        on_ask: String,
        /// Print pulses (token deltas) too.
        #[arg(long)]
        pulses: bool,
    },
    /// Print the JSON Schemas of the protocol.
    Schema,
    /// Fold a session offline and print state + next request.
    Replay {
        session: String,
        #[arg(long)]
        until: Option<u64>,
    },
    /// Context tools.
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Environment diagnostics.
    Doctor,
    /// Configuration tools.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// List sessions in the journal.
    Sessions,
}

#[derive(Subcommand)]
enum ContextCmd {
    /// Where each part of the request came from.
    Explain {
        session: String,
        #[arg(long)]
        until: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Final value of a key and the layer it came from.
    Explain { key: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match real_main(cli).await {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(exit_code::FAILED as u8)
        }
    }
}

fn parse_on_ask(s: &str) -> anyhow::Result<OnAsk> {
    Ok(match s {
        "allow" => OnAsk::Allow,
        "deny" => OnAsk::Deny,
        "defer" => OnAsk::Defer,
        other => anyhow::bail!("--on-ask must be allow|deny|defer, got `{other}`"),
    })
}

async fn load_events(db: &PathBuf, session: &str) -> anyhow::Result<Vec<Envelope<Event>>> {
    let s = agent::adapters::Sqlite::open(db).map_err(|e| anyhow::anyhow!("{e}"))?;
    let evs = s.journal.load(&SessionId::new(session), 0).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    if evs.is_empty() {
        anyhow::bail!("session `{session}` not found in {}", db.display());
    }
    Ok(evs)
}

fn fold(events: &[Envelope<Event>], until: Option<u64>) -> agent::kernel::State {
    let mut s = agent::kernel::State::default();
    for e in events.iter().filter(|e| until.is_none_or(|u| e.seq <= u)) {
        Kernel::evolve(&mut s, e);
    }
    s
}

fn println_json(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}

async fn real_main(cli: Cli) -> anyhow::Result<i32> {
    match cli.cmd {
        Cmd::Schema => {
            use std::io::Write;
            // Ignore EPIPE (`agent schema | head`).
            let _ = writeln!(std::io::stdout().lock(), "{}", agent::proto::schema::export_json());
            Ok(exit_code::OK)
        }
        Cmd::Sessions => {
            let s = agent::adapters::Sqlite::open(&cli.db).map_err(|e| anyhow::anyhow!("{e}"))?;
            for id in s.journal.list_sessions().await.map_err(|e| anyhow::anyhow!("{e}"))? {
                println!("{id}");
            }
            Ok(exit_code::OK)
        }
        Cmd::Replay { session, until } => {
            let events = load_events(&cli.db, &session).await?;
            let s = fold(&events, until);
            let out = serde_json::json!({
                "session": session,
                "events": events.iter().filter(|e| until.is_none_or(|u| e.seq <= u)).count(),
                "phase": format!("{:?}", agent::kernel::phase(&s)),
                "tainted": agent::kernel::is_tainted(&s),
                "epoch": agent::kernel::epoch(&s),
                "pending_questions": agent::kernel::pending_questions(&s),
                "outstanding": Kernel::outstanding(&s).iter().map(|(id, e)| format!("{id} {}", e.kind())).collect::<Vec<_>>(),
                "next_request": agent::kernel::current_prompt(&s),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(exit_code::OK)
        }
        Cmd::Context { cmd: ContextCmd::Explain { session, until } } => {
            let events = load_events(&cli.db, &session).await?;
            let s = fold(&events, until);
            if let Some(head) = agent::kernel::current_head(&s) {
                println!(
                    "static  seq#{} model={} encoder=v{} system={} tools={}",
                    head.seq_no,
                    head.model,
                    head.encoder_version,
                    head.system.len(),
                    head.tools.len()
                );
            }
            for c in agent::kernel::context_sources(&s) {
                let text = serde_json::to_string(&c.rendered.blocks)?;
                let preview: String = text.chars().take(100).collect();
                println!(
                    "{:>6}  {:<22} {:>6} tok  {:?}  {}",
                    c.seq, c.kind, c.rendered.tokens, c.rendered.role, preview
                );
            }
            Ok(exit_code::OK)
        }
        Cmd::Doctor => {
            let r = agent::adapters::probe();
            println!("sandbox: {} (available: {}, isolation: {}, egress proxy: {})", r.implementation, r.available, r.isolation, r.egress_proxy);
            for n in &r.notes {
                println!("  note: {n}");
            }
            if !r.available {
                println!("  => without a sandbox every bash command is treated as Opaque and needs approval");
            }
            println!("ANTHROPIC_API_KEY: {}", if std::env::var_os("ANTHROPIC_API_KEY").is_some() { "set" } else { "missing" });
            match Agent::discover(&cli.dir).profile().await {
                Ok(p) => {
                    println!("profile: {} (model {})", &p.hash[..12.min(p.hash.len())], p.kernel.caps.model);
                    for w in &p.warnings {
                        println!("  warning: {w:?}");
                    }
                }
                Err(e) => println!("profile: error: {e}"),
            }
            Ok(exit_code::OK)
        }
        Cmd::Config { cmd: ConfigCmd::Explain { key } } => {
            let p = Agent::discover(&cli.dir).profile().await?;
            match p.explain(&key) {
                Some(x) => {
                    println_json(x);
                    Ok(exit_code::OK)
                }
                None => {
                    eprintln!("no such key `{key}`; known keys:");
                    for k in p.explain.keys() {
                        eprintln!("  {k}");
                    }
                    Ok(exit_code::FAILED)
                }
            }
        }
        Cmd::Run { prompt, resume, on_ask, pulses } => {
            if let Some(parent) = cli.db.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let agent = Agent::discover(&cli.dir).journal(Sqlite(cli.db.clone())).unattended(parse_on_ask(&on_ask)?);
            let prompt = prompt.unwrap_or_else(|| "continue".to_string());
            let mut run = match &resume {
                Some(id) => agent.session(id.clone()).stream(prompt),
                None => agent.run(prompt),
            };
            println_json(&serde_json::json!({"type": "session", "id": run.session_id()}));
            let mut outcome = None;
            while let Some(u) = run.next().await {
                match u {
                    Update::Text(t) if pulses => println_json(&serde_json::json!({"type": "text_delta", "text": t})),
                    Update::Thinking(t) if pulses => println_json(&serde_json::json!({"type": "thinking_delta", "text": t})),
                    Update::Reply(m) => println_json(&serde_json::json!({"type": "reply", "message": m})),
                    Update::Tool { call, result } => println_json(&serde_json::json!({"type": "tool", "call": call, "result": result})),
                    Update::Ask(a) => {
                        println_json(&serde_json::json!({"type": "ask", "question": a.question}));
                        // Headless: nobody can answer interactively.
                        a.deny("headless client cannot answer");
                    }
                    Update::Event(e) => println_json(&serde_json::json!({"type": "event", "event": e})),
                    Update::Done(o) => {
                        println_json(&serde_json::json!({"type": "done", "outcome": o}));
                        outcome = Some(o);
                    }
                    _ => {}
                }
            }
            Ok(match outcome {
                Some(TurnOutcome::Done { .. }) => exit_code::OK,
                Some(TurnOutcome::Suspended { .. }) => exit_code::SUSPENDED,
                Some(TurnOutcome::Interrupted) => exit_code::INTERRUPTED,
                _ => exit_code::FAILED,
            })
        }
    }
}
