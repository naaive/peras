//! Discovery: the only part of this crate that does IO. It reads every relevant
//! file into a [`Sources`] value; [`crate::compile`] is then a pure function of it.

use agent_proto::ToolSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Where a discovered (non-settings) file came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `~/.agent/...`: always trusted.
    User,
    /// Inside the project: trusted only in a trusted workspace.
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub scope: Scope,
    pub text: String,
}

impl SourceFile {
    pub fn new(path: impl Into<String>, scope: Scope, text: impl Into<String>) -> Self {
        SourceFile { path: path.into(), scope, text: text.into() }
    }
}

/// Raw inputs of compilation: TOML text per layer plus discovered files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sources {
    pub managed: Option<String>,
    pub cli: Option<String>,
    pub local_project: Option<String>,
    pub shared_project: Option<String>,
    pub user: Option<String>,
    /// Absolute project root (becomes `security.workspace_root`).
    pub project_root: Option<String>,
    /// Instruction files, broadest first (user file, then project root -> cwd).
    pub instructions: Vec<SourceFile>,
    /// `SKILL.md` files (`.agent/skills/<name>/SKILL.md`).
    pub skills: Vec<SourceFile>,
    /// `.agent/commands/*.md`.
    pub commands: Vec<SourceFile>,
    /// `.agent/agents/*.md`.
    pub agents: Vec<SourceFile>,
    /// Tool specs known at compile time (usually filled later via `Profile::with_tools`).
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Plugin bundles (`~/.agent/plugins/*`, `.agent/plugins/*`), user first.
    #[serde(default)]
    pub plugins: Vec<PluginSource>,
}

/// A discovered plugin directory: its manifest and bundled files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSource {
    /// Plugin directory.
    pub path: String,
    pub scope: Scope,
    /// `plugin.toml` text.
    pub manifest: String,
    #[serde(default)]
    pub instructions: Vec<SourceFile>,
    #[serde(default)]
    pub skills: Vec<SourceFile>,
    #[serde(default)]
    pub commands: Vec<SourceFile>,
    #[serde(default)]
    pub agents: Vec<SourceFile>,
}

/// Plugin manifest file name.
pub const PLUGIN_MANIFEST: &str = "plugin.toml";

fn collect_plugins(dir: &Path, scope: Scope, out: &mut Vec<PluginSource>) -> io::Result<()> {
    for p in sorted_entries(dir)? {
        let Some(manifest) = read_opt(&p.join(PLUGIN_MANIFEST))? else { continue };
        let mut ps = PluginSource { path: lossy(&p), scope, manifest, ..Default::default() };
        for name in INSTRUCTION_FILES {
            if let Some(t) = read_opt(&p.join(name))? {
                ps.instructions.push(SourceFile::new(lossy(&p.join(name)), scope, t));
            }
        }
        collect_skills(&p.join("skills"), scope, &mut ps.skills)?;
        collect_md(&p.join("commands"), scope, &mut ps.commands)?;
        collect_md(&p.join("agents"), scope, &mut ps.agents)?;
        out.push(ps);
    }
    Ok(())
}

impl Default for PluginSource {
    fn default() -> Self {
        PluginSource {
            path: String::new(),
            scope: Scope::Project,
            manifest: String::new(),
            instructions: vec![],
            skills: vec![],
            commands: vec![],
            agents: vec![],
        }
    }
}

/// Every location whose change affects the compiled profile (for hot reload):
/// `(path, recursive)`. Directories that do not exist yet are included.
pub fn config_locations(opts: &DiscoverOptions) -> Vec<(PathBuf, bool)> {
    let cwd = std::fs::canonicalize(&opts.cwd).unwrap_or_else(|_| opts.cwd.clone());
    let root = match &opts.project_root {
        Some(r) => std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()),
        None => find_project_root(&cwd),
    };
    let mut out = vec![];
    if let Some(m) = &opts.managed_path {
        out.push((m.clone(), false));
    }
    let mut agent_dirs = vec![root.join(".agent")];
    if let Some(h) = &opts.home {
        agent_dirs.push(h.join(".agent"));
    }
    for d in agent_dirs {
        out.push((d.clone(), false));
        for sub in ["skills", "commands", "agents", "plugins"] {
            out.push((d.join(sub), true));
        }
    }
    out.push((root.clone(), false));
    if let Ok(rel) = cwd.strip_prefix(&root) {
        let mut d = root.clone();
        for c in rel.components() {
            d = d.join(c);
            out.push((d.clone(), false));
        }
    }
    out
}

/// Whether a changed path can affect the compiled profile.
pub fn is_config_path(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if INSTRUCTION_FILES.contains(&name) || name == PLUGIN_MANIFEST || (name.ends_with(".toml") && name.starts_with("settings")) {
        return true;
    }
    if name == "managed.toml" {
        return true;
    }
    let in_ext = p.components().any(|c| matches!(c.as_os_str().to_str(), Some("skills" | "commands" | "agents" | "plugins")));
    in_ext && p.components().any(|c| c.as_os_str() == ".agent") && (name.ends_with(".md") || name.ends_with(".toml"))
}

/// Instruction file names, in the order they are collected within a directory.
pub const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Default location of the managed policy file (overridden by `AGENT_MANAGED_CONFIG`).
pub const DEFAULT_MANAGED_PATH: &str = "/etc/agent/managed.toml";

#[derive(Debug, Clone, Default)]
pub struct DiscoverOptions {
    pub cwd: PathBuf,
    /// Project root; `None` = nearest ancestor with `.git`, else `cwd`.
    pub project_root: Option<PathBuf>,
    /// Home directory; `None` = no user layer.
    pub home: Option<PathBuf>,
    /// Managed policy file; `None` = no managed layer.
    pub managed_path: Option<PathBuf>,
    /// CLI layer as TOML (see [`crate::Settings::to_toml`]).
    pub cli: Option<String>,
    pub tools: Vec<ToolSpec>,
}

impl DiscoverOptions {
    /// Options from the process environment: `$HOME`, `AGENT_MANAGED_CONFIG`
    /// (default `/etc/agent/managed.toml`).
    pub fn from_env(cwd: impl Into<PathBuf>) -> Self {
        DiscoverOptions {
            cwd: cwd.into(),
            project_root: None,
            home: std::env::var_os("HOME").map(PathBuf::from),
            managed_path: Some(
                std::env::var_os("AGENT_MANAGED_CONFIG")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_MANAGED_PATH)),
            ),
            cli: None,
            tools: vec![],
        }
    }
}

/// Nearest ancestor of `cwd` (inclusive) containing `.git`, else `cwd`.
pub fn find_project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

fn read_opt(p: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::IsADirectory => Ok(None),
        Err(e) => Err(e),
    }
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Sorted directory entries (deterministic order); missing dir = empty.
fn sorted_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut out = BTreeMap::new();
    for e in rd {
        let e = e?;
        out.insert(e.file_name(), e.path());
    }
    Ok(out.into_values().collect())
}

fn collect_md(dir: &Path, scope: Scope, out: &mut Vec<SourceFile>) -> io::Result<()> {
    for p in sorted_entries(dir)? {
        if p.extension().and_then(|e| e.to_str()) == Some("md") && p.is_file() {
            if let Some(t) = read_opt(&p)? {
                out.push(SourceFile::new(lossy(&p), scope, t));
            }
        }
    }
    Ok(())
}

fn collect_skills(dir: &Path, scope: Scope, out: &mut Vec<SourceFile>) -> io::Result<()> {
    for p in sorted_entries(dir)? {
        let f = p.join("SKILL.md");
        if let Some(t) = read_opt(&f)? {
            out.push(SourceFile::new(lossy(&f), scope, t));
        }
    }
    Ok(())
}

/// Read every configuration source. Missing files are simply absent.
pub fn discover(opts: &DiscoverOptions) -> io::Result<Sources> {
    let cwd = std::fs::canonicalize(&opts.cwd).unwrap_or_else(|_| opts.cwd.clone());
    let root = match &opts.project_root {
        Some(r) => std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()),
        None => find_project_root(&cwd),
    };
    let mut s = Sources {
        cli: opts.cli.clone(),
        project_root: Some(lossy(&root)),
        tools: opts.tools.clone(),
        ..Default::default()
    };
    if let Some(m) = &opts.managed_path {
        s.managed = read_opt(m)?;
    }
    let pdir = root.join(".agent");
    s.local_project = read_opt(&pdir.join("settings.local.toml"))?;
    s.shared_project = read_opt(&pdir.join("settings.toml"))?;

    let udir = opts.home.as_ref().map(|h| h.join(".agent"));
    // Avoid reading the same directory twice when the project *is* the home dir.
    let udir = udir.filter(|u| *u != pdir);
    if let Some(u) = &udir {
        s.user = read_opt(&u.join("settings.toml"))?;
        for name in INSTRUCTION_FILES {
            if let Some(t) = read_opt(&u.join(name))? {
                s.instructions.push(SourceFile::new(lossy(&u.join(name)), Scope::User, t));
            }
        }
    }

    // Project root down to cwd (only if cwd is inside root).
    let mut dirs: Vec<PathBuf> = vec![root.clone()];
    if let Ok(rel) = cwd.strip_prefix(&root) {
        let mut d = root.clone();
        for c in rel.components() {
            d = d.join(c);
            dirs.push(d.clone());
        }
    }
    for d in &dirs {
        for name in INSTRUCTION_FILES {
            let p = d.join(name);
            if let Some(t) = read_opt(&p)? {
                s.instructions.push(SourceFile::new(lossy(&p), Scope::Project, t));
            }
        }
    }

    if let Some(u) = &udir {
        collect_skills(&u.join("skills"), Scope::User, &mut s.skills)?;
        collect_md(&u.join("commands"), Scope::User, &mut s.commands)?;
        collect_md(&u.join("agents"), Scope::User, &mut s.agents)?;
    }
    collect_skills(&pdir.join("skills"), Scope::Project, &mut s.skills)?;
    collect_md(&pdir.join("commands"), Scope::Project, &mut s.commands)?;
    collect_md(&pdir.join("agents"), Scope::Project, &mut s.agents)?;
    if let Some(u) = &udir {
        collect_plugins(&u.join("plugins"), Scope::User, &mut s.plugins)?;
    }
    collect_plugins(&pdir.join("plugins"), Scope::Project, &mut s.plugins)?;
    Ok(s)
}
