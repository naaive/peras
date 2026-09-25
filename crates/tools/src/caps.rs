//! Capability handle types.
//!
//! A capability parameter is, at once:
//! - for the model, a string in the JSON Schema ([`Capability::schema`]);
//! - for the framework, an access declaration ([`Capability::access`]);
//! - for the tool body, a handle that only reaches that resource, bound to the
//!   call's grants before the body runs ([`Capability::bind`]).
//!
//! | type | JSON arg | declaration | class |
//! |---|---|---|---|
//! | `Read<File>` | path | `fs:///abs` read | Pure |
//! | `Write<File>` | path | `fs:///abs` write | LocalWrite |
//! | `Read<Dir>` | path | `fs:///abs/**` read | Pure |
//! | `Get<Url>` | url | `net:host:port` read | Pure |
//! | `Net<Url>` | url | `net:host:port` write | Network |
//! | `Exec<Cmd>` | command line | `cmd:<line>` write | Opaque |
//! | `Secret<Name>` | name | `secret:NAME` read | Pure |
//! | `Mem<Key>` | `scope/key` | `mem:scope/key` write | LocalWrite |

use crate::fsafe;
use agent_proto::{Access, AccessMode, EffectClass, ResourceUri, Trust};
use agent_runtime::{AccessCtx, ExecOutput, SandboxSpec, ToolCtx, ToolError, ToolOutput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Implemented by every capability parameter type.
pub trait Capability: DeserializeOwned + Send + Sized + 'static {
    /// Side-effect class contributed by this parameter.
    const CLASS: EffectClass;
    /// JSON Schema fragment for the parameter.
    fn schema() -> Value;
    /// Access declaration, resolved against the workspace.
    fn access(&self, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError>;
    /// Bind to a running call: checks that the grants cover the declaration.
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError>;
}

impl<C: Capability> Capability for Option<C> {
    const CLASS: EffectClass = C::CLASS;
    fn schema() -> Value {
        C::schema()
    }
    fn access(&self, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        match self {
            Some(c) => c.access(ctx),
            None => Ok(vec![]),
        }
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        match self {
            Some(c) => c.bind(ctx, obs),
            None => Ok(()),
        }
    }
}

/// Content hashes observed during a call (merged into `ToolOutput::observed`).
#[derive(Clone, Default, Debug)]
pub struct Observations(Arc<Mutex<Vec<Access>>>);

impl Observations {
    pub fn push(&self, a: Access) {
        let mut v = self.0.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|x| x.resource != a.resource);
        v.push(a);
    }
    pub fn take(&self) -> Vec<Access> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Does a granted access cover the needed one? Same resource (or a glob over it)
/// and a mode at least as strong. When both carry a content hash they must match
/// (definition-bound commands).
pub fn covers(grant: &Access, needed: &Access) -> bool {
    if grant.mode == AccessMode::Read && needed.mode == AccessMode::Write {
        return false;
    }
    if let (Some(g), Some(n)) = (&grant.content_hash, &needed.content_hash) {
        if g != n {
            return false;
        }
    }
    if grant.resource == needed.resource {
        return true;
    }
    let g = grant.resource.as_str();
    if !g.contains(['*', '?', '[']) {
        return false;
    }
    if grant.resource.scheme() != needed.resource.scheme() {
        return false;
    }
    let fs = g.starts_with("fs://");
    match globset::GlobBuilder::new(g).literal_separator(fs).build() {
        Ok(glob) => glob.compile_matcher().is_match(needed.resource.as_str()),
        Err(_) => false,
    }
}

pub fn check_granted(ctx: &ToolCtx, needed: &Access) -> Result<(), ToolError> {
    if ctx.grants.iter().any(|g| covers(g, needed)) {
        Ok(())
    } else {
        let mode = match needed.mode {
            AccessMode::Read => "read",
            AccessMode::Write => "write",
        };
        Err(ToolError::NotGranted(format!("{mode} {}", needed.resource)))
    }
}

fn access_ctx(ctx: &ToolCtx) -> AccessCtx {
    AccessCtx {
        workspace: ctx.workspace.clone(),
    }
}

fn fs_uri(p: &Path) -> ResourceUri {
    ResourceUri::fs(&p.to_string_lossy())
}

fn not_bound() -> ToolError {
    ToolError::Infra("capability handle used before being bound to a call".into())
}

fn io_err(path: &Path, e: std::io::Error) -> ToolError {
    ToolError::Failed(format!("{}: {e}", path.display()))
}

// ------------------------------------------------------------------ kinds

/// Resource kind markers.
#[derive(Debug, Clone, Copy)]
pub struct File;
#[derive(Debug, Clone, Copy)]
pub struct Dir;
#[derive(Debug, Clone, Copy)]
pub struct Url;
#[derive(Debug, Clone, Copy)]
pub struct Cmd;
#[derive(Debug, Clone, Copy)]
pub struct Name;
#[derive(Debug, Clone, Copy)]
pub struct Key;

#[derive(Clone)]
struct Bound {
    ctx: ToolCtx,
    obs: Observations,
}

macro_rules! handle_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        pub struct $name<K> {
            raw: String,
            bound: Option<Bound>,
            _k: PhantomData<fn() -> K>,
        }

        impl<K> $name<K> {
            /// An unbound handle for `raw` (a path, url, command, name or key).
            pub fn new(raw: impl Into<String>) -> Self {
                $name { raw: raw.into(), bound: None, _k: PhantomData }
            }
            /// The argument as given by the model.
            pub fn raw(&self) -> &str {
                &self.raw
            }
            fn bound(&self) -> Result<&Bound, ToolError> {
                self.bound.as_ref().ok_or_else(not_bound)
            }
        }

        impl<K> Clone for $name<K> {
            fn clone(&self) -> Self {
                $name { raw: self.raw.clone(), bound: self.bound.clone(), _k: PhantomData }
            }
        }

        impl<K> fmt::Debug for $name<K> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.raw).finish()
            }
        }

        impl<'de, K> Deserialize<'de> for $name<K> {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                if s.is_empty() {
                    return Err(serde::de::Error::custom("empty value"));
                }
                Ok($name::new(s))
            }
        }
    };
}

handle_type!(
    /// Read access to a file (`Read<File>`) or a directory tree (`Read<Dir>`).
    Read
);
handle_type!(
    /// Write access to a file (`Write<File>`); also allows reading it.
    Write
);
handle_type!(
    /// HTTP GET of a URL (`Get<Url>`): a read of `net:host:port`.
    Get
);
handle_type!(
    /// Arbitrary requests to a URL's origin (`Net<Url>`): a write of `net:host:port`.
    Net
);
handle_type!(
    /// Execution of a command line (`Exec<Cmd>`).
    Exec
);
handle_type!(
    /// A secret injected as a handle (`Secret<Name>`); never enters the context.
    Secret
);
handle_type!(
    /// A long-term memory entry (`Mem<Key>`), key `scope/key`.
    Mem
);

fn path_schema(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

// ------------------------------------------------------------------ files

fn resolve_path(workspace: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    fsafe::resolve(workspace, raw).map_err(ToolError::InvalidInput)
}

/// Shared file operations for `Read<File>` / `Write<File>`.
async fn read_file(b: &Bound, abs: &Path) -> Result<Vec<u8>, ToolError> {
    let bytes = read_file_unrecorded(b, abs).await?;
    b.obs.push(Access {
        resource: fs_uri(abs),
        mode: AccessMode::Read,
        content_hash: Some(sha256_hex(&bytes)),
    });
    Ok(bytes)
}

async fn read_file_unrecorded(b: &Bound, abs: &Path) -> Result<Vec<u8>, ToolError> {
    let ws = b.ctx.workspace.clone();
    let p = abs.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || fsafe::read_bytes(&ws, &p))
        .await
        .map_err(|e| ToolError::Infra(e.to_string()))?
        .map_err(|e| io_err(abs, e))?;
    Ok(bytes)
}

fn utf8(abs: &Path, bytes: Vec<u8>) -> Result<String, ToolError> {
    String::from_utf8(bytes)
        .map_err(|_| ToolError::Failed(format!("{}: not valid UTF-8", abs.display())))
}

impl Read<File> {
    /// Absolute path (resolved against the workspace).
    pub fn path(&self) -> Result<PathBuf, ToolError> {
        let b = self.bound()?;
        resolve_path(&b.ctx.workspace, &self.raw)
    }
    pub async fn bytes(&self) -> Result<Vec<u8>, ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        read_file(b, &abs).await
    }
    pub async fn text(&self) -> Result<String, ToolError> {
        let abs = self.path()?;
        utf8(&abs, self.bytes().await?)
    }
}

impl Capability for Read<File> {
    const CLASS: EffectClass = EffectClass::Pure;
    fn schema() -> Value {
        path_schema("Path of the file to read (relative to the workspace root, or absolute)")
    }
    fn access(&self, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::read(fs_uri(&resolve_path(
            &ctx.workspace,
            &self.raw,
        )?))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

impl Write<File> {
    pub fn path(&self) -> Result<PathBuf, ToolError> {
        let b = self.bound()?;
        resolve_path(&b.ctx.workspace, &self.raw)
    }
    pub async fn exists(&self) -> Result<bool, ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        let ws = b.ctx.workspace.clone();
        let p = abs.clone();
        let r = tokio::task::spawn_blocking(move || fsafe::read_opt(&ws, &p))
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?
            .map_err(|e| io_err(&abs, e))?;
        Ok(r.is_some())
    }
    pub async fn bytes(&self) -> Result<Vec<u8>, ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        read_file(b, &abs).await
    }
    pub async fn text(&self) -> Result<String, ToolError> {
        let abs = self.path()?;
        utf8(&abs, self.bytes().await?)
    }

    /// Stale check: if the grants carry a content hash for this file, the file's
    /// current content must still have it.
    fn check_stale(b: &Bound, abs: &Path, current: Option<&[u8]>) -> Result<(), ToolError> {
        let uri = fs_uri(abs);
        let expected = b
            .ctx
            .grants
            .iter()
            .filter(|g| g.resource == uri)
            .find_map(|g| g.content_hash.clone());
        if let Some(expected) = expected {
            let now = current.map(sha256_hex);
            if now.as_deref() != Some(expected.as_str()) {
                return Err(ToolError::Stale(abs.display().to_string()));
            }
        }
        Ok(())
    }

    async fn current(b: &Bound, abs: &Path) -> Result<Option<Vec<u8>>, ToolError> {
        let ws = b.ctx.workspace.clone();
        let p = abs.to_path_buf();
        tokio::task::spawn_blocking(move || fsafe::read_opt(&ws, &p))
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?
            .map_err(|e| io_err(abs, e))
    }

    async fn store(b: &Bound, abs: &Path, bytes: Vec<u8>) -> Result<(), ToolError> {
        let ws = b.ctx.workspace.clone();
        let p = abs.to_path_buf();
        let hash = sha256_hex(&bytes);
        tokio::task::spawn_blocking(move || fsafe::write_bytes(&ws, &p, &bytes))
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?
            .map_err(|e| io_err(abs, e))?;
        // The model now knows the new content: record it so later edits based on
        // it are not considered stale.
        b.obs.push(Access {
            resource: fs_uri(abs),
            mode: AccessMode::Read,
            content_hash: Some(hash),
        });
        Ok(())
    }

    pub async fn write_bytes(&self, bytes: impl Into<Vec<u8>>) -> Result<(), ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        let cur = Self::current(b, &abs).await?;
        Self::check_stale(b, &abs, cur.as_deref())?;
        Self::store(b, &abs, bytes.into()).await
    }

    pub async fn write_text(&self, text: &str) -> Result<(), ToolError> {
        self.write_bytes(text.as_bytes().to_vec()).await
    }

    /// Replace the unique occurrence of `old` with `new`; fails when `old` occurs
    /// zero or several times.
    pub async fn replace_once(&self, old: &str, new: &str) -> Result<(), ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        if old.is_empty() {
            return Err(ToolError::Failed("`old` must not be empty".into()));
        }
        let cur = Self::current(b, &abs).await?;
        Self::check_stale(b, &abs, cur.as_deref())?;
        let bytes = cur
            .ok_or_else(|| ToolError::Failed(format!("{}: file does not exist", abs.display())))?;
        let text = utf8(&abs, bytes)?;
        let n = text.matches(old).count();
        match n {
            0 => Err(ToolError::Failed(format!(
                "`old` not found in {}",
                abs.display()
            ))),
            1 => Self::store(b, &abs, text.replacen(old, new, 1).into_bytes()).await,
            n => Err(ToolError::Failed(format!(
                "`old` occurs {n} times in {}; include more context to make it unique",
                abs.display()
            ))),
        }
    }
}

impl Capability for Write<File> {
    const CLASS: EffectClass = EffectClass::LocalWrite;
    fn schema() -> Value {
        path_schema("Path of the file to write (relative to the workspace root, or absolute)")
    }
    fn access(&self, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::write(fs_uri(&resolve_path(
            &ctx.workspace,
            &self.raw,
        )?))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ dirs

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

impl Read<Dir> {
    pub fn path(&self) -> Result<PathBuf, ToolError> {
        let b = self.bound()?;
        resolve_path(&b.ctx.workspace, &self.raw)
    }

    async fn checked_dir(&self) -> Result<(PathBuf, PathBuf), ToolError> {
        let b = self.bound()?;
        let abs = self.path()?;
        let ws = b.ctx.workspace.clone();
        let p = abs.clone();
        tokio::task::spawn_blocking(move || fsafe::open(&ws, &p, fsafe::Mode::Dir).map(drop))
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?
            .map_err(|e| io_err(&abs, e))?;
        Ok((b.ctx.workspace.clone(), abs))
    }

    /// Entries of the directory (not recursive), sorted by name.
    pub async fn list(&self) -> Result<Vec<DirEntry>, ToolError> {
        let (_, abs) = self.checked_dir().await?;
        let p = abs.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<DirEntry>> {
            let mut out = Vec::new();
            for e in std::fs::read_dir(&p)? {
                let e = e?;
                let ft = e.file_type()?;
                out.push(DirEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: ft.is_dir(),
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::Infra(e.to_string()))?
        .map_err(|e| io_err(&abs, e))
    }

    /// Files under the directory (respecting `.gitignore`, not following
    /// symlinks), as paths relative to the directory, sorted. With `pattern`,
    /// only those matching the glob (matched against the relative path).
    pub async fn walk(
        &self,
        pattern: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PathBuf>, ToolError> {
        let (_, abs) = self.checked_dir().await?;
        let matcher = match pattern {
            Some(p) => Some(
                globset::GlobBuilder::new(p)
                    .literal_separator(true)
                    .build()
                    .map_err(|e| ToolError::Failed(format!("invalid glob: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let root = abs.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(false)
                .follow_links(false)
                .require_git(false)
                .build();
            for entry in walker.flatten() {
                let Some(ft) = entry.file_type() else {
                    continue;
                };
                if !ft.is_file() {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(&root) else {
                    continue;
                };
                if rel.components().any(|c| c.as_os_str() == ".git") {
                    continue;
                }
                let ok = match &matcher {
                    Some(m) => m.is_match(rel),
                    None => true,
                };
                if ok {
                    out.push(rel.to_path_buf());
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            out.sort();
            out
        })
        .await
        .map_err(|e| ToolError::Infra(e.to_string()))
    }

    /// Read a file below the directory (path relative to the directory). Not
    /// recorded in `observed` (directory scans would flood it).
    pub async fn read_bytes(&self, rel: &Path) -> Result<Vec<u8>, ToolError> {
        let b = self.bound()?;
        let dir = self.path()?;
        let abs = fsafe::normalize(&dir.join(rel));
        if !abs.starts_with(&dir) {
            return Err(ToolError::NotGranted(format!("read {}", fs_uri(&abs))));
        }
        read_file_unrecorded(b, &abs).await
    }
}

fn dir_glob(abs: &Path) -> ResourceUri {
    let s = abs.to_string_lossy();
    if s == "/" {
        ResourceUri::fs("/**")
    } else {
        ResourceUri::fs(&format!("{s}/**"))
    }
}

impl Capability for Read<Dir> {
    const CLASS: EffectClass = EffectClass::Pure;
    fn schema() -> Value {
        path_schema("Directory (relative to the workspace root, or absolute); use \".\" for the workspace root")
    }
    fn access(&self, ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::read(dir_glob(&resolve_path(
            &ctx.workspace,
            &self.raw,
        )?))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ urls

/// Parse an http(s) URL and return it with its `host` and port.
pub fn parse_url(raw: &str) -> Result<(url::Url, String, u16), ToolError> {
    let u = url::Url::parse(raw)
        .map_err(|e| ToolError::InvalidInput(format!("invalid url `{raw}`: {e}")))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err(ToolError::InvalidInput(format!(
            "unsupported url scheme `{}`",
            u.scheme()
        )));
    }
    let host = u
        .host_str()
        .ok_or_else(|| ToolError::InvalidInput(format!("url `{raw}` has no host")))?
        .to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    Ok((u, host, port))
}

fn url_access(raw: &str, mode: AccessMode) -> Result<Vec<Access>, ToolError> {
    let (_, host, port) = parse_url(raw)?;
    Ok(vec![Access {
        resource: ResourceUri::net(&host, port),
        mode,
        content_hash: None,
    }])
}

fn http_client(host: String, port: u16) -> Result<reqwest::Client, ToolError> {
    // Redirects may only stay on the granted origin.
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        let same = attempt.url().host_str() == Some(host.as_str())
            && attempt.url().port_or_known_default() == Some(port);
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if same {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("agent-tools/0.1")
        .build()
        .map_err(|e| ToolError::Infra(e.to_string()))
}

const MAX_BODY: usize = 5 * 1024 * 1024;

async fn read_body(resp: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    use futures::StreamExt;
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ToolError::Failed(e.to_string()))?;
        out.extend_from_slice(&chunk);
        if out.len() > MAX_BODY {
            out.truncate(MAX_BODY);
            break;
        }
    }
    Ok(out)
}

/// A fetched HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl<K> Get<K> {
    /// `host` of the URL (used as the untrusted source label).
    pub fn host(&self) -> Result<String, ToolError> {
        Ok(parse_url(&self.raw)?.1)
    }
}

impl Get<Url> {
    pub fn url(&self) -> Result<url::Url, ToolError> {
        Ok(parse_url(&self.raw)?.0)
    }
    /// Perform the GET.
    pub async fn fetch(&self) -> Result<HttpResponse, ToolError> {
        self.bound()?;
        let (u, host, port) = parse_url(&self.raw)?;
        let resp = http_client(host, port)?
            .get(u)
            .send()
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_body(resp).await?;
        Ok(HttpResponse {
            status,
            content_type,
            body,
        })
    }
    pub async fn text(&self) -> Result<String, ToolError> {
        Ok(self.fetch().await?.text())
    }
    /// A text output labelled `Untrusted { source: host }`.
    pub async fn untrusted_output(&self) -> Result<ToolOutput, ToolError> {
        const MAX_CHARS: usize = 100_000;
        let r = self.fetch().await?;
        let mut text = r.text();
        if text.len() > MAX_CHARS {
            let mut end = MAX_CHARS;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n... [truncated]");
        }
        let mut out = ToolOutput::text(format!("HTTP {}\n\n{text}", r.status));
        out.trust = Some(Trust::Untrusted {
            source: self.host()?,
        });
        Ok(out)
    }
}

impl Capability for Get<Url> {
    const CLASS: EffectClass = EffectClass::Pure;
    fn schema() -> Value {
        json!({ "type": "string", "format": "uri", "description": "http(s) URL to fetch" })
    }
    fn access(&self, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        url_access(&self.raw, AccessMode::Read)
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

impl Net<Url> {
    pub fn url(&self) -> Result<url::Url, ToolError> {
        Ok(parse_url(&self.raw)?.0)
    }
    /// Send a request to the URL (method e.g. `"POST"`).
    pub async fn send(
        &self,
        method: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, ToolError> {
        self.bound()?;
        let (u, host, port) = parse_url(&self.raw)?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| ToolError::InvalidInput(format!("invalid method `{method}`")))?;
        let mut req = http_client(host, port)?.request(method, u);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_body(resp).await?;
        Ok(HttpResponse {
            status,
            content_type,
            body,
        })
    }
}

impl Capability for Net<Url> {
    const CLASS: EffectClass = EffectClass::Network;
    fn schema() -> Value {
        json!({ "type": "string", "format": "uri", "description": "http(s) URL" })
    }
    fn access(&self, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        url_access(&self.raw, AccessMode::Write)
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ commands

/// Compile the call's grants into a sandbox profile: fs grants become readable
/// (and, for writes, writable) paths, `net:` grants reachable hosts.
pub fn compile_spec(ctx: &ToolCtx, timeout_ms: u64, allow_writes: bool) -> SandboxSpec {
    let mut spec = SandboxSpec {
        cwd: ctx.workspace.clone(),
        timeout_ms,
        ..Default::default()
    };
    for g in &ctx.grants {
        match g.resource.scheme() {
            Some(agent_proto::Scheme::Fs) => {
                let p = fs_grant_path(g.resource.rest());
                if !spec.readable.contains(&p) {
                    spec.readable.push(p.clone());
                }
                if allow_writes && g.mode == AccessMode::Write && !spec.writable.contains(&p) {
                    spec.writable.push(p);
                }
            }
            Some(agent_proto::Scheme::Net) if allow_writes || g.mode == AccessMode::Read => {
                let hp = g.resource.rest().to_string();
                if !spec.network.contains(&hp) {
                    spec.network.push(hp);
                }
            }
            _ => {}
        }
    }
    spec
}

/// `/w/src/**` -> `/w/src`; `/w/*.rs` -> `/w`; `/w/a.rs` -> `/w/a.rs`.
fn fs_grant_path(p: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(p).components() {
        if c.as_os_str().to_string_lossy().contains(['*', '?', '[']) {
            break;
        }
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

impl Exec<Cmd> {
    pub fn command_line(&self) -> &str {
        &self.raw
    }
    /// Run the command line with `bash -c` in the sandbox compiled from the grants.
    pub async fn run(&self, timeout_ms: u64) -> Result<ExecOutput, ToolError> {
        let b = self.bound()?;
        let spec = compile_spec(&b.ctx, timeout_ms, true);
        let argv = vec!["bash".to_string(), "-c".to_string(), self.raw.clone()];
        b.ctx
            .sandbox
            .run(&argv, &spec, b.ctx.cancel.child_token())
            .await
            .map_err(ToolError::Failed)
    }
}

impl Capability for Exec<Cmd> {
    const CLASS: EffectClass = EffectClass::Opaque;
    fn schema() -> Value {
        json!({ "type": "string", "description": "Command line to execute" })
    }
    fn access(&self, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        Ok(vec![Access::write(ResourceUri::cmd(self.raw.trim()))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ secrets

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl Secret<Name> {
    pub fn name(&self) -> &str {
        &self.raw
    }
    /// The secret's value. Never return it (or anything derived from it) as
    /// tool output.
    pub fn expose(&self) -> Result<String, ToolError> {
        let b = self.bound()?;
        b.ctx
            .secrets
            .get(&self.raw)
            .ok_or_else(|| ToolError::Failed(format!("secret `{}` is not set", self.raw)))
    }
}

impl Capability for Secret<Name> {
    const CLASS: EffectClass = EffectClass::Pure;
    fn schema() -> Value {
        json!({ "type": "string", "description": "Name of the secret" })
    }
    fn access(&self, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        if !valid_name(&self.raw) {
            return Err(ToolError::InvalidInput(format!(
                "invalid secret name `{}`",
                self.raw
            )));
        }
        Ok(vec![Access::read(ResourceUri::secret(&self.raw))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

// ------------------------------------------------------------------ memory

/// Default memory scope for keys without a `scope/` prefix.
pub const DEFAULT_MEM_SCOPE: &str = "project";

/// `"scope/key"` -> `("scope", "key")`; `"key"` -> `("project", "key")`.
pub fn split_mem_key(raw: &str) -> Result<(String, String), ToolError> {
    let (scope, key) = match raw.split_once('/') {
        Some((s, k)) => (s.to_string(), k.to_string()),
        None => (DEFAULT_MEM_SCOPE.to_string(), raw.to_string()),
    };
    if !valid_name(&scope) || key.is_empty() || key.contains(['*', '?', '[', '\n']) {
        return Err(ToolError::InvalidInput(format!(
            "invalid memory key `{raw}`"
        )));
    }
    Ok((scope, key))
}

impl Mem<Key> {
    pub fn scope_and_key(&self) -> Result<(String, String), ToolError> {
        split_mem_key(&self.raw)
    }
    fn store(&self) -> Result<&Arc<dyn agent_runtime::MemoryStore>, ToolError> {
        self.bound()?
            .ctx
            .memory
            .as_ref()
            .ok_or_else(|| ToolError::Failed("no memory store configured".into()))
    }
    /// Store `value`; returns the previous value.
    pub async fn set(&self, value: &str) -> Result<Option<String>, ToolError> {
        let (scope, key) = self.scope_and_key()?;
        self.store()?
            .remember(&scope, &key, value)
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))
    }
    pub async fn get(&self) -> Result<Option<String>, ToolError> {
        let (scope, key) = self.scope_and_key()?;
        let all = self
            .store()?
            .load(&scope)
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))?;
        Ok(all.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v))
    }
    pub async fn forget(&self) -> Result<Option<String>, ToolError> {
        let (scope, key) = self.scope_and_key()?;
        self.store()?
            .forget(&scope, &key)
            .await
            .map_err(|e| ToolError::Infra(e.to_string()))
    }
}

impl Capability for Mem<Key> {
    const CLASS: EffectClass = EffectClass::LocalWrite;
    fn schema() -> Value {
        json!({ "type": "string", "description": "Memory key, `scope/key` (scope defaults to `project`)" })
    }
    fn access(&self, _ctx: &AccessCtx) -> Result<Vec<Access>, ToolError> {
        let (scope, key) = split_mem_key(&self.raw)?;
        Ok(vec![Access::write(ResourceUri::mem(&format!(
            "{scope}/{key}"
        )))])
    }
    fn bind(&mut self, ctx: &ToolCtx, obs: &Observations) -> Result<(), ToolError> {
        for a in self.access(&access_ctx(ctx))? {
            check_granted(ctx, &a)?;
        }
        self.bound = Some(Bound {
            ctx: ctx.clone(),
            obs: obs.clone(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_globs_and_modes() {
        let w = Access::write(ResourceUri::fs("/w/**"));
        assert!(covers(&w, &Access::read(ResourceUri::fs("/w/a/b.rs"))));
        assert!(covers(&w, &Access::write(ResourceUri::fs("/w/a/b.rs"))));
        assert!(covers(&w, &Access::read(ResourceUri::fs("/w/sub/**"))));
        assert!(!covers(&w, &Access::read(ResourceUri::fs("/x/a"))));
        let r = Access::read(ResourceUri::fs("/w/a"));
        assert!(!covers(&r, &Access::write(ResourceUri::fs("/w/a"))));
        let star = Access::read(ResourceUri::fs("/w/*.rs"));
        assert!(!covers(
            &star,
            &Access::read(ResourceUri::fs("/w/src/a.rs"))
        ));
        assert!(covers(
            &Access::write(ResourceUri::cmd("npm run *")),
            &Access::write(ResourceUri::cmd("npm run test"))
        ));
    }

    #[test]
    fn grant_paths() {
        assert_eq!(fs_grant_path("/w/src/**"), PathBuf::from("/w/src"));
        assert_eq!(fs_grant_path("/w/a.rs"), PathBuf::from("/w/a.rs"));
        assert_eq!(fs_grant_path("/**"), PathBuf::from("/"));
    }
}
