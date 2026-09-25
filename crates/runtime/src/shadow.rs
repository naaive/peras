//! Shadow checkpointer: a content-addressed snapshot store kept in a
//! directory OUTSIDE the workspace, fully separate from the user's `.git`.
//!
//! - **Scan**: walks the workspace (skipping `.git`), hashing only files whose
//!   size/mtime changed since the last scan. Ignore rules follow git: every
//!   `.gitignore` on the way down (nested files, negations, directory-only
//!   patterns, via the `ignore` crate) plus `.git/info/exclude`. Ignored
//!   files are recorded as path + metadata only (content not saved, never
//!   restored). An ignored directory is recorded once, as `dir/` with its own
//!   metadata, and never descended into (so a huge `target/` or
//!   `node_modules/` costs one `stat`); changes inside it are only noticed when
//!   the directory's own mtime changes (entries added, removed or renamed).
//! - **Git refs**: every checkpoint records `HEAD` and all refs (loose files
//!   under `.git/refs/` and `packed-refs`). `restore` lists ref changes since
//!   the target checkpoint in `RestoreReport::git_refs` as
//!   `"<ref>: <before> -> <after>"`; refs are never restored automatically.
//! - **Declared writes**: `save_originals` stores the exact original bytes of
//!   declared write paths before a batch runs and registers the declared
//!   patterns (globs allowed, e.g. an Opaque call's `fs:///ws/**`).
//! - **Attribution**: at each checkpoint, changes since the previous one whose
//!   path matches a write declared during that interval are the agent's; all
//!   other changes are external.
//! - **Restore**: for agent-attributed files only: if the current content equals
//!   the agent's last written version, the original (content before the agent's
//!   first change after the target checkpoint) is put back; if the current
//!   content already equals the original it is a no-op (idempotent); otherwise
//!   the file is reported as a conflict. Agent-changed ignored paths are listed
//!   as unrestored. Restores are themselves recorded as agent changes, so
//!   re-running the same plan converges and later rewinds stay consistent.
//!
//! State is persisted as `state.json` in the store directory after every
//! operation (atomic rename), objects under `objects/`.

use crate::mem::sha256_hex;
use crate::ports::Checkpointer;
use agent_proto::*;
use async_trait::async_trait;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileMeta {
    size: u64,
    mtime_ns: u64,
    /// Content hash (None for ignored paths).
    sha: Option<String>,
    ignored: bool,
    /// An ignored directory recorded without descending (key ends in `/`).
    #[serde(default)]
    dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Change {
    path: String,
    /// Content before / after (None = absent).
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    agent: Vec<Change>,
    external: Vec<String>,
    /// Agent-attributed changes to ignored paths (not restorable).
    agent_ignored: Vec<String>,
    external_ignored: Vec<String>,
    internal: bool,
    /// `HEAD` and refs at this checkpoint (None: not recorded / no `.git`).
    #[serde(default)]
    git_refs: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    scan: BTreeMap<String, FileMeta>,
    records: Vec<Record>,
    /// Write patterns declared for the current interval (workspace-relative globs).
    pending_patterns: Vec<String>,
    /// Originals for the current interval.
    pending_originals: BTreeMap<String, Option<String>>,
    /// Declared by `save_originals` since the last checkpoint; they belong to
    /// the interval that the next checkpoint opens.
    staged_patterns: Vec<String>,
    staged_originals: BTreeMap<String, Option<String>>,
    next_id: u64,
    scanned_once: bool,
}

struct Inner {
    root: PathBuf,
    store: PathBuf,
    state: Mutex<State>,
}

/// See module docs.
#[derive(Clone)]
pub struct ShadowCheckpointer {
    inner: Arc<Inner>,
}

fn io<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn is_glob(p: &str) -> bool {
    p.contains(['*', '?', '[', '{'])
}

fn build_globs(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        match GlobBuilder::new(p).literal_separator(true).build() {
            Ok(g) => {
                b.add(g);
            }
            Err(e) => tracing::warn!(pattern = %p, error = %e, "bad glob"),
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

/// Very small `.gitignore` subset: comments, blank lines, `dir/`, anchored
/// (`/x`, `a/b`) and unanchored (`*.log`) patterns. Negations are ignored.
///
/// Legacy helper: the scanner now uses full git semantics (see module docs).
pub fn parse_gitignore(text: &str) -> Vec<String> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let dir_only = line.ends_with('/');
        let p = line.trim_end_matches('/');
        let anchored = p.starts_with('/') || p.contains('/');
        let p = p.trim_start_matches('/');
        if p.is_empty() {
            continue;
        }
        let base = if anchored { p.to_string() } else { format!("**/{p}") };
        if !dir_only {
            out.push(base.clone());
        }
        out.push(format!("{base}/**"));
    }
    out
}

fn mtime_ns(md: &fs::Metadata) -> u64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Deepest matcher with an opinion wins (git semantics; negations whitelist).
fn is_ignored(matchers: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for m in matchers.iter().rev() {
        match m.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// The repository's git directory: `.git/`, or the target of a `.git` file
/// (`gitdir: <path>`, used by worktrees and submodules).
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    let md = fs::metadata(&dot).ok()?;
    if md.is_dir() {
        return Some(dot);
    }
    let text = fs::read_to_string(&dot).ok()?;
    let p = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let p = Path::new(p);
    Some(if p.is_absolute() { p.to_path_buf() } else { root.join(p) })
}

/// `HEAD` plus every ref (`refs/...` name -> value). Loose refs override
/// `packed-refs`. For worktrees, `HEAD` comes from the worktree's git
/// directory and refs from the common directory. `None` without a git
/// directory.
pub fn read_git_refs(root: &Path) -> Option<BTreeMap<String, String>> {
    let gd = git_dir(root)?;
    let common = match fs::read_to_string(gd.join("commondir")) {
        Ok(c) => {
            let p = Path::new(c.trim());
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                gd.join(p)
            }
        }
        Err(_) => gd.clone(),
    };
    let mut refs = BTreeMap::new();
    if let Ok(text) = fs::read_to_string(common.join("packed-refs")) {
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((sha, name)) = line.trim().split_once(' ') {
                refs.insert(name.trim().to_string(), sha.trim().to_string());
            }
        }
    }
    let mut stack = vec![common.join("refs")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let p = ent.path();
            match ent.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => {
                    let Ok(rel) = p.strip_prefix(&common) else { continue };
                    let name = rel.to_string_lossy().replace('\\', "/");
                    if name.ends_with(".lock") {
                        continue;
                    }
                    if let Ok(v) = fs::read_to_string(&p) {
                        refs.insert(name, v.trim().to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if let Ok(head) = fs::read_to_string(gd.join("HEAD")) {
        refs.insert("HEAD".into(), head.trim().to_string());
    }
    Some(refs)
}

/// `"<ref>: <before> -> <after>"` for every ref that changed (`(none)` for
/// created / deleted refs), sorted by ref name.
pub fn diff_refs(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter_map(|n| {
            let (b, a) = (before.get(n), after.get(n));
            (b != a).then(|| {
                format!(
                    "{n}: {} -> {}",
                    b.map(String::as_str).unwrap_or("(none)"),
                    a.map(String::as_str).unwrap_or("(none)")
                )
            })
        })
        .collect()
}

impl Inner {
    fn obj_path(&self, sha: &str) -> PathBuf {
        self.store.join("objects").join(&sha[..2]).join(&sha[2..])
    }

    fn put_object(&self, bytes: &[u8]) -> Result<String, String> {
        let sha = sha256_hex(bytes);
        let p = self.obj_path(&sha);
        if !p.exists() {
            fs::create_dir_all(p.parent().unwrap()).map_err(io)?;
            let tmp = p.with_extension("tmp");
            fs::write(&tmp, bytes).map_err(io)?;
            fs::rename(&tmp, &p).map_err(io)?;
        }
        Ok(sha)
    }

    fn get_object(&self, sha: &str) -> Result<Vec<u8>, String> {
        fs::read(self.obj_path(sha)).map_err(|e| format!("object {sha}: {e}"))
    }

    fn persist(&self, st: &State) -> Result<(), String> {
        let tmp = self.store.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_vec(st).map_err(io)?).map_err(io)?;
        fs::rename(&tmp, self.store.join("state.json")).map_err(io)
    }

    /// Workspace-relative path/glob for an fs access, if inside the workspace.
    fn rel(&self, a: &Access) -> Option<String> {
        if a.resource.scheme() != Some(Scheme::Fs) {
            return None;
        }
        let abs = a.resource.rest();
        let root = self.root.to_string_lossy();
        let root = root.trim_end_matches('/');
        let rest = abs.strip_prefix(root)?;
        if rest.is_empty() {
            return Some("**".into());
        }
        let rest = rest.strip_prefix('/')?;
        Some(if rest.is_empty() { "**".into() } else { rest.to_string() })
    }

    /// Matchers that apply at the workspace root: `.git/info/exclude`.
    fn root_matchers(&self) -> Vec<Gitignore> {
        let mut v = vec![];
        if let Some(gd) = git_dir(&self.root) {
            let p = gd.join("info").join("exclude");
            if p.is_file() {
                let mut b = GitignoreBuilder::new(&self.root);
                if let Some(e) = b.add(&p) {
                    tracing::debug!(path = %p.display(), error = %e, "bad exclude file");
                }
                if let Ok(g) = b.build() {
                    v.push(g);
                }
            }
        }
        v
    }

    fn scan(&self, old: &BTreeMap<String, FileMeta>) -> Result<BTreeMap<String, FileMeta>, String> {
        let mut out = BTreeMap::new();
        // (directory, matchers in effect for its entries: shallowest first).
        let mut stack: Vec<(PathBuf, Arc<Vec<Gitignore>>)> = vec![(self.root.clone(), Arc::new(self.root_matchers()))];
        while let Some((dir, inherited)) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    tracing::debug!(dir = %dir.display(), error = %e, "unreadable dir");
                    continue;
                }
            };
            let matchers = {
                let gi = dir.join(".gitignore");
                if gi.is_file() {
                    let (g, err) = Gitignore::new(&gi);
                    if let Some(e) = err {
                        tracing::debug!(path = %gi.display(), error = %e, "bad .gitignore line(s)");
                    }
                    let mut v = (*inherited).clone();
                    v.push(g);
                    Arc::new(v)
                } else {
                    inherited
                }
            };
            for ent in rd {
                let ent = ent.map_err(io)?;
                let path = ent.path();
                let md = match fs::symlink_metadata(&path) {
                    Ok(md) => md,
                    // Vanished between listing and stat.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.to_string()),
                };
                let rel = path.strip_prefix(&self.root).map_err(io)?.to_string_lossy().replace('\\', "/");
                if md.file_type().is_symlink() {
                    continue;
                }
                let is_dir = md.is_dir();
                if is_dir && ent.file_name() == ".git" {
                    continue;
                }
                if !is_dir && !md.is_file() {
                    continue;
                }
                let ignored = is_ignored(&matchers, &path, is_dir);
                let mtime_ns = mtime_ns(&md);
                if is_dir {
                    if ignored {
                        out.insert(
                            format!("{rel}/"),
                            FileMeta { size: md.len(), mtime_ns, sha: None, ignored: true, dir: true },
                        );
                    } else {
                        stack.push((path, matchers.clone()));
                    }
                    continue;
                }
                let size = md.len();
                let sha = if ignored {
                    None
                } else {
                    match old.get(&rel) {
                        Some(m) if !m.ignored && m.size == size && m.mtime_ns == mtime_ns && m.sha.is_some() => m.sha.clone(),
                        _ => match fs::read(&path) {
                            Ok(b) => Some(self.put_object(&b)?),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(e) => return Err(e.to_string()),
                        },
                    }
                };
                out.insert(rel, FileMeta { size, mtime_ns, sha, ignored, dir: false });
            }
        }
        Ok(out)
    }

    fn checkpoint(&self, scope: &CheckpointScope, internal: bool) -> Result<CheckpointInfo, String> {
        let mut st = self.state.lock().unwrap();
        let new = self.scan(&st.scan)?;
        let first = !st.scanned_once;
        let patterns = build_globs(&st.pending_patterns);
        let mut rec = Record {
            id: String::new(),
            agent: vec![],
            external: vec![],
            agent_ignored: vec![],
            external_ignored: vec![],
            internal,
            git_refs: read_git_refs(&self.root),
        };
        if !first {
            let paths: BTreeSet<&String> = st.scan.keys().chain(new.keys()).collect();
            for p in paths {
                let (o, n) = (st.scan.get(p), new.get(p));
                let ignored = o.map(|m| m.ignored).unwrap_or(false) || n.map(|m| m.ignored).unwrap_or(false);
                let changed = if ignored {
                    o.map(|m| (m.size, m.mtime_ns)) != n.map(|m| (m.size, m.mtime_ns))
                } else {
                    o.and_then(|m| m.sha.clone()) != n.and_then(|m| m.sha.clone())
                };
                if !changed {
                    continue;
                }
                // An ignored directory is the agent's when a declared write
                // falls inside it.
                let agent = patterns.is_match(p.as_str())
                    || (p.ends_with('/')
                        && (patterns.is_match(p.trim_end_matches('/'))
                            || st.pending_patterns.iter().any(|pat| pat.starts_with(p.as_str()))));
                match (ignored, agent) {
                    (true, true) => rec.agent_ignored.push(p.clone()),
                    (true, false) => rec.external_ignored.push(p.clone()),
                    (false, true) => rec.agent.push(Change {
                        path: p.clone(),
                        before: st
                            .pending_originals
                            .get(p)
                            .cloned()
                            .unwrap_or_else(|| o.and_then(|m| m.sha.clone())),
                        after: n.and_then(|m| m.sha.clone()),
                    }),
                    (false, false) => rec.external.push(p.clone()),
                }
            }
        }
        st.scan = new;
        st.scanned_once = true;
        // Open the next interval.
        let mut next_patterns = std::mem::take(&mut st.staged_patterns);
        next_patterns.extend(scope.declared_writes.iter().filter(|a| a.mode == AccessMode::Write).filter_map(|a| self.rel(a)));
        next_patterns.sort();
        next_patterns.dedup();
        st.pending_patterns = next_patterns;
        st.pending_originals = std::mem::take(&mut st.staged_originals);
        st.next_id += 1;
        rec.id = if internal { format!("ck-{}-internal", st.next_id) } else { format!("ck-{}", st.next_id) };
        let info = CheckpointInfo {
            id: CheckpointId(rec.id.clone()),
            agent_changes: rec.agent.iter().map(|c| c.path.clone()).chain(rec.agent_ignored.iter().cloned()).collect(),
            external_changes: rec.external.iter().chain(rec.external_ignored.iter()).cloned().collect(),
        };
        st.records.push(rec);
        self.persist(&st)?;
        Ok(info)
    }

    fn read_current(&self, rel: &str) -> Result<Option<Vec<u8>>, String> {
        match fs::read(self.root.join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save_originals(&self, writes: &[Access]) -> Result<(), String> {
        let mut st = self.state.lock().unwrap();
        for a in writes.iter().filter(|a| a.mode == AccessMode::Write) {
            let Some(rel) = self.rel(a) else { continue };
            if !is_glob(&rel) && !st.staged_originals.contains_key(&rel) {
                let sha = match self.read_current(&rel)? {
                    Some(b) => Some(self.put_object(&b)?),
                    None => None,
                };
                st.staged_originals.insert(rel.clone(), sha);
            }
            st.staged_patterns.push(rel);
        }
        st.staged_patterns.sort();
        st.staged_patterns.dedup();
        self.persist(&st)
    }

    fn write_content(&self, rel: &str, sha: &Option<String>) -> Result<(), String> {
        let path = self.root.join(rel);
        match sha {
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            },
            Some(sha) => {
                let bytes = self.get_object(sha)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(io)?;
                }
                fs::write(&path, bytes).map_err(io)
            }
        }
    }

    fn restore_originals(&self, writes: &[Access]) -> Result<(), String> {
        let st = self.state.lock().unwrap();
        for a in writes.iter().filter(|a| a.mode == AccessMode::Write) {
            let Some(rel) = self.rel(a) else { continue };
            let orig = st.staged_originals.get(&rel).or_else(|| st.pending_originals.get(&rel));
            if let Some(orig) = orig {
                self.write_content(&rel, orig)?;
            }
        }
        Ok(())
    }

    fn restore(&self, plan: &RestorePlan) -> Result<RestoreReport, String> {
        // Capture everything up to now (attributed with the current interval).
        self.checkpoint(&CheckpointScope { declared_writes: vec![], safe_point: true }, true)?;
        let mut st = self.state.lock().unwrap();
        let start = match &plan.checkpoint {
            None => 0,
            Some(id) => {
                st.records.iter().position(|r| r.id == id.0).ok_or_else(|| format!("unknown checkpoint {id}"))? + 1
            }
        };
        let mut per_path: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
        let mut ignored = BTreeSet::new();
        for r in &st.records[start..] {
            for c in &r.agent {
                per_path
                    .entry(c.path.clone())
                    .and_modify(|e| e.1 = c.after.clone())
                    .or_insert((c.before.clone(), c.after.clone()));
            }
            ignored.extend(r.agent_ignored.iter().cloned());
        }
        let mut report = RestoreReport::default();
        let mut undo = vec![];
        for (path, (target, last_agent)) in per_path {
            let current = st.scan.get(&path).and_then(|m| m.sha.clone());
            if current == target {
                report.restored.push(path);
            } else if current == last_agent {
                self.write_content(&path, &target)?;
                undo.push(Change { path: path.clone(), before: current, after: target.clone() });
                report.restored.push(path);
            } else {
                report.conflicts.push(path);
            }
        }
        report.unrestored_ignored = ignored.into_iter().collect();
        // Git refs: listed, never restored. Baseline = the target checkpoint
        // (or the first one); current = the checkpoint just taken.
        let baseline = match start {
            0 => st.records.first(),
            n => st.records.get(n - 1),
        };
        if let (Some(before), Some(after)) =
            (baseline.and_then(|r| r.git_refs.as_ref()), st.records.last().and_then(|r| r.git_refs.as_ref()))
        {
            report.git_refs = diff_refs(before, after);
        }
        if !undo.is_empty() {
            // Refresh scan entries of rewritten files so they are not seen as
            // external changes, and record the restore as agent changes.
            for c in &undo {
                let p = self.root.join(&c.path);
                match fs::symlink_metadata(&p) {
                    Ok(md) => {
                        st.scan.insert(
                            c.path.clone(),
                            FileMeta {
                                size: md.len(),
                                mtime_ns: mtime_ns(&md),
                                sha: c.after.clone(),
                                ignored: false,
                                dir: false,
                            },
                        );
                    }
                    Err(_) => {
                        st.scan.remove(&c.path);
                    }
                }
            }
            st.next_id += 1;
            let id = format!("ck-{}-restore", st.next_id);
            st.records.push(Record {
                id,
                agent: undo,
                external: vec![],
                agent_ignored: vec![],
                external_ignored: vec![],
                internal: true,
                git_refs: None,
            });
        }
        self.persist(&st)?;
        Ok(report)
    }
}

impl ShadowCheckpointer {
    /// `workspace`: the directory to snapshot. `store`: shadow store directory,
    /// which must be outside the workspace (created if missing). Existing state
    /// in `store` is loaded.
    pub fn new(workspace: impl AsRef<Path>, store: impl AsRef<Path>) -> Result<Self, String> {
        let root = fs::canonicalize(workspace.as_ref()).map_err(|e| format!("workspace: {e}"))?;
        fs::create_dir_all(store.as_ref()).map_err(io)?;
        let store = fs::canonicalize(store.as_ref()).map_err(io)?;
        if store.starts_with(&root) {
            return Err(format!("shadow store {} must be outside the workspace {}", store.display(), root.display()));
        }
        let state = match fs::read(store.join("state.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("state.json: {e}"))?,
            Err(_) => State::default(),
        };
        Ok(ShadowCheckpointer { inner: Arc::new(Inner { root, store, state: Mutex::new(state) }) })
    }

    pub fn workspace(&self) -> &Path {
        &self.inner.root
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || f(&inner)).await.map_err(io)?
    }
}

#[async_trait]
impl Checkpointer for ShadowCheckpointer {
    async fn checkpoint(&self, scope: &CheckpointScope) -> Result<CheckpointInfo, String> {
        let scope = scope.clone();
        self.blocking(move |i| i.checkpoint(&scope, false)).await
    }

    async fn save_originals(&self, writes: &[Access]) -> Result<(), String> {
        let writes = writes.to_vec();
        self.blocking(move |i| i.save_originals(&writes)).await
    }

    async fn restore(&self, plan: &RestorePlan) -> Result<RestoreReport, String> {
        let plan = plan.clone();
        self.blocking(move |i| i.restore(&plan)).await
    }

    async fn restore_originals(&self, writes: &[Access]) -> Result<(), String> {
        let writes = writes.to_vec();
        self.blocking(move |i| i.restore_originals(&writes)).await
    }
}
