//! Copy-based isolated execution.
//!
//! Works everywhere (no overlayfs or user namespaces needed): the workspace
//! is copied into a scratch directory (reflinks where the filesystem supports
//! them, never hardlinks, so writes in the copy can not reach the original),
//! the command runs against the copy, and afterwards the exact change list is
//! computed by comparing the copy with the baseline recorded right after the
//! copy was made. Nothing is written back; approved changes are merged with
//! [`apply_isolated_changes`].
//!
//! `.git/objects` is not copied: the copy gets an empty object store whose
//! `info/alternates` points at the original (read-only) objects, so git reads
//! work and new objects land in the copy.
//!
//! Runs awaiting review are kept by a [`Staging`] area on disk (durable in the
//! framework data directory when [`Staging::set_root`] is given one), so a
//! staged run survives a restart between the run and its merge, and merging
//! is idempotent: a re-dispatched merge after a crash recognizes changes it
//! already applied (design: crash recovery re-executes the merge per plan).

use agent_runtime::ExecOutput;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Kind of change in an isolated run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One change, with a workspace-relative path (`/`-separated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

impl AsRef<str> for Change {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    Dir,
    File,
    Symlink(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    kind: Kind,
    size: u64,
    mode: u32,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Meta {
    fn of(p: &Path) -> io::Result<Option<Meta>> {
        let m = std::fs::symlink_metadata(p)?;
        let ft = m.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink(std::fs::read_link(p)?)
        } else {
            return Ok(None); // sockets, fifos, devices: not tracked
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Some(Meta {
                kind,
                size: m.size(),
                mode: m.mode() & 0o7777,
                ino: m.ino(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
            }))
        }
        #[cfg(not(unix))]
        {
            let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).unwrap_or_default();
            let mtime = (t.as_secs() as i64, t.subsec_nanos() as i64);
            Ok(Some(Meta {
                kind,
                size: m.len(),
                mode: if m.permissions().readonly() { 0o444 } else { 0o644 },
                ino: 0,
                mtime,
                ctime: mtime,
            }))
        }
    }

    /// Like [`Meta::of`], `None` also when `p` does not exist.
    fn of_opt(p: &Path) -> io::Result<Option<Meta>> {
        match Meta::of(p) {
            Ok(m) => Ok(m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Same content for sure (metadata untouched, including ctime, which
    /// user space cannot set).
    fn untouched(&self, other: &Meta) -> bool {
        self == other
    }
}

/// A scratch copy of a workspace.
#[derive(Debug)]
pub struct IsolatedCopy {
    scratch: tempfile::TempDir,
    root: PathBuf,
    tmp: PathBuf,
    workspace: PathBuf,
    /// Copy-side metadata right after setup.
    baseline: BTreeMap<String, Meta>,
    /// Original-side metadata at copy time (to compare content cheaply).
    originals: BTreeMap<String, Meta>,
}

/// Path at which the original `.git/objects` is visible to the command, for
/// the copy's `objects/info/alternates`. `None` = the original path itself.
pub type GitObjectsAlias<'a> = Option<&'a Path>;

impl IsolatedCopy {
    /// Copy `workspace` into a new scratch directory.
    pub fn create(workspace: &Path, git_objects_alias: GitObjectsAlias<'_>) -> io::Result<Self> {
        if workspace.as_os_str().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "isolated execution needs a workspace (cwd)"));
        }
        let workspace = std::fs::canonicalize(workspace)?;
        let scratch = tempfile::Builder::new().prefix("agent-iso-").tempdir()?;
        let root = scratch.path().join("ws");
        let tmp = scratch.path().join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let mut originals = BTreeMap::new();
        copy_tree(&workspace, &root, "", &mut originals)?;
        let git = workspace.join(".git");
        if std::fs::symlink_metadata(&git).map(|m| m.is_dir()).unwrap_or(false) {
            setup_git_objects(&git.join("objects"), &root.join(".git/objects"), git_objects_alias)?;
        }
        let mut baseline = BTreeMap::new();
        snapshot(&root, &root, &mut baseline)?;
        Ok(IsolatedCopy { scratch, root, tmp, workspace, baseline, originals })
    }

    /// The copy (use as the command's workspace).
    pub fn path(&self) -> &Path {
        &self.root
    }
    /// A private temp directory next to the copy (for `TMPDIR`).
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }
    /// The (canonical) original workspace.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    /// The scratch directory holding the copy.
    pub fn scratch(&self) -> &Path {
        self.scratch.path()
    }

    /// Exact list of changes in the copy since it was made.
    pub fn changes(&self) -> io::Result<Vec<Change>> {
        let mut after = BTreeMap::new();
        snapshot(&self.root, &self.root, &mut after)?;
        let mut out = vec![];
        for (path, a) in &after {
            let kind = match self.baseline.get(path) {
                None => ChangeKind::Added,
                Some(b) if b.untouched(a) => continue,
                Some(b) => {
                    if b.kind != a.kind || b.mode != a.mode {
                        ChangeKind::Modified
                    } else if a.kind == Kind::Dir {
                        continue; // directory entries changed: reported per entry
                    } else if matches!(a.kind, Kind::Symlink(_)) {
                        continue; // same target (kind compared above)
                    } else if self.same_as_original(path, a) {
                        continue; // rewritten with identical content
                    } else {
                        ChangeKind::Modified
                    }
                }
            };
            out.push(Change { path: path.clone(), kind });
        }
        for path in self.baseline.keys() {
            if !after.contains_key(path) {
                out.push(Change { path: path.clone(), kind: ChangeKind::Deleted });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Content equality with the original, when the original provably did
    /// not change since the copy was made.
    fn same_as_original(&self, rel: &str, now: &Meta) -> bool {
        let orig_path = self.workspace.join(rel);
        let (Some(at_copy), Ok(Some(orig_now))) = (self.originals.get(rel), Meta::of(&orig_path)) else {
            return false;
        };
        if !at_copy.untouched(&orig_now) || orig_now.size != now.size || orig_now.mode != now.mode {
            return false;
        }
        files_equal(&orig_path, &self.root.join(rel)).unwrap_or(false)
    }

    /// Keep the copy on disk (e.g. until changes are approved); returns the
    /// scratch directory, whose `ws/` is the copy. The caller removes it.
    pub fn keep(self) -> PathBuf {
        self.scratch.keep()
    }

    /// Paths among `changes` that changed in the original workspace since the
    /// copy was made (merging them would overwrite someone else's change).
    pub fn conflicts<S: AsRef<str>>(&self, changes: &[S]) -> io::Result<Vec<String>> {
        let mut out = vec![];
        for c in changes {
            if !untouched_since(&self.workspace, &self.originals, c.as_ref())? {
                out.push(c.as_ref().to_string());
            }
        }
        Ok(out)
    }
}

/// `rel` in `workspace` is as it was when the copy was made (`originals`).
fn untouched_since(workspace: &Path, originals: &BTreeMap<String, Meta>, rel: &str) -> io::Result<bool> {
    let now = Meta::of_opt(&workspace.join(safe_rel(rel)?))?;
    Ok(match (originals.get(rel), &now) {
        (None, None) => true,
        // A directory's own metadata changes with its entries; the entries
        // are compared one by one.
        (Some(a), Some(b)) if a.kind == Kind::Dir && b.kind == Kind::Dir => true,
        (Some(a), Some(b)) => a.untouched(b),
        _ => false,
    })
}

/// Isolated runs whose changes await review, kept on disk by key until
/// merged or discarded (bounded: the oldest are discarded first, e.g. runs
/// abandoned by a hard interrupt).
///
/// Layout under the root (one directory per key, named by a hash of it):
/// `<id>/manifest.json` (key, workspace, change list, the originals'
/// metadata at copy time) and `<id>/ws/` holding the changed entries of the
/// copy (a path absent there is a deletion). Once merged or discarded, a
/// small `<id>.done.json` records the outcome and the copy is removed, so a
/// merge re-dispatched after a crash reports the same outcome instead of
/// "no longer available". A merge interrupted by a crash midway is resumed:
/// paths that already hold the staged content are not re-applied, paths
/// still as they were are applied, anything else is a conflict.
///
/// Without [`Staging::set_root`] the root is a private temporary directory
/// (removed with the `Staging`): nothing survives a restart.
#[derive(Debug, Default)]
pub struct Staging {
    root: std::sync::Mutex<Option<PathBuf>>,
    /// The temporary root used until one is set.
    temp: std::sync::OnceLock<tempfile::TempDir>,
    /// Serializes staging and merging.
    lock: std::sync::Mutex<()>,
}

/// Staged runs kept at most.
pub const MAX_STAGED: usize = 16;
/// Outcome records kept at most (they are tiny; only needed until the merge
/// is journaled).
const MAX_DONE: usize = 256;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    key: String,
    workspace: PathBuf,
    /// Staging order (oldest discarded first).
    created_ns: u128,
    changes: Vec<Change>,
    /// Metadata of the changed paths in the workspace when the copy was made.
    originals: BTreeMap<String, Meta>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Done {
    key: String,
    apply: bool,
    applied: Vec<String>,
    /// Nothing was applied: why (e.g. conflicting workspace changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

const NO_LONGER_AVAILABLE: &str = "the staged changes are no longer available (discarded, or lost in a restart)";

impl Staging {
    /// A staging area kept under `root` (created if missing): staged runs
    /// survive a restart of the process.
    pub fn durable(root: impl Into<PathBuf>) -> Self {
        let s = Staging::default();
        s.set_root(root);
        s
    }

    /// Keep staged runs under `root` from now on (e.g. the framework data
    /// directory, keyed by call). Runs staged before stay where they are.
    pub fn set_root(&self, root: impl Into<PathBuf>) {
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = Some(root.into());
    }

    fn root(&self) -> io::Result<PathBuf> {
        if let Some(r) = self.root.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            std::fs::create_dir_all(&r)?;
            return Ok(r);
        }
        if let Some(t) = self.temp.get() {
            return Ok(t.path().to_path_buf());
        }
        let t = tempfile::Builder::new().prefix("agent-staged-").tempdir()?;
        Ok(self.temp.get_or_init(|| t).path().to_path_buf())
    }

    /// Keep `run` under `key` when it changed anything; returns its output.
    /// A run staged earlier under the same key is replaced.
    pub fn stage(&self, key: &str, run: IsolatedRun) -> Result<ExecOutput, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.stage_locked(key, run).map_err(|e| format!("staging the changes: {e}"))
    }

    fn stage_locked(&self, key: &str, run: IsolatedRun) -> io::Result<ExecOutput> {
        let root = self.root()?;
        let id = entry_id(key);
        let (dir, done) = (root.join(&id), root.join(format!("{id}.done.json")));
        remove_any(&dir)?;
        remove_any(&done)?;
        let out = run.output.clone();
        if run.changes.is_empty() {
            return Ok(out);
        }
        // Built aside, then renamed into place: a crash never leaves a
        // half-written entry under the key's name.
        let tmp = tempfile::Builder::new().prefix(".staging-").tempdir_in(&root)?;
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws)?;
        for c in &run.changes {
            let rel = safe_rel(&c.path)?;
            keep_entry(&run.copy.path().join(&rel), &ws.join(&rel))?;
        }
        let originals = run
            .changes
            .iter()
            .filter_map(|c| run.copy.originals.get(&c.path).map(|m| (c.path.clone(), m.clone())))
            .collect();
        let created_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let m = Manifest { key: key.to_string(), workspace: run.copy.workspace().to_path_buf(), created_ns, changes: run.changes, originals };
        write_json(&tmp.path().join("manifest.json"), &m)?;
        std::fs::rename(tmp.keep(), &dir)?;
        self.bound(&root)?;
        Ok(out)
    }

    /// Discard the oldest staged runs beyond [`MAX_STAGED`] and the oldest
    /// outcome records beyond [`MAX_DONE`].
    fn bound(&self, root: &Path) -> io::Result<()> {
        let mut staged = vec![];
        let mut done = vec![];
        for e in std::fs::read_dir(root)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if name.ends_with(".done.json") {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                done.push((t, e.path()));
            } else if let Ok(m) = read_json::<Manifest>(&e.path().join("manifest.json")) {
                staged.push((m.created_ns, e.path()));
            }
        }
        staged.sort();
        for (_, p) in staged.iter().take(staged.len().saturating_sub(MAX_STAGED)) {
            remove_any(p)?;
        }
        done.sort();
        for (_, p) in done.iter().take(done.len().saturating_sub(MAX_DONE)) {
            remove_any(p)?;
        }
        Ok(())
    }

    /// Apply (after checking for conflicting workspace changes) or discard
    /// the run staged under `key`. Idempotent: merging again (e.g. the merge
    /// re-dispatched after a crash) returns the same outcome.
    pub fn merge(&self, key: &str, apply: bool) -> Result<Vec<String>, String> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let root = self.root().map_err(|e| e.to_string())?;
        let id = entry_id(key);
        let (dir, done_path) = (root.join(&id), root.join(format!("{id}.done.json")));
        if let Ok(d) = read_json::<Done>(&done_path) {
            if d.key == key {
                return match (d.apply, apply) {
                    (true, true) => d.error.map_or(Ok(d.applied), Err),
                    (false, false) => Ok(vec![]),
                    (true, false) => Err("the staged changes were already applied".into()),
                    (false, true) => Err(NO_LONGER_AVAILABLE.into()),
                };
            }
        }
        let m = match read_json::<Manifest>(&dir.join("manifest.json")) {
            Ok(m) if m.key == key => m,
            _ => return Err(NO_LONGER_AVAILABLE.into()),
        };
        // Either way the run is gone afterwards; the outcome is recorded first.
        let r = if apply { apply_staged(&dir.join("ws"), &m) } else { Ok(vec![]) };
        let done = Done { key: key.to_string(), apply, applied: r.clone().unwrap_or_default(), error: r.clone().err() };
        write_json(&done_path, &done).map_err(|e| e.to_string())?;
        remove_any(&dir).map_err(|e| e.to_string())?;
        r
    }

    /// Staged runs awaiting review.
    pub fn len(&self) -> usize {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(root) = self.root() else { return 0 };
        std::fs::read_dir(root)
            .map(|rd| rd.filter_map(Result::ok).filter(|e| e.path().join("manifest.json").is_file()).count())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// [`Staging::stage`] off the async runtime.
pub(crate) async fn stage_blocking(staging: &std::sync::Arc<Staging>, key: &str, run: IsolatedRun) -> Result<ExecOutput, String> {
    let (staging, key) = (staging.clone(), key.to_string());
    tokio::task::spawn_blocking(move || staging.stage(&key, run)).await.map_err(|e| e.to_string())?
}

/// Apply the staged entries (`ws`, the changed paths only) of `m`. Paths
/// already holding the staged result (a merge interrupted by a crash) are
/// kept and reported applied; the others must be as they were when the copy
/// was made, else nothing more is applied.
fn apply_staged(ws: &Path, m: &Manifest) -> Result<Vec<String>, String> {
    let mut already = vec![];
    let mut pending = vec![];
    let mut conflicts = vec![];
    for c in &m.changes {
        let rel = safe_rel(&c.path).map_err(|e| e.to_string())?;
        if holds_staged(ws, &m.workspace, &rel).map_err(|e| e.to_string())? {
            already.push(c.path.clone());
        } else if untouched_since(&m.workspace, &m.originals, &c.path).map_err(|e| e.to_string())? {
            pending.push(c.path.clone());
        } else {
            conflicts.push(c.path.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(format!(
            "the workspace changed since the command ran, nothing applied; conflicting: {}",
            conflicts.join(", ")
        ));
    }
    let mut applied = already;
    applied.extend(apply_isolated_changes(ws, &m.workspace, &pending).map_err(|e| e.to_string())?);
    Ok(applied)
}

/// `workspace/rel` already equals the staged entry (`ws/rel`; absent there =
/// deleted).
fn holds_staged(ws: &Path, workspace: &Path, rel: &Path) -> io::Result<bool> {
    let staged = ws.join(rel);
    let current = workspace.join(rel);
    Ok(match (Meta::of_opt(&staged)?, Meta::of_opt(&current)?) {
        (None, None) => true,
        (Some(s), Some(c)) => match (&s.kind, &c.kind) {
            (Kind::Dir, Kind::Dir) => s.mode == c.mode,
            (Kind::Symlink(a), Kind::Symlink(b)) => a == b,
            (Kind::File, Kind::File) => s.mode == c.mode && s.size == c.size && files_equal(&staged, &current)?,
            _ => false,
        },
        _ => false,
    })
}

/// Keep one changed entry of the copy: files are moved when possible (same
/// file system), else copied; directories keep their permissions; symlinks
/// are recreated.
fn keep_entry(src: &Path, dst: &Path) -> io::Result<()> {
    let m = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // a deletion
        Err(e) => return Err(e),
    };
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if m.is_dir() {
        std::fs::create_dir_all(dst)?;
        std::fs::set_permissions(dst, m.permissions())?;
    } else if m.file_type().is_symlink() {
        symlink(&std::fs::read_link(src)?, dst)?;
    } else if m.is_file() {
        if std::fs::hard_link(src, dst).is_err() {
            copy_file(src, dst)?;
        }
        std::fs::set_permissions(dst, m.permissions())?;
    }
    Ok(())
}

/// Directory name for `key`: its SHA-256 (keys contain `/`).
fn entry_id(key: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(key.as_bytes()))[..32].to_string()
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new().prefix(".tmp-").tempfile_in(dir)?;
    serde_json::to_writer(&mut tmp, v).map_err(io::Error::other)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Result of an isolated run: the output, the typed change list and the copy
/// (dropped = discarded; pass `copy.path()` to [`apply_isolated_changes`]).
#[derive(Debug)]
pub struct IsolatedRun {
    pub output: ExecOutput,
    pub changes: Vec<Change>,
    pub copy: IsolatedCopy,
}

impl IsolatedRun {
    /// Finish an isolated run: fill `overlay_changes` (discarded on timeout).
    pub(crate) fn finish(mut output: ExecOutput, copy: IsolatedCopy) -> Result<Self, String> {
        let changes =
            if output.timed_out { vec![] } else { copy.changes().map_err(|e| format!("change list: {e}"))? };
        output.overlay_changes = changes.iter().map(|c| c.path.clone()).collect();
        Ok(IsolatedRun { output, changes, copy })
    }
}

fn rel_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn copy_tree(from: &Path, to: &Path, rel: &str, originals: &mut BTreeMap<String, Meta>) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    let mut entries: Vec<_> = std::fs::read_dir(from)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let r = rel_join(rel, &name);
        let src = e.path();
        let dst = to.join(e.file_name());
        let Some(meta) = Meta::of(&src)? else {
            continue;
        };
        match &meta.kind {
            Kind::Dir => {
                if r == ".git/objects" {
                    std::fs::create_dir_all(&dst)?;
                } else {
                    copy_tree(&src, &dst, &r, originals)?;
                }
            }
            Kind::File => copy_file(&src, &dst)?,
            Kind::Symlink(target) => symlink(target, &dst)?,
        }
        originals.insert(r, meta);
    }
    // Permissions last so read-only directories can still be filled.
    let perm = std::fs::metadata(from)?.permissions();
    std::fs::set_permissions(to, perm)?;
    Ok(())
}

/// Reflink when the filesystem supports it, else a real copy (never a
/// hardlink: in-place writes would reach the original).
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let s = std::fs::File::open(src)?;
        let d = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        // SAFETY: both fds are valid for the duration of the call.
        let r = unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE, s.as_raw_fd()) };
        if r == 0 {
            std::fs::set_permissions(dst, s.metadata()?.permissions())?;
            return Ok(());
        }
        drop(d);
        std::fs::remove_file(dst)?;
    }
    std::fs::copy(src, dst).map(|_| ())
}

fn symlink(target: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, dst);
        Ok(())
    }
}

fn setup_git_objects(orig: &Path, copy: &Path, alias: GitObjectsAlias<'_>) -> io::Result<()> {
    std::fs::create_dir_all(copy.join("info"))?;
    std::fs::create_dir_all(copy.join("pack"))?;
    let mut lines = vec![alias.unwrap_or(orig).to_string_lossy().into_owned()];
    // Chained alternates of the original (relative ones resolve against it).
    if let Ok(s) = std::fs::read_to_string(orig.join("info/alternates")) {
        for l in s.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let p = Path::new(l);
            let abs = if p.is_absolute() { p.to_path_buf() } else { orig.join(p) };
            lines.push(abs.to_string_lossy().into_owned());
        }
    }
    std::fs::write(copy.join("info/alternates"), lines.join("\n") + "\n")
}

fn snapshot(root: &Path, dir: &Path, out: &mut BTreeMap<String, Meta>) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let Ok(rel) = p.strip_prefix(root) else {
            continue;
        };
        let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        let Some(m) = Meta::of(&p)? else { continue };
        let is_dir = m.kind == Kind::Dir;
        out.insert(rel, m);
        if is_dir {
            snapshot(root, &p, out)?;
        }
    }
    Ok(())
}

fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
    fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }
}

/// Validate a workspace-relative path: no absolute paths, `..` or `.`.
fn safe_rel(rel: &str) -> io::Result<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unsafe change path: {rel:?}")));
    }
    Ok(p.to_path_buf())
}

/// The parent of `ws/rel` must resolve inside the workspace (no escaping
/// through symlinked directories in the workspace).
fn check_parent(ws: &Path, rel: &Path) -> io::Result<()> {
    let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let mut cur = ws.to_path_buf();
    for c in parent.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing to write through symlinked directory {}", cur.display()),
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn remove_any(p: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Merge approved changes from an isolated copy (`copy_dir`, the copy root,
/// i.e. [`IsolatedCopy::path`]) into `workspace`. Paths present in the copy
/// are written (atomically for files, permissions preserved); paths absent
/// from it are deleted. Returns the paths applied, in order.
pub fn apply_isolated_changes<S: AsRef<str>>(
    copy_dir: &Path,
    workspace: &Path,
    changes: &[S],
) -> io::Result<Vec<String>> {
    let mut rels: Vec<(String, PathBuf)> =
        changes.iter().map(|c| safe_rel(c.as_ref()).map(|p| (c.as_ref().to_string(), p))).collect::<Result<_, _>>()?;
    rels.sort_by(|a, b| a.1.cmp(&b.1));
    rels.dedup_by(|a, b| a.1 == b.1);
    let (writes, deletes): (Vec<_>, Vec<_>) =
        rels.into_iter().partition(|(_, p)| std::fs::symlink_metadata(copy_dir.join(p)).is_ok());
    let mut applied = vec![];
    // Deletions deepest first, so emptied directories can go too.
    for (s, p) in deletes.iter().rev() {
        check_parent(workspace, p)?;
        remove_any(&workspace.join(p))?;
        applied.push(s.clone());
    }
    // Writes parents first.
    for (s, p) in &writes {
        check_parent(workspace, p)?;
        let src = copy_dir.join(p);
        let dst = workspace.join(p);
        let m = std::fs::symlink_metadata(&src)?;
        let existing = std::fs::symlink_metadata(&dst).ok();
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if m.is_dir() {
            if existing.as_ref().is_some_and(|e| !e.is_dir()) {
                remove_any(&dst)?;
            }
            std::fs::create_dir_all(&dst)?;
            std::fs::set_permissions(&dst, m.permissions())?;
        } else if m.file_type().is_symlink() {
            remove_any(&dst)?;
            symlink(&std::fs::read_link(&src)?, &dst)?;
        } else {
            let parent = dst.parent().unwrap_or(workspace);
            let tmp = tempfile::Builder::new().prefix(".agent-apply-").tempfile_in(parent)?;
            std::fs::copy(&src, tmp.path())?;
            if existing.as_ref().is_some_and(|e| e.is_dir()) {
                std::fs::remove_dir_all(&dst)?;
            }
            tmp.persist(&dst).map_err(|e| e.error)?;
            std::fs::set_permissions(&dst, m.permissions())?;
        }
        applied.push(s.clone());
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn copy_diff_apply() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        w(&root.join("keep"), "k");
        w(&root.join("mod"), "1");
        w(&root.join("same"), "s");
        w(&root.join("gone/x"), "x");
        w(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        w(&root.join(".git/objects/ab/cdef"), "obj");
        let c = IsolatedCopy::create(root, None).unwrap();
        let cp = c.path().to_path_buf();
        assert!(cp.join("keep").exists());
        assert!(!cp.join(".git/objects/ab").exists(), "objects are shared via alternates");
        let alt = std::fs::read_to_string(cp.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(alt.trim(), std::fs::canonicalize(root).unwrap().join(".git/objects").to_string_lossy());
        assert!(c.changes().unwrap().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(20));
        w(&cp.join("mod"), "2");
        w(&cp.join("same"), "s"); // rewritten, identical content
        std::fs::remove_dir_all(cp.join("gone")).unwrap();
        w(&cp.join("new/deep/f"), "n");
        w(&cp.join(".git/objects/12/3456"), "newobj");
        let ch = c.changes().unwrap();
        let got: Vec<(&str, ChangeKind)> = ch.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        use ChangeKind::*;
        assert_eq!(
            got,
            vec![
                (".git/objects/12", Added),
                (".git/objects/12/3456", Added),
                ("gone", Deleted),
                ("gone/x", Deleted),
                ("mod", Modified),
                ("new", Added),
                ("new/deep", Added),
                ("new/deep/f", Added),
            ]
        );
        // The original is untouched until applied.
        assert_eq!(std::fs::read_to_string(root.join("mod")).unwrap(), "1");
        let applied = apply_isolated_changes(&cp, root, &ch).unwrap();
        assert_eq!(applied.len(), ch.len());
        assert_eq!(std::fs::read_to_string(root.join("mod")).unwrap(), "2");
        assert_eq!(std::fs::read_to_string(root.join("new/deep/f")).unwrap(), "n");
        assert_eq!(std::fs::read_to_string(root.join(".git/objects/12/3456")).unwrap(), "newobj");
        assert!(root.join(".git/objects/ab/cdef").exists());
        assert!(!root.join("gone").exists());
        assert!(apply_isolated_changes(&cp, root, &["../x".to_string()]).is_err());
        assert!(apply_isolated_changes(&cp, root, &["/etc/passwd".to_string()]).is_err());
    }

    fn run_in_copy(root: &Path, f: impl FnOnce(&Path)) -> IsolatedRun {
        let copy = IsolatedCopy::create(root, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        f(copy.path());
        IsolatedRun::finish(ExecOutput { status: Some(0), ..Default::default() }, copy).unwrap()
    }

    #[test]
    fn staging_merges_discards_and_detects_conflicts() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        w(&root.join("a"), "1");
        let staging = Staging::default();
        // Nothing changed: nothing staged.
        staging.stage("s/none", run_in_copy(root, |_| {})).unwrap();
        assert!(staging.is_empty());
        assert!(staging.merge("s/none", true).unwrap_err().contains("no longer available"));
        // Staged, then merged.
        let out = staging.stage("s/c1", run_in_copy(root, |c| w(&c.join("a"), "2"))).unwrap();
        assert_eq!(out.overlay_changes, vec!["a"]);
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "1", "staged only");
        assert_eq!(staging.merge("s/c1", true).unwrap(), vec!["a"]);
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "2");
        assert!(staging.is_empty());
        // Merging again (re-dispatched after a crash) reports the same outcome.
        assert_eq!(staging.merge("s/c1", true).unwrap(), vec!["a"]);
        assert!(staging.merge("s/c1", false).is_err(), "already applied");
        // Discarded.
        staging.stage("s/c2", run_in_copy(root, |c| w(&c.join("b"), "new"))).unwrap();
        assert_eq!(staging.merge("s/c2", false).unwrap(), Vec::<String>::new());
        assert!(!root.join("b").exists());
        // The original changed meanwhile: nothing applied.
        staging.stage("s/c3", run_in_copy(root, |c| {
            w(&c.join("a"), "3");
            w(&c.join("c"), "x");
        }))
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        w(&root.join("a"), "user");
        let e = staging.merge("s/c3", true).unwrap_err();
        assert!(e.contains("conflicting: a"), "{e}");
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "user");
        assert!(!root.join("c").exists());
        assert_eq!(staging.merge("s/c3", true).unwrap_err(), e, "the same outcome again");
        // Bounded: the oldest are dropped.
        for i in 0..MAX_STAGED + 2 {
            staging.stage(&format!("s/{i}"), run_in_copy(root, |c| w(&c.join("d"), "d"))).unwrap();
        }
        assert_eq!(staging.len(), MAX_STAGED);
        assert!(staging.merge("s/0", false).is_err());
    }

    #[test]
    fn durable_staging_survives_restarts_and_merges_idempotently() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (root, dir) = (ws.path(), data.path().join("staged"));
        w(&root.join("a"), "1");
        w(&root.join("gone"), "g");
        let change = |c: &Path| {
            w(&c.join("a"), "2");
            w(&c.join("new/f"), "n");
            std::fs::remove_file(c.join("gone")).unwrap();
        };
        // Staged, then the process "restarts": a new staging area on the
        // same directory still has the run.
        Staging::durable(&dir).stage("s/c1", run_in_copy(root, change)).unwrap();
        let staging = Staging::durable(&dir);
        assert_eq!(staging.len(), 1);
        let applied = staging.merge("s/c1", true).unwrap();
        assert_eq!(applied, vec!["gone", "a", "new", "new/f"]);
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "2");
        assert!(!root.join("gone").exists());
        // Crash after applying, before the merge was journaled: the merge is
        // re-dispatched after a restart and reports the same outcome.
        assert_eq!(Staging::durable(&dir).merge("s/c1", true).unwrap(), applied);
        assert_eq!(std::fs::read_to_string(root.join("new/f")).unwrap(), "n");

        // Crash in the middle of applying: what was applied is recognized,
        // the rest is applied.
        Staging::durable(&dir).stage("s/c2", run_in_copy(root, |c| {
            w(&c.join("a"), "3");
            w(&c.join("b"), "b");
        }))
        .unwrap();
        w(&root.join("a"), "3"); // applied before the crash
        let mut applied = Staging::durable(&dir).merge("s/c2", true).unwrap();
        applied.sort();
        assert_eq!(applied, vec!["a", "b"]);
        assert_eq!(std::fs::read_to_string(root.join("b")).unwrap(), "b");
        // ...but a path changed by someone else is still a conflict.
        Staging::durable(&dir).stage("s/c3", run_in_copy(root, |c| w(&c.join("a"), "4"))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        w(&root.join("a"), "user");
        assert!(Staging::durable(&dir).merge("s/c3", true).unwrap_err().contains("conflicting: a"));
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "user");
        // Discarding is idempotent too; the copies are gone afterwards.
        Staging::durable(&dir).stage("s/c4", run_in_copy(root, |c| w(&c.join("z"), "z"))).unwrap();
        assert_eq!(Staging::durable(&dir).merge("s/c4", false).unwrap(), Vec::<String>::new());
        assert_eq!(Staging::durable(&dir).merge("s/c4", false).unwrap(), Vec::<String>::new());
        assert!(!root.join("z").exists());
        assert!(Staging::durable(&dir).is_empty());
        // Unknown keys: nothing to merge.
        assert!(Staging::durable(&dir).merge("s/other", true).unwrap_err().contains("no longer available"));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_parent() {
        let ws = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(out.path(), ws.path().join("link")).unwrap();
        w(&copy.path().join("link/f"), "x");
        assert!(apply_isolated_changes(copy.path(), ws.path(), &["link/f"]).is_err());
        assert!(!out.path().join("f").exists());
    }
}
