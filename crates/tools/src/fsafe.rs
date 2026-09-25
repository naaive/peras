//! Path resolution and workspace-confined file opening.
//!
//! Paths inside the workspace are opened relative to a directory fd of the
//! workspace root, so symlinks cannot take the access out of it:
//! - Linux: `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`; symlinks that stay
//!   inside the workspace are followed.
//! - fallback (no `openat2`, other unixes): component-wise `openat` with
//!   `O_NOFOLLOW` (symlinks are refused altogether).
//! - non-unix: canonicalize + `starts_with` check.
//!
//! Paths outside the workspace (only reachable when explicitly granted) are opened
//! from `/` without following any symlink.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Lexically normalise `path` (remove `.`, resolve `..`); keeps it absolute.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Resolve a model-supplied path against the workspace: relative paths are joined
/// to the workspace root; the result is absolute and normalised.
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("empty path".into());
    }
    if raw.starts_with('~') {
        return Err(format!(
            "`{raw}`: home-relative paths are not supported; use an absolute path"
        ));
    }
    if raw.contains('\0') {
        return Err("path contains NUL".into());
    }
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    Ok(normalize(&joined))
}

/// `(root, relative)` for opening `abs`: the workspace when `abs` is inside it,
/// otherwise `/` with no symlinks allowed at all.
fn split(workspace: &Path, abs: &Path) -> (PathBuf, PathBuf, bool) {
    let ws = normalize(workspace);
    match abs.strip_prefix(&ws) {
        Ok(rel) => (ws, rel.to_path_buf(), true),
        Err(_) => (
            PathBuf::from("/"),
            abs.strip_prefix("/").unwrap_or(abs).to_path_buf(),
            false,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Dir,
    /// Create or truncate; missing parent directories are created.
    Write,
}

fn escape_err(abs: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{}: path escapes the workspace through a symlink",
            abs.display()
        ),
    )
}

/// Open `abs` safely (see the module docs).
pub fn open(workspace: &Path, abs: &Path, mode: Mode) -> io::Result<File> {
    let (root, rel, inside) = split(workspace, abs);
    imp::open(&root, &rel, mode, inside).map_err(|e| {
        if e.raw_os_error() == Some(libc_exdev()) || e.raw_os_error() == Some(libc_eloop()) {
            escape_err(abs)
        } else {
            e
        }
    })
}

#[cfg(unix)]
fn libc_exdev() -> i32 {
    libc::EXDEV
}
#[cfg(unix)]
fn libc_eloop() -> i32 {
    libc::ELOOP
}
#[cfg(not(unix))]
fn libc_exdev() -> i32 {
    -1
}
#[cfg(not(unix))]
fn libc_eloop() -> i32 {
    -1
}

pub fn read_bytes(workspace: &Path, abs: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut f = open(workspace, abs, Mode::Read)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// `None` when the file does not exist.
pub fn read_opt(workspace: &Path, abs: &Path) -> io::Result<Option<Vec<u8>>> {
    match read_bytes(workspace, abs) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn write_bytes(workspace: &Path, abs: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut f = open(workspace, abs, Mode::Write)?;
    f.write_all(bytes)?;
    f.flush()
}

#[cfg(unix)]
mod imp {
    use super::Mode;
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    fn cstr(p: &[u8]) -> io::Result<CString> {
        CString::new(p)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }

    fn open_root(root: &Path) -> io::Result<OwnedFd> {
        let c = cstr(root.as_os_str().as_bytes())?;
        // SAFETY: valid NUL-terminated path; the returned fd is owned below.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh, valid descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn openat(dir: &OwnedFd, name: &[u8], flags: i32, mode: libc::mode_t) -> io::Result<OwnedFd> {
        let c = cstr(name)?;
        // SAFETY: valid dirfd and path.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    #[cfg(target_os = "linux")]
    fn openat2(
        dir: &OwnedFd,
        rel: &[u8],
        flags: i32,
        mode: u64,
        beneath: bool,
    ) -> io::Result<OwnedFd> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_NO_SYMLINKS: u64 = 0x04;
        const RESOLVE_BENEATH: u64 = 0x08;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode,
            resolve: if beneath {
                RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS
            } else {
                RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS
            },
        };
        let c = cstr(if rel.is_empty() { b"." } else { rel })?;
        // SAFETY: arguments follow the openat2(2) ABI.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir.as_raw_fd(),
                c.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    fn components(rel: &Path) -> io::Result<Vec<&[u8]>> {
        let mut out = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => out.push(n.as_bytes()),
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "non-normal path component",
                    ))
                }
            }
        }
        Ok(out)
    }

    /// Component-wise walk with `O_NOFOLLOW` on every component.
    fn walk_nofollow(
        root: OwnedFd,
        comps: &[&[u8]],
        flags: i32,
        mode: libc::mode_t,
        create_dirs: bool,
    ) -> io::Result<OwnedFd> {
        let mut cur = root;
        if comps.is_empty() {
            return openat(&cur, b".", flags, mode);
        }
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            if last {
                return openat(&cur, comp, flags | libc::O_NOFOLLOW, mode);
            }
            let dflags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
            cur = match openat(&cur, comp, dflags, 0) {
                Ok(fd) => fd,
                Err(e) if create_dirs && e.kind() == io::ErrorKind::NotFound => {
                    let c = cstr(comp)?;
                    // SAFETY: valid dirfd and path.
                    let r = unsafe { libc::mkdirat(cur.as_raw_fd(), c.as_ptr(), 0o755) };
                    if r < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() != io::ErrorKind::AlreadyExists {
                            return Err(err);
                        }
                    }
                    openat(&cur, comp, dflags, 0)?
                }
                Err(e) => return Err(e),
            };
        }
        unreachable!()
    }

    fn flags_for(mode: Mode) -> (i32, libc::mode_t) {
        match mode {
            Mode::Read => (libc::O_RDONLY, 0),
            Mode::Dir => (libc::O_RDONLY | libc::O_DIRECTORY, 0),
            Mode::Write => (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644),
        }
    }

    pub fn open(root: &Path, rel: &Path, mode: Mode, inside: bool) -> io::Result<File> {
        let comps = components(rel)?;
        let (flags, fmode) = flags_for(mode);
        let root_fd = open_root(root)?;

        #[cfg(target_os = "linux")]
        {
            let rel_bytes = rel.as_os_str().as_bytes();
            let attempt = if mode == Mode::Write {
                // Open (creating if needed) the parent beneath the root, then the
                // file itself without following a final symlink.
                let parent = rel
                    .parent()
                    .map(|p| p.as_os_str().as_bytes())
                    .unwrap_or(b"");
                let dflags = libc::O_RDONLY | libc::O_DIRECTORY;
                let pfd = match openat2(&root_fd, parent, dflags, 0, inside) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        let pcomps = components(Path::new(std::ffi::OsStr::from_bytes(parent)))?;
                        let fresh = open_root(root)?;
                        // Create missing parents without following symlinks, then
                        // re-open the parent beneath the root.
                        let mut with_dot: Vec<&[u8]> = pcomps.clone();
                        with_dot.push(b".");
                        drop(walk_nofollow(fresh, &with_dot, dflags, 0, true)?);
                        openat2(&root_fd, parent, dflags, 0, inside)
                    }
                    other => other,
                };
                match pfd {
                    Ok(pfd) => {
                        let name = comps.last().copied().unwrap_or(b".");
                        openat(&pfd, name, flags | libc::O_NOFOLLOW, fmode)
                    }
                    Err(e) => Err(e),
                }
            } else {
                openat2(&root_fd, rel_bytes, flags, fmode as u64, inside)
            };
            match attempt {
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::E2BIG)
                    ) => {}
                other => return other.map(File::from),
            }
        }
        let _ = inside;
        walk_nofollow(root_fd, &comps, flags, fmode, mode == Mode::Write).map(File::from)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::Mode;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub fn open(root: &Path, rel: &Path, mode: Mode, _inside: bool) -> io::Result<File> {
        let root_c = root.canonicalize()?;
        let target = root.join(rel);
        if mode == Mode::Write {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let check = if mode == Mode::Write {
            target
                .parent()
                .map(|p| p.canonicalize())
                .transpose()?
                .unwrap_or(root_c.clone())
        } else {
            target.canonicalize()?
        };
        if !check.starts_with(&root_c) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path escapes the workspace",
            ));
        }
        match mode {
            Mode::Read | Mode::Dir => File::open(&target),
            Mode::Write => OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(
            normalize(Path::new("/w/a/../b/./c")),
            PathBuf::from("/w/b/c")
        );
        assert_eq!(normalize(Path::new("/w/../../..")), PathBuf::from("/"));
        assert_eq!(
            resolve(Path::new("/w"), "../etc/passwd").unwrap(),
            PathBuf::from("/etc/passwd")
        );
        assert_eq!(
            resolve(Path::new("/w"), "src/x.rs").unwrap(),
            PathBuf::from("/w/src/x.rs")
        );
        assert!(resolve(Path::new("/w"), "~/x").is_err());
    }
}
