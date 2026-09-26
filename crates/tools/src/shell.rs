//! Shell command analysis: a conservative tokenizer/parser that splits a command
//! line into simple commands, and a [`SemanticTable`] mapping known commands to
//! accesses and side-effect classes.
//!
//! The analysis only reduces approvals; the security boundary is the sandbox
//! compiled from the declaration. Anything the parser does not fully understand
//! (variable expansion, command substitution, heredocs, background jobs, control
//! flow, assignments, redirections out of the workspace, unknown commands) is
//! `Opaque`: it declares a write of the whole workspace.

use crate::caps::sha256_hex;
use crate::fsafe;
use agent_proto::{Access, AccessMode, EffectClass, ResourceUri};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ tokens

/// A shell word.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    /// Value after quote removal.
    pub text: String,
    /// As written.
    pub raw: String,
    /// Contains `$...` or backticks (outside single quotes).
    pub expansion: bool,
    /// Contains unquoted glob / brace characters.
    pub glob: bool,
    /// Starts with an unquoted `~`.
    pub tilde: bool,
    /// Some part was quoted.
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(Word),
    Op(&'static str),
    IoNum(u32),
}

const OPS: &[&str] = &[
    "&>>", "<<<", "<<-", "&&", "||", "|&", ";;", "<<", ">>", ">|", ">&", "<&", "<>", "&>", "|",
    "&", ";", "(", ")", "<", ">",
];

fn is_op_start(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>')
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' || c == '\t' || c == '\r' {
            i += 1;
            continue;
        }
        if c == '\\' && chars.get(i + 1) == Some(&'\n') {
            i += 2;
            continue;
        }
        if c == '\n' {
            out.push(Tok::Op(";"));
            i += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if is_op_start(c) {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            let op = OPS
                .iter()
                .find(|op| rest.starts_with(**op))
                .expect("op start");
            out.push(Tok::Op(op));
            i += op.chars().count();
            continue;
        }
        // A word.
        let mut w = Word::default();
        let start = i;
        while i < chars.len() {
            let c = chars[i];
            if c == ' ' || c == '\t' || c == '\n' || c == '\r' || is_op_start(c) {
                break;
            }
            match c {
                '\'' => {
                    w.quoted = true;
                    i += 1;
                    let mut closed = false;
                    while i < chars.len() {
                        if chars[i] == '\'' {
                            closed = true;
                            break;
                        }
                        w.text.push(chars[i]);
                        i += 1;
                    }
                    if !closed {
                        return Err("unterminated single quote".into());
                    }
                    i += 1;
                }
                '"' => {
                    w.quoted = true;
                    i += 1;
                    let mut closed = false;
                    while i < chars.len() {
                        match chars[i] {
                            '"' => {
                                closed = true;
                                break;
                            }
                            '\\' if i + 1 < chars.len()
                                && matches!(chars[i + 1], '$' | '`' | '"' | '\\' | '\n') =>
                            {
                                if chars[i + 1] != '\n' {
                                    w.text.push(chars[i + 1]);
                                }
                                i += 2;
                                continue;
                            }
                            '$' | '`' => {
                                w.expansion = true;
                                w.text.push(chars[i]);
                            }
                            ch => w.text.push(ch),
                        }
                        i += 1;
                    }
                    if !closed {
                        return Err("unterminated double quote".into());
                    }
                    i += 1;
                }
                '\\' => {
                    if i + 1 < chars.len() {
                        if chars[i + 1] != '\n' {
                            w.text.push(chars[i + 1]);
                        }
                        w.quoted = true;
                        i += 2;
                    } else {
                        return Err("trailing backslash".into());
                    }
                }
                '$' | '`' => {
                    w.expansion = true;
                    w.text.push(c);
                    i += 1;
                }
                '*' | '?' | '[' | '{' => {
                    w.glob = true;
                    w.text.push(c);
                    i += 1;
                }
                '~' if i == start => {
                    w.tilde = true;
                    w.text.push(c);
                    i += 1;
                }
                _ => {
                    w.text.push(c);
                    i += 1;
                }
            }
        }
        w.raw = chars[start..i].iter().collect();
        let io_number = !w.quoted
            && !w.text.is_empty()
            && w.text.chars().all(|c| c.is_ascii_digit())
            && matches!(chars.get(i), Some('<') | Some('>'));
        if io_number {
            out.push(Tok::IoNum(w.text.parse().unwrap_or(0)));
        } else {
            out.push(Tok::Word(w));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ parse

/// A redirection attached to a simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub fd: Option<u32>,
    pub op: &'static str,
    pub target: Word,
}

/// A simple command: words and redirections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimpleCommand {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

impl SimpleCommand {
    pub fn argv(&self) -> Vec<String> {
        self.words.iter().map(|w| w.text.clone()).collect()
    }
    /// The command line (words as written, redirections left out); this is the
    /// `cmd:` resource.
    pub fn line(&self) -> String {
        self.words
            .iter()
            .map(|w| w.raw.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Result of parsing a command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub commands: Vec<SimpleCommand>,
    /// Why the line cannot be analysed (if so).
    pub opaque: Option<String>,
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "{", "}", "!", "[[", "]]", "time", "coproc",
];

fn is_assignment(w: &Word) -> bool {
    match w.raw.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '[' || c == ']' || c == '+'
                })
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Split a command line into simple commands (pipelines, `&&`, `||`, `;`,
/// newlines and subshells are all separators).
pub fn parse(s: &str) -> Parsed {
    let toks = match tokenize(s) {
        Ok(t) => t,
        Err(e) => {
            return Parsed {
                commands: vec![],
                opaque: Some(format!("syntax: {e}")),
            }
        }
    };
    let mut p = Parsed::default();
    let mut cur = SimpleCommand::default();
    let mut depth: i32 = 0;
    let mut pending_fd: Option<u32> = None;
    let mut need_command = false; // after a binary operator
    let mut closed_group = false; // a `)` just closed a non-empty group
    let mut it = toks.into_iter().peekable();
    let fail = |p: &mut Parsed, why: &str| {
        if p.opaque.is_none() {
            p.opaque = Some(why.to_string());
        }
    };
    fn finish(p: &mut Parsed, cur: &mut SimpleCommand) -> Result<bool, &'static str> {
        if cur.words.is_empty() {
            if !cur.redirects.is_empty() {
                return Err("redirection without a command");
            }
            return Ok(false);
        }
        p.commands.push(std::mem::take(cur));
        Ok(true)
    }
    while let Some(t) = it.next() {
        match t {
            Tok::Word(w) => {
                if closed_group {
                    fail(&mut p, "syntax: word after `)`");
                }
                cur.words.push(w);
                need_command = false;
            }
            Tok::IoNum(n) => {
                pending_fd = Some(n);
                if !matches!(it.peek(), Some(Tok::Op(_))) {
                    fail(&mut p, "syntax: dangling fd number");
                }
            }
            Tok::Op(op) => match op {
                "<" | ">" | ">>" | ">|" | "&>" | "&>>" | ">&" | "<&" | "<>" => {
                    let fd = pending_fd.take();
                    match it.next() {
                        Some(Tok::Word(target)) => cur.redirects.push(Redirect { fd, op, target }),
                        _ => fail(&mut p, "syntax: redirection without a target"),
                    }
                }
                "<<" | "<<-" | "<<<" => fail(&mut p, "heredoc / herestring"),
                "&" => fail(&mut p, "background job"),
                ";;" => fail(&mut p, "case syntax"),
                "(" => {
                    if !cur.words.is_empty() {
                        fail(&mut p, "function definition");
                    }
                    depth += 1;
                }
                ")" => {
                    match finish(&mut p, &mut cur) {
                        Ok(pushed) => closed_group = pushed || closed_group,
                        Err(e) => fail(&mut p, e),
                    }
                    depth -= 1;
                    if depth < 0 {
                        fail(&mut p, "syntax: unbalanced `)`");
                    }
                }
                "|" | "|&" | "&&" | "||" | ";" => {
                    match finish(&mut p, &mut cur) {
                        Ok(true) => {}
                        Ok(false) => {
                            if !closed_group && (op != ";" || need_command) {
                                fail(&mut p, "syntax: missing command");
                            }
                        }
                        Err(e) => fail(&mut p, e),
                    }
                    closed_group = false;
                    need_command = op != ";";
                }
                _ => fail(&mut p, "unsupported operator"),
            },
        }
    }
    if let Err(e) = finish(&mut p, &mut cur) {
        fail(&mut p, e);
    }
    if depth != 0 {
        fail(&mut p, "syntax: unbalanced `(`");
    }
    if need_command {
        fail(&mut p, "syntax: missing command");
    }
    if p.commands.is_empty() {
        fail(&mut p, "empty command");
    }
    let mut why: Option<&str> = None;
    for c in &p.commands {
        for w in c.words.iter().chain(c.redirects.iter().map(|r| &r.target)) {
            if w.expansion {
                why = why.or(Some("variable expansion or command substitution"));
            }
            if w.tilde {
                why = why.or(Some("tilde expansion"));
            }
        }
        if let Some(first) = c.words.first() {
            if is_assignment(first) {
                why = why.or(Some("environment assignment"));
            }
            if !first.quoted && KEYWORDS.contains(&first.text.as_str()) {
                why = why.or(Some("shell control flow"));
            }
        }
    }
    if let Some(w) = why {
        fail(&mut p, w);
    }
    p
}

// ------------------------------------------------------------------ semantic table

/// What a known command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEffect {
    /// Pure: reads the workspace (plus path arguments outside it).
    ReadOnly,
    /// LocalWrite on the workspace plus `git:refs`.
    RepoWrite,
    /// `git push`: Irreversible, network to the remote's host.
    GitPush,
    /// `curl`: Network to the hosts of its URL arguments.
    Curl,
    /// `wget`: Network to its URL hosts; writes the workspace.
    Wget,
    /// Behaviour defined by a workspace file (`npm run`, `make`, `cargo`): the
    /// `cmd:` access carries the sha256 of the first existing file of `files`.
    /// LocalWrite over the workspace, offline.
    DefinitionBound { files: Vec<String> },
    /// Explicit class and accesses; `{ws}` in a URI is replaced by the workspace.
    Custom {
        class: EffectClass,
        accesses: Vec<(String, AccessMode)>,
    },
    /// Never analysed.
    Opaque,
}

/// One table entry: an argv prefix and its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub prefix: Vec<String>,
    pub effect: CommandEffect,
    /// Flags that make the command unanalysable (e.g. `find -exec`).
    pub deny_flags: Vec<String>,
}

impl Rule {
    /// `prefix` is split on whitespace (`"git diff"`).
    pub fn new(prefix: &str, effect: CommandEffect) -> Self {
        Rule {
            prefix: prefix.split_whitespace().map(str::to_string).collect(),
            effect,
            deny_flags: vec![],
        }
    }
    pub fn deny(mut self, flags: &[&str]) -> Self {
        self.deny_flags.extend(flags.iter().map(|f| f.to_string()));
        self
    }
    fn denies(&self, args: &[String]) -> Option<String> {
        for a in args {
            for f in &self.deny_flags {
                let short = f.len() == 2 && f.starts_with('-') && !f.starts_with("--");
                let hit = a == f
                    || a.starts_with(&format!("{f}="))
                    || (short
                        && a.starts_with('-')
                        && !a.starts_with("--")
                        && a[1..].contains(&f[1..]));
                if hit {
                    return Some(f.clone());
                }
            }
        }
        None
    }
}

/// Maps commands to accesses and classes. Extensible from configuration via
/// [`SemanticTable::extend`]; the longest matching prefix wins, later rules win
/// ties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticTable {
    rules: Vec<Rule>,
}

impl Default for SemanticTable {
    fn default() -> Self {
        Self::defaults()
    }
}

const READ_ONLY: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "rg",
    "grep",
    "egrep",
    "fgrep",
    "find",
    "pwd",
    "echo",
    "printf",
    "which",
    "true",
    "false",
    "exit",
    "sort",
    "uniq",
    "cut",
    "tr",
    "diff",
    "cmp",
    "file",
    "stat",
    "du",
    "df",
    "tree",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "date",
    "whoami",
    "uname",
    "nl",
    "jq",
    "sha256sum",
    "sha1sum",
    "md5sum",
    "test",
    "[",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git blame",
    "git rev-parse",
    "git ls-files",
    "git grep",
    "cargo --version",
    "rustc --version",
    "node --version",
    "python3 --version",
];

const GIT_WRITE: &[&str] = &[
    "add",
    "commit",
    "checkout",
    "switch",
    "restore",
    "reset",
    "merge",
    "rebase",
    "stash",
    "branch",
    "tag",
    "cherry-pick",
    "revert",
    "mv",
    "rm",
    "init",
    "apply",
    "am",
    "clean",
];

impl SemanticTable {
    /// A table with no rules: every command is Opaque.
    pub fn empty() -> Self {
        SemanticTable { rules: vec![] }
    }

    /// The built-in table.
    pub fn defaults() -> Self {
        let mut rules: Vec<Rule> = READ_ONLY
            .iter()
            .map(|c| Rule::new(c, CommandEffect::ReadOnly))
            .collect();
        for r in &mut rules {
            let deny: &[&str] = match r.prefix.join(" ").as_str() {
                "find" => &[
                    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0",
                    "-fprintf", "-fls",
                ],
                "rg" => &["--pre"],
                "sort" => &["-o", "--output"],
                "tree" => &["-o"],
                "date" => &["-s", "--set"],
                "git diff" | "git log" | "git show" => &["--output", "--ext-diff"],
                "git grep" => &["--open-files-in-pager", "-O"],
                _ => &[],
            };
            r.deny_flags = deny.iter().map(|s| s.to_string()).collect();
        }
        for sub in GIT_WRITE {
            rules.push(Rule::new(&format!("git {sub}"), CommandEffect::RepoWrite));
        }
        rules.push(Rule::new("git push", CommandEffect::GitPush));
        rules.push(Rule::new("curl", CommandEffect::Curl).deny(&["-K", "--config"]));
        rules.push(Rule::new("wget", CommandEffect::Wget).deny(&[
            "-i",
            "--input-file",
            "-e",
            "--execute",
        ]));
        let cargo = CommandEffect::DefinitionBound {
            files: vec!["Cargo.toml".into()],
        };
        for sub in [
            "build", "test", "check", "run", "clippy", "bench", "doc", "fmt", "nextest",
        ] {
            rules.push(Rule::new(&format!("cargo {sub}"), cargo.clone()));
        }
        let npm = CommandEffect::DefinitionBound {
            files: vec!["package.json".into()],
        };
        for p in [
            "npm run",
            "npm test",
            "npm start",
            "yarn run",
            "yarn test",
            "pnpm run",
            "pnpm test",
        ] {
            rules.push(Rule::new(p, npm.clone()));
        }
        let make = CommandEffect::DefinitionBound {
            files: vec!["GNUmakefile".into(), "makefile".into(), "Makefile".into()],
        };
        rules.push(Rule::new("make", make).deny(&[
            "-f",
            "--file",
            "--makefile",
            "-C",
            "--directory",
        ]));
        SemanticTable { rules }
    }

    /// Add rules (they take precedence over existing rules of equal length).
    pub fn extend(&mut self, rules: impl IntoIterator<Item = Rule>) -> &mut Self {
        self.rules.extend(rules);
        self
    }

    pub fn with(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The rule for an argv (longest prefix; later rules win ties).
    pub fn lookup(&self, argv: &[String]) -> Option<&Rule> {
        let mut best: Option<&Rule> = None;
        for r in &self.rules {
            if r.prefix.is_empty() || r.prefix.len() > argv.len() {
                continue;
            }
            if r.prefix.iter().zip(argv).all(|(a, b)| a == b)
                && best
                    .map(|b| r.prefix.len() >= b.prefix.len())
                    .unwrap_or(true)
            {
                best = Some(r);
            }
        }
        best
    }

    /// Analyse a command line relative to `workspace`.
    pub fn analyze(&self, command: &str, workspace: &Path) -> ShellAnalysis {
        analyze(command, self, workspace)
    }
}

// ------------------------------------------------------------------ analysis

/// Analysis of one simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAnalysis {
    /// The `cmd:` resource line.
    pub line: String,
    pub class: EffectClass,
    pub accesses: Vec<Access>,
    /// Why it is Opaque, if so.
    pub opaque_reason: Option<String>,
}

/// Analysis of a whole command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAnalysis {
    pub commands: Vec<CommandAnalysis>,
    /// Max class across simple commands.
    pub class: EffectClass,
    /// Union of the accesses (sorted, deduplicated).
    pub accesses: Vec<Access>,
    /// Why the line is Opaque, if so.
    pub opaque_reason: Option<String>,
}

fn ws_glob(ws: &Path) -> ResourceUri {
    let s = ws.to_string_lossy();
    if s == "/" {
        ResourceUri::fs("/**")
    } else {
        ResourceUri::fs(&format!("{}/**", s.trim_end_matches('/')))
    }
}

fn fs_path(p: &Path) -> ResourceUri {
    ResourceUri::fs(&p.to_string_lossy())
}

impl ShellAnalysis {
    /// The Opaque declaration: each simple command (or the whole line) as a
    /// `cmd:` write, plus a write of the whole workspace (exclusive).
    pub fn opaque(
        command: &str,
        lines: &[String],
        workspace: &Path,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let lines: Vec<String> = if lines.is_empty() {
            vec![command.trim().to_string()]
        } else {
            lines.to_vec()
        };
        let ws = fsafe::normalize(workspace);
        let commands: Vec<CommandAnalysis> = lines
            .iter()
            .map(|l| CommandAnalysis {
                line: l.clone(),
                class: EffectClass::Opaque,
                accesses: vec![Access::write(ResourceUri::cmd(l))],
                opaque_reason: Some(reason.clone()),
            })
            .collect();
        let mut set: BTreeSet<Access> = commands
            .iter()
            .flat_map(|c| c.accesses.iter().cloned())
            .collect();
        set.insert(Access::write(ws_glob(&ws)));
        ShellAnalysis {
            commands,
            class: EffectClass::Opaque,
            accesses: set.into_iter().collect(),
            opaque_reason: Some(reason),
        }
    }
}

/// Analyse `command` with `table` relative to `workspace`.
pub fn analyze(command: &str, table: &SemanticTable, workspace: &Path) -> ShellAnalysis {
    let ws = fsafe::normalize(workspace);
    let parsed = parse(command);
    let lines: Vec<String> = parsed.commands.iter().map(|c| c.line()).collect();
    if let Some(why) = &parsed.opaque {
        return ShellAnalysis::opaque(
            command,
            if parsed.commands.is_empty() {
                &[]
            } else {
                &lines
            },
            &ws,
            why.clone(),
        );
    }
    let mut commands = Vec::new();
    for c in &parsed.commands {
        let a = analyze_simple(c, table, &ws);
        if let Some(why) = &a.opaque_reason {
            return ShellAnalysis::opaque(command, &lines, &ws, format!("`{}`: {why}", a.line));
        }
        commands.push(a);
    }
    let class = commands
        .iter()
        .map(|c| c.class)
        .max()
        .unwrap_or(EffectClass::Opaque);
    let set: BTreeSet<Access> = commands
        .iter()
        .flat_map(|c| c.accesses.iter().cloned())
        .collect();
    ShellAnalysis {
        commands,
        class,
        accesses: set.into_iter().collect(),
        opaque_reason: None,
    }
}

fn opaque_cmd(line: String, why: impl Into<String>) -> CommandAnalysis {
    CommandAnalysis {
        accesses: vec![Access::write(ResourceUri::cmd(&line))],
        line,
        class: EffectClass::Opaque,
        opaque_reason: Some(why.into()),
    }
}

fn resolve_arg(ws: &Path, arg: &str) -> PathBuf {
    let p = Path::new(arg);
    fsafe::normalize(&if p.is_absolute() {
        p.to_path_buf()
    } else {
        ws.join(p)
    })
}

fn analyze_simple(c: &SimpleCommand, table: &SemanticTable, ws: &Path) -> CommandAnalysis {
    let line = c.line();
    let argv = c.argv();
    let Some(rule) = table.lookup(&argv) else {
        return opaque_cmd(line, "unknown command");
    };
    let args = &argv[rule.prefix.len()..];
    if let Some(flag) = rule.denies(args) {
        return opaque_cmd(line, format!("flag `{flag}`"));
    }
    let mut acc: Vec<Access> = vec![];
    let mut class;
    match &rule.effect {
        CommandEffect::Opaque => return opaque_cmd(line, "marked opaque"),
        CommandEffect::ReadOnly => {
            class = EffectClass::Pure;
            acc.push(Access::read(ResourceUri::cmd(&line)));
            acc.push(Access::read(ws_glob(ws)));
            for a in args {
                if a.starts_with('-') {
                    continue;
                }
                if a.contains('/') || a == ".." {
                    let p = resolve_arg(ws, a);
                    if !p.starts_with(ws) {
                        acc.push(Access::read(fs_path(&p)));
                    }
                }
            }
        }
        CommandEffect::RepoWrite => {
            class = EffectClass::LocalWrite;
            acc.push(Access::write(ResourceUri::cmd(&line)));
            acc.push(Access::write(ws_glob(ws)));
            acc.push(Access::write(ResourceUri::git("refs")));
        }
        CommandEffect::GitPush => {
            class = EffectClass::Irreversible;
            acc.push(Access::write(ResourceUri::cmd(&line)));
            acc.push(Access::read(ws_glob(ws)));
            acc.push(Access::read(ResourceUri::git("refs")));
            let remote = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .cloned()
                .unwrap_or_else(|| "origin".into());
            let url = if remote.contains("://") || remote.contains('@') {
                Some(remote.clone())
            } else {
                git_remote_url(ws, &remote)
            };
            let net = url
                .as_deref()
                .and_then(remote_host)
                .map(|(h, p)| ResourceUri::net(&h, p))
                .unwrap_or_else(|| ResourceUri("net:*".into()));
            acc.push(Access::write(net));
        }
        CommandEffect::Curl | CommandEffect::Wget => {
            class = EffectClass::Network;
            acc.push(Access::write(ResourceUri::cmd(&line)));
            acc.push(Access::read(ws_glob(ws)));
            let mut hosts = 0;
            for a in args {
                if a.starts_with("http://") || a.starts_with("https://") {
                    match crate::caps::parse_url(a) {
                        Ok((_, h, p)) => {
                            acc.push(Access::write(ResourceUri::net(&h, p)));
                            hosts += 1;
                        }
                        Err(_) => return opaque_cmd(line, "unparsable url"),
                    }
                } else if !a.starts_with('-') && a.contains("://") {
                    return opaque_cmd(line, "non-http url");
                }
            }
            if hosts == 0 {
                return opaque_cmd(line, "no http(s) url");
            }
            let writes = matches!(rule.effect, CommandEffect::Wget)
                || args.iter().any(|a| {
                    matches!(
                        a.as_str(),
                        "--output"
                            | "--remote-name"
                            | "--remote-name-all"
                            | "--dump-header"
                            | "--cookie-jar"
                    ) || a.starts_with("--output=")
                        || (a.starts_with('-')
                            && !a.starts_with("--")
                            && a[1..].contains(['o', 'O', 'D', 'c']))
                });
            if writes {
                acc.push(Access::write(ws_glob(ws)));
            }
        }
        CommandEffect::DefinitionBound { files } => {
            class = EffectClass::LocalWrite;
            let hash = files.iter().find_map(|f| {
                fsafe::read_opt(ws, &ws.join(f))
                    .ok()
                    .flatten()
                    .map(|b| sha256_hex(&b))
            });
            acc.push(Access {
                resource: ResourceUri::cmd(&line),
                mode: AccessMode::Write,
                content_hash: hash,
            });
            acc.push(Access::write(ws_glob(ws)));
        }
        CommandEffect::Custom { class: k, accesses } => {
            class = *k;
            acc.push(Access {
                resource: ResourceUri::cmd(&line),
                mode: if *k == EffectClass::Pure {
                    AccessMode::Read
                } else {
                    AccessMode::Write
                },
                content_hash: None,
            });
            let wss = ws.to_string_lossy();
            for (uri, mode) in accesses {
                let u = uri.replace("{ws}", wss.trim_end_matches('/'));
                match ResourceUri::parse(&u) {
                    Ok(r) => acc.push(Access {
                        resource: r,
                        mode: *mode,
                        content_hash: None,
                    }),
                    Err(_) => return opaque_cmd(line, format!("bad rule uri `{u}`")),
                }
            }
        }
    }
    // Redirections.
    for r in &c.redirects {
        let t = &r.target.text;
        match r.op {
            ">&" | "<&" if t == "-" || t.chars().all(|c| c.is_ascii_digit()) => {}
            "<" | "<&" => {
                let p = resolve_arg(ws, t);
                acc.push(Access::read(fs_path(&p)));
            }
            ">" | ">>" | ">|" | "&>" | "&>>" | ">&" => {
                if t == "/dev/null" {
                    continue;
                }
                if r.target.glob {
                    return opaque_cmd(line, "glob in redirection target");
                }
                let p = resolve_arg(ws, t);
                if !p.starts_with(ws) {
                    return opaque_cmd(line, "redirection outside the workspace");
                }
                acc.push(Access::write(fs_path(&p)));
                class = class.max(EffectClass::LocalWrite);
                // A read-only command writing a file: its cmd access is a write now.
                for a in acc.iter_mut() {
                    if a.resource.scheme() == Some(agent_proto::Scheme::Cmd) {
                        a.mode = AccessMode::Write;
                    }
                }
            }
            _ => return opaque_cmd(line, format!("redirection `{}`", r.op)),
        }
    }
    CommandAnalysis {
        line,
        class,
        accesses: acc,
        opaque_reason: None,
    }
}

/// The url of `[remote "<name>"]` in `.git/config`.
fn git_remote_url(ws: &Path, remote: &str) -> Option<String> {
    let bytes = fsafe::read_opt(ws, &ws.join(".git/config"))
        .ok()
        .flatten()?;
    let text = String::from_utf8_lossy(&bytes);
    let header = format!("[remote \"{remote}\"]");
    let mut in_section = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_section = l == header;
            continue;
        }
        if in_section {
            if let Some((k, v)) = l.split_once('=') {
                if k.trim() == "url" {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// Host and port of a git remote url (`https://`, `ssh://`, `git@host:path`).
fn remote_host(url: &str) -> Option<(String, u16)> {
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority = rest.split('/').next()?;
        let authority = authority.rsplit('@').next()?;
        let default = match scheme {
            "https" => 443,
            "http" => 80,
            "ssh" | "git+ssh" => 22,
            "git" => 9418,
            _ => return None,
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), default),
        };
        return (!host.is_empty()).then_some((host, port));
    }
    // scp-like: [user@]host:path
    let (before, _) = url.split_once(':')?;
    let host = before.rsplit('@').next()?;
    (!host.is_empty() && !host.contains('/')).then(|| (host.to_string(), 22))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_and_split() {
        let p = parse("rg foo | head -n 3 && (ls src; pwd) || echo 'a b'");
        assert_eq!(p.opaque, None);
        let lines: Vec<_> = p.commands.iter().map(|c| c.line()).collect();
        assert_eq!(
            lines,
            ["rg foo", "head -n 3", "ls src", "pwd", "echo 'a b'"]
        );
        assert_eq!(p.commands[4].argv(), ["echo", "a b"]);
    }

    #[test]
    fn redirects_parsed() {
        let p = parse("cargo test 2>&1 > out.txt");
        assert_eq!(p.opaque, None);
        assert_eq!(p.commands[0].line(), "cargo test");
        assert_eq!(p.commands[0].redirects.len(), 2);
        assert_eq!(p.commands[0].redirects[0].fd, Some(2));
    }

    #[test]
    fn opaque_constructs() {
        for c in [
            "echo $HOME",
            "echo \"$(whoami)\"",
            "echo `id`",
            "cat <<EOF\nx\nEOF",
            "sleep 10 &",
            "FOO=1 ls",
            "for f in *; do rm $f; done",
            "ls |",
            "echo 'unterminated",
            "cat ~/.ssh/id_rsa",
            "",
            "(ls",
        ] {
            assert!(parse(c).opaque.is_some(), "{c:?} should be opaque");
        }
        // Single quotes suppress expansion.
        assert_eq!(parse("echo '$HOME'").opaque, None);
    }

    #[test]
    fn remote_hosts() {
        assert_eq!(
            remote_host("https://github.com/a/b.git"),
            Some(("github.com".into(), 443))
        );
        assert_eq!(
            remote_host("git@github.com:a/b.git"),
            Some(("github.com".into(), 22))
        );
        assert_eq!(
            remote_host("ssh://git@host:2222/x"),
            Some(("host".into(), 2222))
        );
    }
}
