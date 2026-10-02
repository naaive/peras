//! `peras`: a coding agent in the terminal.
//!
//! ```text
//! peras                         interactive session in the current directory
//! peras "fix the failing test"  interactive, starting with this message
//! peras -p "explain src/lib.rs" print mode: answer and exit (stdin is appended)
//! peras -c                      continue the last conversation
//! peras -r <id>                 resume a conversation
//! ```

use agent::proto::exit_code;
use agent::proto::OnAsk;
use agent_code::mode::PermissionMode;
use agent_code::print::OutputFormat;
use agent_code::repl::Repl;
use agent_code::trust::TrustStore;
use agent_code::{render, Options};
use clap::Parser;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "peras", version, about = "A coding agent in your terminal")]
struct Cli {
    /// First message (interactive) or the prompt (print mode).
    prompt: Option<String>,
    /// Print mode: answer and exit (non-interactive).
    #[arg(short, long)]
    print: bool,
    /// Continue the most recent conversation of this workspace.
    #[arg(short = 'c', long = "continue")]
    continue_: bool,
    /// Resume a conversation by id (or unique prefix); without an id, list them.
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
    /// Model id (e.g. claude-opus-5-5, or the gateway's model name).
    #[arg(long)]
    model: Option<String>,
    /// Model API: anthropic | openai (OpenAI-compatible chat completions).
    /// Default: `$PERAS_PROVIDER`, else anthropic. Keys come from
    /// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`.
    #[arg(long)]
    provider: Option<String>,
    /// API base URL (default `$ANTHROPIC_BASE_URL` / `$OPENAI_BASE_URL`); for
    /// openai a URL without a version gets `/v1`.
    #[arg(long)]
    base_url: Option<String>,
    /// Context window of the model in tokens (openai default 128000).
    #[arg(long)]
    context_window: Option<u32>,
    /// default | acceptEdits | plan | bypassPermissions
    #[arg(long, default_value = "default")]
    permission_mode: String,
    /// Same as `--permission-mode bypassPermissions` (invariant checks still ask).
    #[arg(long)]
    dangerously_skip_permissions: bool,
    /// Tools never asked about: `edit`, `bash(git diff:*)` (comma or space separated).
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    allowed_tools: Vec<String>,
    /// Tools denied by policy: `web_fetch`, `bash(git push:*)`.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    disallowed_tools: Vec<String>,
    /// Text appended to the system prompt.
    #[arg(long)]
    append_system_prompt: Option<String>,
    /// Settings file (TOML) applied as the command-line layer.
    #[arg(long)]
    settings: Option<PathBuf>,
    /// Print mode output: text | json | stream-json
    #[arg(long, default_value = "text")]
    output_format: String,
    /// Print mode: questions nobody can answer are denied (`deny`) or suspend
    /// the session for a later `--resume` (`defer`).
    #[arg(long, default_value = "deny")]
    on_ask: String,
    /// Trust this workspace (and remember it) without asking.
    #[arg(long)]
    trust: bool,
    /// Workspace directory.
    #[arg(long, short = 'C', default_value = ".")]
    dir: PathBuf,
    /// Journal database (default `<dir>/.agent/runs.db`).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Show thinking and longer tool output.
    #[arg(long)]
    verbose: bool,
    /// Disable colors.
    #[arg(long)]
    no_color: bool,
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

async fn real_main(cli: Cli) -> anyhow::Result<i32> {
    let interactive = !cli.print;
    render::set_color(!cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal());

    let mode = if cli.dangerously_skip_permissions {
        PermissionMode::BypassPermissions
    } else {
        PermissionMode::parse(&cli.permission_mode)
            .ok_or_else(|| anyhow::anyhow!("--permission-mode must be default, acceptEdits, plan or bypassPermissions"))?
    };
    let provider = cli.provider.clone().or_else(|| std::env::var("PERAS_PROVIDER").ok()).unwrap_or_else(|| "anthropic".into());
    let endpoint = agent_code::ModelEndpoint {
        provider: agent_code::Provider::parse(&provider).ok_or_else(|| anthropic_or_openai(&provider))?,
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        api_key: None,
        context_window: cli.context_window,
    };
    let mut opts = Options::new(cli.dir.clone());
    opts.port = endpoint.port().map_err(|e| anyhow::anyhow!(e))?;
    opts.db = cli.db.clone();
    opts.model = cli.model.clone();
    opts.mode = mode;
    opts.allowed_tools = split_specs(&cli.allowed_tools);
    opts.disallowed_tools = split_specs(&cli.disallowed_tools);
    opts.append_system_prompt = cli.append_system_prompt.clone();
    opts.settings = cli.settings.clone();
    opts.trusted = workspace_trust(&cli, interactive)?;

    // Which conversation.
    let history = agent_code::history::History::new(&agent_code::data_dir(&cli.dir));
    let session = if cli.continue_ {
        Some(history.last().ok_or_else(|| anyhow::anyhow!("no earlier conversation in this workspace"))?.id)
    } else {
        match cli.resume.as_deref() {
            Some("") => {
                let all = history.list();
                if all.is_empty() {
                    println!("No earlier conversations in this workspace.");
                } else {
                    for e in all.iter().rev().take(30) {
                        println!("{}  {}", e.id, e.title);
                    }
                    println!("Resume one with: peras --resume <id>");
                }
                return Ok(exit_code::OK);
            }
            Some(id) => Some(history.find(id).map(|e| e.id).unwrap_or_else(|| id.to_string())),
            None => None,
        }
    };

    if interactive {
        opts.hot_reload = true;
        let coding = opts.build();
        coding.agent.check().await?;
        let repl = Repl::new(
            coding,
            session,
            agent_code::repl::stdin_lines(),
            agent_code::repl::interrupts(),
            Box::new(std::io::stdout()),
        )
        .verbose(cli.verbose)
        .steer_while_busy(std::io::stdin().is_terminal());
        repl.run(cli.prompt.clone()).await?;
        return Ok(exit_code::OK);
    }

    // Print mode.
    let format = OutputFormat::parse(&cli.output_format).ok_or_else(|| anyhow::anyhow!("--output-format must be text, json or stream-json"))?;
    opts.unattended = Some(match cli.on_ask.as_str() {
        "deny" => OnAsk::Deny,
        "defer" => OnAsk::Defer,
        "allow" => OnAsk::Allow,
        other => anyhow::bail!("--on-ask must be deny, defer or allow, got `{other}`"),
    });
    let mut prompt = cli.prompt.clone().unwrap_or_default();
    if let Some(piped) = piped_stdin(prompt.is_empty())? {
        if !piped.trim().is_empty() {
            prompt = if prompt.is_empty() { piped } else { format!("{prompt}\n\n{piped}") };
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("print mode needs a prompt (argument or stdin)");
    }
    let coding = opts.build();
    let mut out = std::io::stdout();
    let result = agent_code::print::run(&coding, session, prompt, format, &mut out).await;
    out.flush().ok();
    Ok(result.exit_code())
}

/// Piped stdin (`cat log | peras -p "why does it fail?"`). With a prompt
/// argument, stdin only counts when data arrives promptly: an inherited,
/// never-written pipe must not hang the run.
fn piped_stdin(wait: bool) -> anyhow::Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    // First the first chunk (or the end), then the rest: only the wait for
    // the first chunk is bounded.
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut first = [0u8; 8192];
        match stdin.read(&mut first) {
            Ok(0) => {
                let _ = tx.send(Ok(vec![]));
            }
            Ok(n) => {
                let _ = tx.send(Ok(first[..n].to_vec()));
                let mut rest = vec![];
                let r = stdin.read_to_end(&mut rest).map(|_| rest);
                let _ = tx.send(r);
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        }
    });
    let first = if wait { rx.recv().ok() } else { rx.recv_timeout(std::time::Duration::from_millis(300)).ok() };
    let mut bytes = match first {
        None => return Ok(None),
        Some(r) => r?,
    };
    if !bytes.is_empty() {
        bytes.extend(rx.recv().unwrap_or(Ok(vec![]))?);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn anthropic_or_openai(got: &str) -> anyhow::Error {
    anyhow::anyhow!("--provider must be anthropic or openai, got `{got}`")
}

/// `--allowed-tools "edit bash(git diff:*)"`: split on spaces outside parentheses.
fn split_specs(raw: &[String]) -> Vec<String> {
    let mut out = vec![];
    for r in raw {
        let mut depth = 0;
        let mut cur = String::new();
        for ch in r.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    cur.push(ch);
                }
                ')' => {
                    depth -= 1;
                    cur.push(ch);
                }
                c if c.is_whitespace() && depth == 0 => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// Trusted if remembered or `--trust`; interactively, ask once.
fn workspace_trust(cli: &Cli, interactive: bool) -> anyhow::Result<bool> {
    let store = TrustStore::from_env();
    if store.as_ref().is_some_and(|s| s.is_trusted(&cli.dir)) {
        return Ok(true);
    }
    let remember = |s: &Option<TrustStore>| {
        if let Some(s) = s {
            if let Err(e) = s.trust(&cli.dir) {
                eprintln!("warning: could not remember the trust decision: {e}");
            }
        }
    };
    if cli.trust {
        remember(&store);
        return Ok(true);
    }
    if !interactive || !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let dir = std::fs::canonicalize(&cli.dir).unwrap_or_else(|_| cli.dir.clone());
    println!("Do you trust the files in {}?", dir.display());
    println!("A trusted workspace's instruction files, commands, hooks and MCP servers are active, and its files are");
    println!("not treated as untrusted content. Only trust code you know; you can still work in an untrusted folder.");
    print!("Trust this folder? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if yes {
        remember(&store);
    }
    println!();
    Ok(yes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_split_outside_parentheses() {
        let v = split_specs(&["edit bash(git diff:*)".into(), "web_fetch".into()]);
        assert_eq!(v, vec!["edit", "bash(git diff:*)", "web_fetch"]);
    }

    #[test]
    fn cli_parses() {
        let c = Cli::try_parse_from(["peras", "-p", "hi", "--allowed-tools", "edit,bash(git status)", "--output-format", "json"]).unwrap();
        assert!(c.print);
        assert_eq!(c.allowed_tools, vec!["edit", "bash(git status)"]);
        let c = Cli::try_parse_from(["peras", "-r"]).unwrap();
        assert_eq!(c.resume.as_deref(), Some(""));
        let c = Cli::try_parse_from(["peras", "-c", "--permission-mode", "plan"]).unwrap();
        assert!(c.continue_);
    }
}
