//! Egress proxy: HTTP `CONNECT` and plain (absolute-form) HTTP proxying with
//! a `host:port` allowlist. Every destination is logged (tracing target
//! `agent::egress`) and recorded in [`EgressProxy::events`].
//!
//! Sandboxes reach the proxy through a unix socket bound into an otherwise
//! network-less namespace; inside, the [`netbridge_main`] helper (the
//! `agent-netbridge` binary) listens on loopback and forwards each TCP
//! connection to that socket, then runs the command with `HTTP(S)_PROXY`
//! pointing at it.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Loopback address the bridge listens on inside a sandbox.
pub const BRIDGE_LISTEN: &str = "127.0.0.1:3128";
/// Binary name of the bridge helper.
pub const NETBRIDGE_BIN: &str = "agent-netbridge";

const MAX_HEAD: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// `host:port` allowlist. Entries: `host:port`, `[v6]:port`, `*.suffix:port`
/// (subdomains only), `host:*` (any port), or `host` (ports 80 and 443).
/// Host names compare case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    entries: Vec<(String, PortRule)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PortRule {
    Any,
    Web,
    Is(u16),
}

impl Allowlist {
    pub fn new<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> Self {
        let entries = entries
            .into_iter()
            .filter_map(|e| {
                let e = e.as_ref().trim();
                if e.is_empty() {
                    return None;
                }
                Some(match split_host_port(e) {
                    Some((h, "*")) => (norm_host(h), PortRule::Any),
                    Some((h, p)) => (norm_host(h), PortRule::Is(p.parse().ok()?)),
                    None => (norm_host(e), PortRule::Web),
                })
            })
            .collect();
        Allowlist { entries }
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = norm_host(host);
        self.entries.iter().any(|(pat, pr)| {
            let port_ok = match pr {
                PortRule::Any => true,
                PortRule::Web => port == 80 || port == 443,
                PortRule::Is(p) => *p == port,
            };
            let host_ok = match pat.strip_prefix("*.") {
                Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
                None => *pat == host,
            };
            port_ok && host_ok
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn norm_host(h: &str) -> String {
    h.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase()
}

/// Split `host:port` / `[v6]:port`. `None` when there is no port.
fn split_host_port(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        return Some((h, tail.strip_prefix(':')?));
    }
    let (h, p) = s.rsplit_once(':')?;
    if h.contains(':') {
        return None; // bare IPv6 without brackets
    }
    Some((h, p))
}

/// One proxied (or refused) destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEvent {
    /// `CONNECT` or the HTTP method.
    pub method: String,
    /// `host:port`.
    pub dest: String,
    pub allowed: bool,
}

/// A running proxy; stops when dropped.
#[derive(Debug)]
pub struct EgressProxy {
    addr: Option<SocketAddr>,
    unix: Option<PathBuf>,
    events: Arc<Mutex<Vec<EgressEvent>>>,
    cancel: CancellationToken,
}

struct Shared {
    allow: Allowlist,
    events: Arc<Mutex<Vec<EgressEvent>>>,
}

impl EgressProxy {
    /// Listen on `127.0.0.1:<ephemeral>`. Must be called inside a tokio runtime.
    pub fn start<S: AsRef<str>>(allowlist: impl IntoIterator<Item = S>) -> io::Result<Self> {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_l.set_nonblocking(true)?;
        let addr = std_l.local_addr()?;
        let (proxy, shared) = Self::new(allowlist, Some(addr), None);
        let l = in_runtime(|| tokio::net::TcpListener::from_std(std_l))?;
        let cancel = proxy.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = l.accept() => if let Ok((s, _)) = r {
                        let (sh, c) = (shared.clone(), cancel.clone());
                        tokio::spawn(async move { tokio::select! { _ = handle(s, &sh) => {}, _ = c.cancelled() => {} } });
                    }
                }
            }
        });
        Ok(proxy)
    }

    /// Listen on a unix socket at `path` (mode 0666 so sandboxed users with a
    /// different uid mapping can connect). Must be called inside a tokio runtime.
    #[cfg(unix)]
    pub fn start_unix<S: AsRef<str>>(path: &Path, allowlist: impl IntoIterator<Item = S>) -> io::Result<Self> {
        let std_l = std::os::unix::net::UnixListener::bind(path)?;
        std_l.set_nonblocking(true)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        }
        let (proxy, shared) = Self::new(allowlist, None, Some(path.to_path_buf()));
        let l = in_runtime(|| tokio::net::UnixListener::from_std(std_l))?;
        let cancel = proxy.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = l.accept() => if let Ok((s, _)) = r {
                        let (sh, c) = (shared.clone(), cancel.clone());
                        tokio::spawn(async move { tokio::select! { _ = handle(s, &sh) => {}, _ = c.cancelled() => {} } });
                    }
                }
            }
        });
        Ok(proxy)
    }

    fn new<S: AsRef<str>>(
        allowlist: impl IntoIterator<Item = S>,
        addr: Option<SocketAddr>,
        unix: Option<PathBuf>,
    ) -> (Self, Arc<Shared>) {
        let events = Arc::new(Mutex::new(vec![]));
        let shared = Arc::new(Shared { allow: Allowlist::new(allowlist), events: events.clone() });
        (EgressProxy { addr, unix, events, cancel: CancellationToken::new() }, shared)
    }

    /// TCP address (for [`EgressProxy::start`]).
    pub fn addr(&self) -> Option<SocketAddr> {
        self.addr
    }
    /// Unix socket path (for [`EgressProxy::start_unix`]).
    pub fn unix_path(&self) -> Option<&Path> {
        self.unix.as_deref()
    }
    /// Destinations seen so far.
    pub fn events(&self) -> Vec<EgressEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn in_runtime<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::other(format!("egress proxy needs a tokio runtime: {e}")))?;
    f()
}

async fn respond<W: AsyncWrite + Unpin>(w: &mut W, status: &str, msg: &str) {
    let body = format!("{msg}\n");
    let r = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = w.write_all(r.as_bytes()).await;
    let _ = w.shutdown().await;
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(s: S, sh: &Shared) {
    let mut r = BufReader::new(s);
    // Read the request head.
    let mut head: Vec<String> = vec![];
    let mut total = 0;
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(30), r.read_line(&mut line)).await;
        match read {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => return,
            Ok(Ok(n)) => total += n,
        }
        if total > MAX_HEAD {
            return respond(r.get_mut(), "431 Request Header Fields Too Large", "request head too large").await;
        }
        let l = line.trim_end_matches(['\r', '\n']).to_string();
        if l.is_empty() {
            if head.is_empty() {
                continue;
            }
            break;
        }
        head.push(l);
    }
    let mut parts = head[0].split_whitespace();
    let (Some(method), Some(target), version) = (parts.next(), parts.next(), parts.next().unwrap_or("HTTP/1.1")) else {
        return respond(r.get_mut(), "400 Bad Request", "malformed request line").await;
    };
    let (method, target, version) = (method.to_string(), target.to_string(), version.to_string());
    let connect = method.eq_ignore_ascii_case("CONNECT");
    let parsed = if connect {
        split_host_port(&target).and_then(|(h, p)| Some((h.to_string(), p.parse::<u16>().ok()?, String::new())))
    } else {
        parse_http_url(&target)
    };
    let Some((host, port, path)) = parsed else {
        return respond(
            r.get_mut(),
            "400 Bad Request",
            "proxy requests need CONNECT host:port or an absolute http:// URL",
        )
        .await;
    };
    let dest = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let allowed = sh.allow.allows(&host, port);
    tracing::info!(target: "agent::egress", method = %method, dest = %dest, allowed, "egress");
    if let Ok(mut e) = sh.events.lock() {
        e.push(EgressEvent { method: method.clone(), dest: dest.clone(), allowed });
    }
    if !allowed {
        return respond(r.get_mut(), "403 Forbidden", &format!("egress to {dest} is not in the allowlist")).await;
    }
    let up = match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect((host.as_str(), port))).await {
        Ok(Ok(u)) => u,
        Ok(Err(e)) => return respond(r.get_mut(), "502 Bad Gateway", &format!("connect {dest}: {e}")).await,
        Err(_) => return respond(r.get_mut(), "504 Gateway Timeout", &format!("connect {dest}: timed out")).await,
    };
    let mut up = up;
    let pending = r.buffer().to_vec();
    let mut client = r.into_inner();
    if connect {
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
    } else {
        // Origin-form request line; drop proxy headers; one request per
        // upstream connection (the destination is fixed per connection).
        let mut out = format!("{method} {path} {version}\r\n");
        for h in &head[1..] {
            let name = h.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
            if name.starts_with("proxy-") || name == "connection" || name == "keep-alive" {
                continue;
            }
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str("Connection: close\r\n\r\n");
        if up.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
    if !pending.is_empty() && up.write_all(&pending).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
}

/// `http://host[:port][/path]` -> (host, port, origin-form path).
fn parse_http_url(u: &str) -> Option<(String, u16, String)> {
    let rest = u.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).map(|_| &u[7..])?;
    let (auth, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let auth = auth.rsplit_once('@').map(|(_, a)| a).unwrap_or(auth);
    let (host, port) = match split_host_port(auth) {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (auth, 80),
    };
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    Some((norm_host(host), port, path))
}

/// Environment variables pointing a sandboxed command at the bridge.
pub fn proxy_env(listen: &str) -> Vec<(String, String)> {
    let url = format!("http://{listen}");
    ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .map(|k| (k.to_string(), url.clone()))
        .chain([("NO_PROXY".to_string(), String::new()), ("no_proxy".to_string(), String::new())])
        .collect()
}

/// Find the `agent-netbridge` helper: `$AGENT_NETBRIDGE`, next to the
/// current executable (or its parent, for test binaries in `deps/`), `PATH`.
pub fn find_netbridge() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGENT_NETBRIDGE").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1).take(2) {
            let p = dir.join(NETBRIDGE_BIN);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    super::which(NETBRIDGE_BIN)
}

/// Entry point of the `agent-netbridge` helper:
/// `agent-netbridge --listen ADDR --unix PATH -- CMD [ARGS...]`.
/// Forwards every TCP connection on `ADDR` to the unix socket `PATH`, runs
/// the command and exits with its status.
#[cfg(unix)]
pub fn netbridge_main(args: Vec<String>) -> i32 {
    use std::os::unix::net::UnixStream;
    let mut listen = BRIDGE_LISTEN.to_string();
    let mut unix = None;
    let mut it = args.into_iter();
    let mut cmd = vec![];
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => listen = it.next().unwrap_or_default(),
            "--unix" => unix = it.next(),
            "--" => {
                cmd = it.collect();
                break;
            }
            other => {
                eprintln!("agent-netbridge: unknown argument {other}");
                return 125;
            }
        }
    }
    let (Some(unix), false) = (unix, cmd.is_empty()) else {
        eprintln!("usage: agent-netbridge --listen ADDR --unix PATH -- CMD [ARGS...]");
        return 125;
    };
    let l = match std::net::TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-netbridge: listen {listen}: {e}");
            return 125;
        }
    };
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let unix = unix.clone();
            std::thread::spawn(move || {
                let Ok(u) = UnixStream::connect(&unix) else {
                    return;
                };
                let (Ok(mut s2), Ok(mut u2)) = (s.try_clone(), u.try_clone()) else {
                    return;
                };
                let up = std::thread::spawn(move || {
                    let _ = io::copy(&mut s2, &mut u2);
                    let _ = u2.shutdown(std::net::Shutdown::Write);
                });
                let (mut u, mut s) = (u, s);
                let _ = io::copy(&mut u, &mut s);
                let _ = s.shutdown(std::net::Shutdown::Write);
                let _ = up.join();
            });
        }
    });
    match std::process::Command::new(&cmd[0]).args(&cmd[1..]).status() {
        Ok(st) => {
            use std::os::unix::process::ExitStatusExt;
            st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
        }
        Err(e) => {
            eprintln!("agent-netbridge: {}: {e}", cmd[0]);
            if e.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn allowlist_matching() {
        let a = Allowlist::new(["api.example.com:443", "*.crates.io:443", "10.0.0.1:*", "plain.org", "[::1]:8080"]);
        assert!(a.allows("api.example.com", 443));
        assert!(a.allows("API.Example.com.", 443));
        assert!(!a.allows("api.example.com", 80));
        assert!(!a.allows("evil.com", 443));
        assert!(a.allows("static.crates.io", 443));
        assert!(!a.allows("crates.io", 443));
        assert!(!a.allows("evilcrates.io", 443));
        assert!(a.allows("10.0.0.1", 22));
        assert!(a.allows("plain.org", 80) && a.allows("plain.org", 443) && !a.allows("plain.org", 22));
        assert!(a.allows("::1", 8080));
        assert!(Allowlist::new(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn url_parsing() {
        assert_eq!(parse_http_url("http://h:81/a?b"), Some(("h".into(), 81, "/a?b".into())));
        assert_eq!(parse_http_url("HTTP://H"), Some(("h".into(), 80, "/".into())));
        assert_eq!(parse_http_url("http://u:p@h?q"), Some(("h".into(), 80, "/?q".into())));
        assert_eq!(parse_http_url("https://h/"), None);
        assert_eq!(parse_http_url("/rel"), None);
    }

    /// Echo server: replies with everything it receives.
    async fn echo_server() -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        a
    }

    async fn talk(proxy: SocketAddr, req: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await.unwrap().unwrap();
        out
    }

    #[tokio::test]
    async fn connect_allow_and_deny() {
        let echo = echo_server().await;
        let p = EgressProxy::start([format!("127.0.0.1:{}", echo.port())]).unwrap();
        let pa = p.addr().unwrap();
        let ok = talk(pa, &format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\n\r\nping", echo.port())).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.ends_with("\r\n\r\nping"), "{ok}");
        // Same host, other port: denied.
        let other = echo_server().await;
        let no = talk(pa, &format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nping", other.port())).await;
        assert!(no.starts_with("HTTP/1.1 403"), "{no}");
        assert!(!no.contains("ping"));
        let no = talk(pa, &format!("CONNECT localhost:{} HTTP/1.1\r\n\r\n", echo.port())).await;
        assert!(no.starts_with("HTTP/1.1 403"), "names are not resolved for matching: {no}");
        let ev = p.events();
        assert_eq!(ev.len(), 3);
        assert!(ev[0].allowed && !ev[1].allowed && !ev[2].allowed);
        assert_eq!(ev[0].dest, format!("127.0.0.1:{}", echo.port()));
        assert_eq!(ev[0].method, "CONNECT");
    }

    #[tokio::test]
    async fn plain_http_is_rewritten() {
        let echo = echo_server().await;
        let p = EgressProxy::start([format!("127.0.0.1:{}", echo.port())]).unwrap();
        let req = format!(
            "GET http://127.0.0.1:{}/x?y HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Authorization: secret\r\nConnection: keep-alive\r\n\r\n",
            echo.port()
        );
        let got = talk(p.addr().unwrap(), &req).await;
        // The echo server returns what the proxy sent upstream.
        assert!(got.starts_with("GET /x?y HTTP/1.1\r\nHost: 127.0.0.1\r\n"), "{got}");
        assert!(!got.contains("secret") && got.contains("Connection: close"), "{got}");
        let no = talk(p.addr().unwrap(), "GET http://example.com/ HTTP/1.1\r\n\r\n").await;
        assert!(no.starts_with("HTTP/1.1 403"), "{no}");
        let bad = talk(p.addr().unwrap(), "GET /relative HTTP/1.1\r\n\r\n").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        assert_eq!(p.events().len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener() {
        let echo = echo_server().await;
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("egress.sock");
        let p = EgressProxy::start_unix(&sock, [format!("127.0.0.1:{}", echo.port())]).unwrap();
        assert_eq!(p.unix_path(), Some(sock.as_path()));
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        s.write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nhi", echo.port()).as_bytes()).await.unwrap();
        s.shutdown().await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200") && out.ends_with("hi"), "{out}");
        drop(p);
    }
}
