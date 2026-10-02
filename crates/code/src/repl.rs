//! The interactive terminal REPL.
//!
//! - Text streams as the model writes it; each tool call is shown as a title
//!   with a short preview of its result (diffs for edits, the checklist for
//!   `todo_write`).
//! - Approvals: `1` yes, `2` yes and don't ask again this session (all edits
//!   for edit tools: accept-edits mode; the command prefix for `bash`), `3`
//!   no, with what to do instead. Leaving plan mode is approved the same way.
//! - Typing while the agent works steers it (delivered at its next step);
//!   Ctrl-C interrupts the turn, twice at the prompt exits.
//! - `!cmd` runs a shell command and hands its output to the next message;
//!   `#note` appends a note to the project's `AGENTS.md`.
//! - Built-in commands: see [`HELP`]; other `/name args` are the prompt
//!   commands of the configuration (`/init`, `/review`, ...).

use crate::history::Entry;
use crate::mode::{Grant, PermissionMode};
use crate::render::{self, Totals};
use crate::Coding;
use agent::prelude::*;
use agent::proto::{ApprovalLevel, CallId, Control, Event, GateRef, ModelId, ToolCall};
use std::collections::HashMap;
use std::io::Write;
use tokio::sync::mpsc;

pub const HELP: &str = "\
Commands:
  /help                 this help
  /clear                start a new conversation
  /compact [focus]      summarize the conversation and continue from the summary
  /resume [id]          list conversations of this workspace, or switch to one
  /rewind               go back to an earlier message (restores the agent's file changes)
  /mode [name]          show or set the permission mode (default, acceptEdits, plan, bypassPermissions)
  /plan                 enter plan mode
  /model [id]           show or switch the model
  /permissions          the session's \"don't ask again\" grants
  /todos                the current task list
  /cost                 token usage and cost of this conversation
  /status               session, model, mode, workspace
  /commands             prompt commands of the configuration (/init, /review, ...)
  /exit                 quit
Input:
  !<command>            run a shell command; its output goes with your next message
  #<note>               add a note to AGENTS.md (project memory)
  a line ending in \\    continues on the next line
  typing while the agent works steers it; Ctrl-C interrupts";

/// One line of user input (or the end of input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Line(String),
    Eof,
}

/// Read stdin lines on a thread (lines ending in `\` are joined).
pub fn stdin_lines() -> mpsc::UnboundedReceiver<Input> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        let mut acc = String::new();
        loop {
            buf.clear();
            match stdin.read_line(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Input::Eof);
                    return;
                }
                Ok(_) => {
                    let line = buf.trim_end_matches(['\n', '\r']);
                    if let Some(part) = line.strip_suffix('\\') {
                        acc.push_str(part);
                        acc.push('\n');
                        continue;
                    }
                    acc.push_str(line);
                    if tx.send(Input::Line(std::mem::take(&mut acc))).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// Ctrl-C presses.
pub fn interrupts() -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if tx.send(()).is_err() {
                return;
            }
        }
    });
    rx
}

pub struct Repl {
    coding: Coding,
    session: String,
    /// The conversation is recorded in the index (it has a first message).
    recorded: bool,
    totals: Totals,
    input: mpsc::UnboundedReceiver<Input>,
    interrupts: mpsc::UnboundedReceiver<()>,
    out: Box<dyn Write + Send>,
    verbose: bool,
    /// Context handed to the next message (`!` output, a compacted summary).
    pending: Vec<String>,
    /// The model the session was switched to (`/model`).
    model: Option<String>,
    /// Told whenever the REPL waits for the user (prompt, approval).
    ready: Option<mpsc::UnboundedSender<()>>,
}

enum Flow {
    Continue,
    Exit,
}

impl Repl {
    pub fn new(
        coding: Coding,
        session: Option<String>,
        input: mpsc::UnboundedReceiver<Input>,
        interrupts: mpsc::UnboundedReceiver<()>,
        out: Box<dyn Write + Send>,
    ) -> Repl {
        let recorded = session.is_some();
        Repl {
            coding,
            session: session.unwrap_or_else(new_id),
            recorded,
            totals: Totals::default(),
            input,
            interrupts,
            out,
            verbose: false,
            pending: vec![],
            model: None,
            ready: None,
        }
    }

    /// Be told whenever the REPL waits for the user (scripted frontends and
    /// tests feed input on it).
    pub fn on_ready(mut self, tx: mpsc::UnboundedSender<()>) -> Repl {
        self.ready = Some(tx);
        self
    }

    fn waiting(&self) {
        if let Some(r) = &self.ready {
            let _ = r.send(());
        }
    }

    pub fn verbose(mut self, yes: bool) -> Repl {
        self.verbose = yes;
        self
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    fn say(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn write(&mut self, text: &str) {
        let _ = write!(self.out, "{text}");
        let _ = self.out.flush();
    }

    async fn read_line(&mut self) -> Option<String> {
        self.waiting();
        match self.input.recv().await {
            Some(Input::Line(l)) => Some(l),
            Some(Input::Eof) | None => None,
        }
    }

    /// The REPL loop; `first` is sent as the first message.
    pub async fn run(mut self, first: Option<String>) -> anyhow::Result<()> {
        self.banner().await;
        if self.recorded {
            self.replay_tail().await;
        }
        if let Some(p) = first {
            self.say(&format!("{} {p}", render::bold(">")));
            self.turn(p).await;
        }
        let mut armed_exit = false;
        loop {
            let mode = self.coding.permissions.mode();
            if mode != PermissionMode::Default {
                self.say(&render::dim(&mode_line(mode)));
            }
            self.write(&format!("{} ", render::bold(">")));
            self.waiting();
            let line = tokio::select! {
                l = self.input.recv() => l,
                _ = self.interrupts.recv() => {
                    if armed_exit {
                        self.say("");
                        return Ok(());
                    }
                    armed_exit = true;
                    self.say(&render::dim("\n(press Ctrl-C again to exit)"));
                    continue;
                }
            };
            armed_exit = false;
            let line = match line {
                Some(Input::Line(l)) => l,
                Some(Input::Eof) | None => {
                    self.say("");
                    return Ok(());
                }
            };
            if let Flow::Exit = self.handle(line).await {
                self.say(&render::dim(&format!("Resume this conversation with: peras --resume {}", self.session)));
                return Ok(());
            }
        }
    }

    async fn banner(&mut self) {
        let dir = std::fs::canonicalize(&self.coding.dir).unwrap_or_else(|_| self.coding.dir.clone());
        let model = match self.coding.model_name().await {
            Ok(m) => m,
            Err(e) => {
                self.say(&render::red(&format!("configuration error: {e}")));
                String::from("?")
            }
        };
        self.say(&format!("{} {}", render::bold("✻ Peras"), render::dim("— a coding agent in your terminal")));
        self.say(&render::dim(&format!("  model: {model} · cwd: {} · /help for commands", dir.display())));
        self.say("");
    }

    /// Show the end of a resumed conversation.
    async fn replay_tail(&mut self) {
        let Ok(events) = self.coding.agent.session(self.session.clone()).events().await else { return };
        let mut lines = vec![];
        for e in &events {
            match &e.body {
                Event::UserMessage { text, .. } => lines.push(format!("{} {}", render::bold(">"), first_line(text))),
                Event::AssistantReplied { message, .. } if !message.text().trim().is_empty() => {
                    lines.push(first_line(&message.text()));
                }
                Event::ToolResulted { call, .. } if call.name == "todo_write" => {
                    if let Some(t) = crate::tools::TodoStore::parse(&call.input) {
                        self.coding.todos.set(&agent::proto::SessionId::new(self.session.clone()), t);
                    }
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            return;
        }
        self.say(&render::dim(&format!("Resumed conversation {} ({} messages):", self.session, lines.len())));
        let start = lines.len().saturating_sub(6);
        for l in &lines[start..] {
            self.say(&render::dim(&format!("  {l}")));
        }
        self.say("");
    }

    async fn handle(&mut self, line: String) -> Flow {
        let text = line.trim();
        if text.is_empty() {
            return Flow::Continue;
        }
        if let Some(cmd) = text.strip_prefix('!') {
            self.shell(cmd.trim()).await;
            return Flow::Continue;
        }
        if let Some(note) = text.strip_prefix('#') {
            self.remember(note.trim());
            return Flow::Continue;
        }
        if let Some((name, args)) = agent::proto::parse_slash(text) {
            let (name, args) = (name.to_string(), args.to_string());
            return self.slash(&name, &args, text).await;
        }
        self.turn(line).await;
        Flow::Continue
    }

    async fn slash(&mut self, name: &str, args: &str, raw: &str) -> Flow {
        match name {
            "exit" | "quit" => return Flow::Exit,
            "help" => self.say(HELP),
            "clear" | "new" => {
                self.session = new_id();
                self.recorded = false;
                self.totals = Totals::default();
                self.pending.clear();
                self.say(&render::dim("Started a new conversation."));
            }
            "compact" => self.compact(args).await,
            "cost" => {
                let d = self.totals.describe();
                self.say(&d);
            }
            "status" => self.status().await,
            "mode" => {
                let m = if args.is_empty() {
                    self.coding.permissions.mode().cycle()
                } else {
                    match PermissionMode::parse(args) {
                        Some(m) => m,
                        None => {
                            self.say(&render::red(&format!("unknown mode `{args}` (default, acceptEdits, plan, bypassPermissions)")));
                            return Flow::Continue;
                        }
                    }
                };
                self.coding.permissions.set_mode(m);
                self.say(&format!("Permission mode: {}", render::bold(m.name())));
            }
            "plan" => {
                self.coding.permissions.set_mode(PermissionMode::Plan);
                self.say(&format!("Permission mode: {}", render::bold("plan")));
                if !args.is_empty() {
                    self.turn(args.to_string()).await;
                }
            }
            "model" => self.model(args).await,
            "permissions" => {
                let grants = self.coding.permissions.grants();
                if grants.is_empty() {
                    self.say("No session grants. Approve a call with `2` to stop being asked for it.");
                }
                for g in grants {
                    self.say(&format!("  allow {}", g.describe()));
                }
            }
            "todos" => {
                let t = self.coding.todos.get(&agent::proto::SessionId::new(self.session.clone()));
                if t.is_empty() {
                    self.say("No tasks.");
                }
                for l in render::todos(&t) {
                    self.say(&l);
                }
            }
            "resume" => self.resume(args).await,
            "rewind" => self.rewind().await,
            "commands" => match self.coding.agent.commands().await {
                Ok(cs) => {
                    for c in cs {
                        self.say(&format!("  /{:<18} {}", c.name, render::dim(&c.description)));
                    }
                }
                Err(e) => self.say(&render::red(&e.to_string())),
            },
            _ => match self.coding.agent.expand(raw).await {
                Ok(Some(_)) => self.turn(raw.to_string()).await,
                Ok(None) => self.say(&render::red(&format!("Unknown command /{name} (/help lists the commands)"))),
                Err(e) => self.say(&render::red(&e.to_string())),
            },
        }
        Flow::Continue
    }

    /// One turn of the conversation, rendered as it happens.
    pub async fn turn(&mut self, text: String) {
        let text = if self.pending.is_empty() {
            text
        } else {
            let ctx = std::mem::take(&mut self.pending).join("\n\n");
            format!("{ctx}\n\n{text}")
        };
        if !self.recorded {
            let _ = self.coding.history().record(&self.session, &text);
            self.recorded = true;
        }
        let mut run = self.coding.agent.session(self.session.clone()).stream(text);
        let ctl = run.control();
        let mut calls: HashMap<CallId, ToolCall> = HashMap::new();
        let mut streamed = false;
        let mut at_line_start = true;
        loop {
            tokio::select! {
                u = run.next() => {
                    let Some(u) = u else { break };
                    match u {
                        Update::Text(t) => {
                            streamed = true;
                            at_line_start = t.ends_with('\n');
                            self.write(&t);
                        }
                        Update::Thinking(t) if self.verbose => self.write(&render::dim(&t)),
                        Update::Reply(m) => {
                            self.totals.add(&m.usage);
                            let text = m.text();
                            if !streamed && !text.trim().is_empty() {
                                self.say(text.trim_end());
                            } else if streamed && !at_line_start {
                                self.say("");
                            }
                            streamed = false;
                            at_line_start = true;
                            for c in m.tool_calls() {
                                calls.insert(c.id.clone(), c.clone());
                                if c.name == "exit_plan_mode" {
                                    continue; // shown with its approval
                                }
                                let title = render::tool_title(c);
                                self.say(&title);
                            }
                        }
                        Update::Tool { call, result } => {
                            let lines = if call.name == "todo_write" && !result.is_error {
                                render::todo_call(&call)
                            } else {
                                render::result_preview(&call, &result, if self.verbose { 40 } else { 4 })
                            };
                            for l in lines {
                                self.say(&l);
                            }
                        }
                        Update::Ask(a) => {
                            let call = match &a.subject {
                                Some(GateRef::Call(id)) => calls.get(id).cloned(),
                                _ => None,
                            };
                            self.ask(a, call, &ctl).await;
                        }
                        Update::Event(e) => self.event(&e.body),
                        Update::Done(o) => self.outcome(&o),
                        _ => {}
                    }
                }
                l = self.input.recv() => match l {
                    Some(Input::Line(l)) if !l.trim().is_empty() => {
                        ctl.steer(l.trim().to_string());
                        self.say(&render::dim("  (sent: the agent sees it at its next step)"));
                    }
                    Some(Input::Line(_)) => {}
                    Some(Input::Eof) | None => ctl.interrupt(),
                },
                _ = self.interrupts.recv() => {
                    ctl.interrupt();
                    self.say(&render::yellow("\n  Interrupted · what should the agent do instead?"));
                }
            }
        }
    }

    fn event(&mut self, e: &Event) {
        match e {
            Event::SubagentStarted { child, .. } => {
                let l = render::dim(&format!("  ⎿  sub-agent session {child} started"));
                self.say(&l);
            }
            Event::Replaced(_) => {
                let l = render::dim("  (context compacted)");
                self.say(&l);
            }
            Event::ModelSwitched { .. } => {
                let l = render::dim("  (model switched)");
                self.say(&l);
            }
            _ => {}
        }
    }

    fn outcome(&mut self, o: &TurnOutcome) {
        let l = match o {
            TurnOutcome::Done { .. } => return,
            TurnOutcome::Interrupted => render::yellow("  Interrupted."),
            TurnOutcome::Failed { error } => render::red(&format!("  Error: {error}")),
            TurnOutcome::BudgetExhausted { what } => render::red(&format!("  Budget exhausted: {what}")),
            TurnOutcome::Suspended { .. } => render::yellow("  Suspended: waiting for an approval."),
        };
        self.say(&l);
    }

    /// Ask the user to approve a call.
    async fn ask(&mut self, a: Ask, call: Option<ToolCall>, ctl: &agent::RunControl) {
        let plan = call.as_ref().is_some_and(|c| c.name == "exit_plan_mode");
        let grant = call.as_ref().filter(|_| !plan && a.question.level == ApprovalLevel::Policy).map(grant_offer);
        if plan {
            let text = call.as_ref().and_then(|c| c.input.get("plan")).and_then(|p| p.as_str()).unwrap_or("").to_string();
            self.say(&render::cyan("╭─ Plan ─────────────────────────────────────────"));
            for l in text.lines() {
                self.say(&format!("{} {l}", render::cyan("│")));
            }
            self.say(&render::cyan("╰────────────────────────────────────────────────"));
            self.say("Would you like to proceed?");
            self.say("  1. Yes, and auto-accept edits");
            self.say("  2. Yes, and manually approve edits");
            self.say("  3. No, keep planning");
        } else {
            self.say(&render::yellow(&format!("╭─ Permission required: {}", a.question.prompt.lines().next().unwrap_or(""))));
            for l in a.question.prompt.lines().skip(1).take(8) {
                self.say(&format!("{} {}", render::yellow("│"), render::dim(l)));
            }
            if let Some(c) = &call {
                let title = render::tool_title(c);
                self.say(&format!("{} {title}", render::yellow("│")));
                for l in render::diff(c).into_iter().take(30) {
                    self.say(&format!("{}   {l}", render::yellow("│")));
                }
            }
            self.say(&render::yellow("╰─"));
            self.say("  1. Yes");
            if let Some((_, label)) = &grant {
                self.say(&format!("  2. Yes, and don't ask again for {label} this session"));
            }
            self.say("  3. No, and tell the agent what to do differently");
        }
        loop {
            self.write(&render::bold("  choice [1]: "));
            self.waiting();
            let answer = tokio::select! {
                l = self.input.recv() => match l {
                    Some(Input::Line(l)) => Some(l),
                    Some(Input::Eof) | None => None,
                },
                _ = self.interrupts.recv() => None,
            };
            let Some(answer) = answer else {
                a.deny("The user interrupted");
                ctl.interrupt();
                return;
            };
            match answer.trim().to_ascii_lowercase().as_str() {
                "" | "1" | "y" | "yes" => {
                    if plan {
                        self.coding.permissions.set_mode(PermissionMode::AcceptEdits);
                    }
                    a.allow();
                    return;
                }
                "2" if plan => {
                    self.coding.permissions.set_mode(PermissionMode::Default);
                    a.allow();
                    return;
                }
                "2" | "a" | "always" if grant.is_some() => {
                    if let Some((g, _)) = &grant {
                        match g {
                            Offer::AcceptEdits => self.coding.permissions.set_mode(PermissionMode::AcceptEdits),
                            Offer::Grant(g) => self.coding.permissions.grant(g.clone()),
                        }
                    }
                    a.allow();
                    return;
                }
                "3" | "n" | "no" => {
                    self.write(&render::bold("  what should the agent do instead? "));
                    let why = self.read_line().await.unwrap_or_default();
                    let why = why.trim();
                    let reason = if why.is_empty() {
                        "The user declined. Ask them how to proceed.".to_string()
                    } else {
                        format!("The user declined and said: {why}")
                    };
                    a.deny(reason);
                    return;
                }
                other => self.say(&render::red(&format!("  `{other}`: answer 1, 2 or 3"))),
            }
        }
    }

    /// `!cmd`: run it in the workspace and keep the output for the next message.
    async fn shell(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        let dir = self.coding.dir.clone();
        let c = cmd.to_string();
        let out = tokio::task::spawn_blocking(move || std::process::Command::new("sh").arg("-c").arg(&c).current_dir(dir).output()).await;
        let (stdout, stderr, code) = match out {
            Ok(Ok(o)) => (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned(), o.status.code()),
            Ok(Err(e)) => (String::new(), e.to_string(), None),
            Err(e) => (String::new(), e.to_string(), None),
        };
        let shown = format!("{stdout}{stderr}");
        for l in shown.lines().take(40) {
            self.say(&render::dim(&format!("  {l}")));
        }
        let mut ctx = format!("The user ran a shell command:\n<bash-input>{cmd}</bash-input>\n");
        ctx.push_str(&format!("<bash-stdout>{}</bash-stdout>\n", truncate(&stdout, 8000)));
        if !stderr.is_empty() {
            ctx.push_str(&format!("<bash-stderr>{}</bash-stderr>\n", truncate(&stderr, 4000)));
        }
        ctx.push_str(&format!("<exit-code>{}</exit-code>", code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())));
        self.pending.push(ctx);
        self.say(&render::dim("  (output will be sent with your next message)"));
    }

    /// `#note`: append to the project's `AGENTS.md` (read at session start).
    fn remember(&mut self, note: &str) {
        if note.is_empty() {
            return;
        }
        let path = self.coding.dir.join("AGENTS.md");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut text = existing.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !existing.contains("\n## Notes\n") && !existing.starts_with("## Notes\n") {
            text.push_str(if existing.is_empty() { "## Notes\n\n" } else { "\n## Notes\n\n" });
        }
        text.push_str(&format!("- {note}\n"));
        match std::fs::write(&path, text) {
            Ok(()) => self.say(&render::dim(&format!("  Noted in {} (applies from the next conversation).", path.display()))),
            Err(e) => self.say(&render::red(&format!("  {}: {e}", path.display()))),
        }
    }

    async fn status(&mut self) {
        let model = match &self.model {
            Some(m) => m.clone(),
            None => self.coding.model_name().await.unwrap_or_else(|e| format!("error: {e}")),
        };
        let trusted = self.coding.agent.profile().await.map(|p| p.kernel.security.workspace_trusted).unwrap_or(false);
        let lines = [
            format!("Session:    {}", self.session),
            format!("Model:      {model}"),
            format!("Mode:       {}", self.coding.permissions.mode().name()),
            format!(
                "Workspace:  {} ({})",
                std::fs::canonicalize(&self.coding.dir).unwrap_or_else(|_| self.coding.dir.clone()).display(),
                if trusted { "trusted" } else { "untrusted" }
            ),
            format!("Journal:    {}", self.coding.db.display()),
            format!("Tokens:     {} in / {} out", self.totals.input + self.totals.cache_read + self.totals.cache_write, self.totals.output),
        ];
        for l in lines {
            self.say(&l);
        }
    }

    async fn model(&mut self, args: &str) {
        if args.is_empty() {
            let current = match &self.model {
                Some(m) => m.clone(),
                None => self.coding.model_name().await.unwrap_or_default(),
            };
            self.say(&format!("Model: {current} (switch with /model <id>)"));
            return;
        }
        let chat = self.coding.agent.session(self.session.clone());
        match chat.control(Control::SwitchModel { model: ModelId::new(args.to_string()) }).await {
            Ok(()) => {
                self.model = Some(args.to_string());
                self.say(&format!("Model: {}", render::bold(args)));
            }
            Err(e) => self.say(&render::red(&format!("could not switch: {e}"))),
        }
    }

    /// Summarize the conversation and start a new one from the summary.
    async fn compact(&mut self, focus: &str) {
        self.say(&render::dim("Compacting the conversation..."));
        let mut instruction = format!(
            "{}\n\nReply with the summary only, as plain text. Do not call any tools.",
            agent::proto::config::DEFAULT_SUMMARY_INSTRUCTION
        );
        if !focus.is_empty() {
            instruction.push_str(&format!("\nFocus on: {focus}"));
        }
        let mut run = self.coding.agent.session(self.session.clone()).stream(instruction);
        let mut summary = None;
        while let Some(u) = run.next().await {
            match u {
                Update::Reply(m) => self.totals.add(&m.usage),
                Update::Ask(a) => a.deny("Compacting: no tools."),
                Update::Done(TurnOutcome::Done { text }) => summary = Some(text),
                Update::Done(o) => self.outcome(&o),
                _ => {}
            }
        }
        let Some(summary) = summary.filter(|s| !s.trim().is_empty()) else {
            self.say(&render::red("Compaction failed; the conversation is unchanged."));
            return;
        };
        let previous = std::mem::replace(&mut self.session, new_id());
        self.recorded = false;
        self.pending.push(format!(
            "This conversation continues an earlier one ({previous}) that was compacted. Summary of the earlier conversation:\n<summary>\n{}\n</summary>",
            summary.trim()
        ));
        self.say(&render::dim(&format!("Compacted ({} chars). The summary goes with your next message.", summary.len())));
    }

    async fn resume(&mut self, args: &str) {
        let history = self.coding.history();
        if args.is_empty() {
            let all = history.list();
            if all.is_empty() {
                self.say("No earlier conversations in this workspace.");
                return;
            }
            for e in all.iter().rev().take(20) {
                self.say(&format_entry(e, &self.session));
            }
            self.say(&render::dim("Switch with /resume <id> (a unique prefix is enough)."));
            return;
        }
        match history.find(args) {
            Some(e) => {
                self.session = e.id.clone();
                self.recorded = true;
                self.totals = Totals::default();
                self.pending.clear();
                self.replay_tail().await;
            }
            None => self.say(&render::red(&format!("No single conversation matches `{args}`."))),
        }
    }

    /// Go back to an earlier user message: the conversation and the agent's
    /// file changes since then are undone.
    async fn rewind(&mut self) {
        let chat = self.coding.agent.session(self.session.clone());
        let events = match chat.events().await {
            Ok(e) => e,
            Err(e) => {
                self.say(&render::red(&e.to_string()));
                return;
            }
        };
        let points: Vec<(u64, String)> = events
            .iter()
            .filter_map(|e| match &e.body {
                Event::UserMessage { text, .. } => Some((e.seq, first_line(text))),
                _ => None,
            })
            .collect();
        if points.is_empty() {
            self.say("Nothing to rewind.");
            return;
        }
        for (i, (_, text)) in points.iter().enumerate() {
            self.say(&format!("  {}. {text}", i + 1));
        }
        self.write(&render::bold("  rewind to before message #: "));
        let Some(choice) = self.read_line().await else { return };
        let Some((seq, _)) = choice.trim().parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| points.get(i)) else {
            self.say("Cancelled.");
            return;
        };
        let seq = *seq;
        match chat.rewind(seq).await {
            Ok(r) => {
                self.say(&format!("Rewound. Restored {} file(s).", r.restored.len()));
                for c in &r.conflicts {
                    self.say(&render::yellow(&format!("  conflict (left as is): {c}")));
                }
                for i in &r.irreversible {
                    self.say(&render::yellow(&format!("  not undone: {i}")));
                }
            }
            Err(e) => self.say(&render::red(&format!("rewind failed: {e}"))),
        }
    }
}

/// What "don't ask again" means for a call.
#[derive(Debug, Clone)]
enum Offer {
    AcceptEdits,
    Grant(Grant),
}

fn grant_offer(call: &ToolCall) -> (Offer, String) {
    match call.name.as_str() {
        "edit" | "multi_edit" | "write" => (Offer::AcceptEdits, "file edits".to_string()),
        _ => {
            let g = Grant::for_call(call);
            let label = g.describe();
            (Offer::Grant(g), label)
        }
    }
}

fn mode_line(m: PermissionMode) -> String {
    match m {
        PermissionMode::Default => String::new(),
        PermissionMode::AcceptEdits => "⏵⏵ accept edits on (/mode to change)".into(),
        PermissionMode::Plan => "⏸ plan mode on (/mode to change)".into(),
        PermissionMode::BypassPermissions => "⏵⏵ bypass permissions on (/mode to change)".into(),
    }
}

fn format_entry(e: &Entry, current: &str) -> String {
    let marker = if e.id == current { "*" } else { " " };
    let age = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(e.started))
        .unwrap_or(0);
    let age = match age {
        a if a < 3600 => format!("{}m ago", a / 60),
        a if a < 86_400 => format!("{}h ago", a / 3600),
        a => format!("{}d ago", a / 86_400),
    };
    format!("{marker} {}  {:>8}  {}", e.id, render::dim(&age), e.title)
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if l.chars().count() > 100 { format!("{}…", l.chars().take(99).collect::<String>()) } else { l.to_string() }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &s[..end])
}

fn new_id() -> String {
    ulid::Ulid::new().to_string()
}
