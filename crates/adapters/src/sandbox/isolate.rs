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

use agent_runtime::ExecOutput;
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Kind of change in an isolated run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One change, with a workspace-relative path (`/`-separated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

impl AsRef<str> for Change {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
