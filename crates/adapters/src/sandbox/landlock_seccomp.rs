//! Degraded Linux sandbox for hosts where bubblewrap / unprivileged user
//! namespaces are unavailable: landlock (filesystem: read-only everywhere,
//! writes only to `spec.writable` and a private `TMPDIR`; TCP bind/connect
//! denied on ABI >= 4; abstract unix sockets and signals scoped on ABI >= 6)
//! plus a seccomp filter that refuses `AF_INET`/`AF_INET6`/`AF_PACKET`/
//! `AF_VSOCK` sockets and `io_uring_setup`. Both are applied in the child
//! right before `exec` (the ruleset and BPF program are built beforehand, so
//! the child does not allocate).
//!
//! The degraded mode is always offline: it cannot route traffic through the
//! egress proxy, so `spec.network` is ignored (reported via
//! `egress_proxy = false`; callers must ask before running networked
//! commands here). Isolated runs use the copy-based [`IsolatedCopy`].

use super::isolate::{IsolatedCopy, IsolatedRun};
use agent_runtime::{ExecOutput, SandboxPort, SandboxReport, SandboxSpec};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct LandlockSandbox {
    abi: i32,
    notes: Vec<String>,
}

impl LandlockSandbox {
    /// Check that the kernel supports landlock and seccomp and that a trial
    /// command is actually confined. `Err` carries the reason.
    pub fn probe() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            imp::probe()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("landlock is Linux-only".into())
        }
    }

    /// Landlock ABI version of the running kernel.
    pub fn abi(&self) -> i32 {
        self.abi
    }

    /// Run with copy-based isolation and keep the copy (see [`IsolatedRun`]).
    pub async fn run_isolated(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cancel: CancellationToken,
    ) -> Result<IsolatedRun, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let cwd = spec.cwd.clone();
        let copy = tokio::task::spawn_blocking(move || IsolatedCopy::create(&cwd, None))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("workspace copy: {e}"))?;
        let writable = vec![copy.path().to_path_buf(), copy.tmp().to_path_buf()];
        let out = self.run_confined(argv, spec, copy.path(), &writable, copy.tmp(), cancel).await?;
        let copy2 = tokio::task::spawn_blocking(move || IsolatedRun::finish(out, copy));
        copy2.await.map_err(|e| e.to_string())?
    }

    async fn run_confined(
        &self,
        argv: &[String],
        spec: &SandboxSpec,
        cwd: &Path,
        writable: &[PathBuf],
        tmp: &Path,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        #[cfg(target_os = "linux")]
        {
            imp::run(argv, spec, cwd, writable, tmp, cancel).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (argv, spec, cwd, writable, tmp, cancel);
            Err("landlock is Linux-only".into())
        }
    }
}

#[async_trait]
impl SandboxPort for LandlockSandbox {
    fn report(&self) -> SandboxReport {
        SandboxReport {
            implementation: "landlock".into(),
            available: true,
            egress_proxy: false,
            isolation: true,
            notes: self.notes.clone(),
        }
    }

    async fn run(&self, argv: &[String], spec: &SandboxSpec, cancel: CancellationToken) -> Result<ExecOutput, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        if spec.isolated {
            return Ok(self.run_isolated(argv, spec, cancel).await?.output);
        }
        let tmp = tempfile::Builder::new().prefix("agent-tmp-").tempdir().map_err(|e| e.to_string())?;
        let mut writable = spec.writable.clone();
        writable.push(tmp.path().to_path_buf());
        self.run_confined(argv, spec, &spec.cwd, &writable, tmp.path(), cancel).await
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::super::{env_with_path, exec, which};
    use super::LandlockSandbox;
    use agent_runtime::{ExecOutput, SandboxSpec};
    use landlock::{
        Access, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, Scope, ABI,
    };
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
        TargetArch,
    };
    use std::collections::BTreeMap;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    /// Highest ABI whose semantics we rely on; older kernels degrade (best effort).
    const TARGET: ABI = ABI::V6;

    pub(super) fn kernel_abi() -> i32 {
        // SAFETY: LANDLOCK_CREATE_RULESET_VERSION query; no pointers read.
        unsafe {
            libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<libc::c_void>(), 0usize, 1u32) as i32
        }
    }

    pub(super) struct Prepared {
        ruleset: OwnedFd,
        bpf: Option<BpfProgram>,
    }

    fn ll<E: std::fmt::Display>(e: E) -> String {
        format!("landlock: {e}")
    }

    pub(super) fn prepare(writable: &[PathBuf]) -> Result<Prepared, String> {
        let mut rs = Ruleset::default()
            .handle_access(AccessFs::from_all(TARGET))
            .map_err(ll)?
            .handle_access(AccessNet::from_all(TARGET))
            .map_err(ll)?
            .scope(Scope::from_all(TARGET))
            .map_err(ll)?
            .create()
            .map_err(ll)?;
        rs = rs.add_rule(PathBeneath::new(PathFd::new("/").map_err(ll)?, AccessFs::from_read(TARGET))).map_err(ll)?;
        let devs = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];
        let files = devs.iter().map(PathBuf::from).map(|p| (p, false));
        for (p, full) in files.chain(writable.iter().map(|w| (w.clone(), true))) {
            let Ok(fd) = PathFd::new(&p) else { continue };
            let access = if full && p.is_dir() { AccessFs::from_all(TARGET) } else { AccessFs::from_file(TARGET) };
            rs = rs.add_rule(PathBeneath::new(fd, access)).map_err(ll)?;
        }
        let ruleset: Option<OwnedFd> = rs.into();
        let ruleset = ruleset.ok_or("landlock is not supported by this kernel")?;
        Ok(Prepared { ruleset, bpf: socket_filter().ok() })
    }

    /// Deny internet/packet/vsock sockets and io_uring (which can create
    /// sockets without the `socket` syscall).
    pub(super) fn socket_filter() -> Result<BpfProgram, String> {
        let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| format!("seccomp: {e}"))?;
        let err = |e: seccompiler::BackendError| format!("seccomp: {e}");
        let fams = [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET, libc::AF_VSOCK];
        let conds = fams
            .iter()
            .map(|f| {
                SeccompRule::new(vec![
                    SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, *f as u64).map_err(err)?
                ])
                .map_err(err)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut rules = BTreeMap::new();
        rules.insert(libc::SYS_socket, conds.clone());
        #[cfg(target_arch = "x86_64")]
        rules.insert(libc::SYS_socket | 0x4000_0000, conds); // x32 ABI alias
        rules.insert(libc::SYS_io_uring_setup, vec![]);
        let f = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EACCES as u32), arch)
            .map_err(err)?;
        f.try_into().map_err(err)
    }

    /// Runs in the forked child: must not allocate.
    fn lock_self(fd: i32, bpf: Option<&BpfProgram>) -> io::Result<()> {
        // SAFETY: plain syscalls on valid arguments.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if let Some(b) = bpf {
            seccompiler::apply_filter(b).map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
        }
        Ok(())
    }

    pub(super) fn probe() -> Result<LandlockSandbox, String> {
        let abi = kernel_abi();
        if abi < 1 {
            return Err(format!("landlock not available ({})", io::Error::last_os_error()));
        }
        let mut notes = vec![format!("landlock ABI v{abi}")];
        let seccomp = socket_filter();
        if let Err(e) = &seccomp {
            notes.push(format!("seccomp socket filter unavailable ({e})"));
        }
        // Trial: writing outside the writable set must fail.
        let d = tempfile::tempdir().map_err(|e| e.to_string())?;
        let target = d.path().join("probe");
        let sh = which("sh").ok_or("sh not found")?;
        let p = prepare(&[]).map_err(|e| format!("landlock ruleset: {e}"))?;
        let fd = p.ruleset.as_raw_fd();
        let bpf = p.bpf;
        let mut cmd = std::process::Command::new(sh);
        cmd.args(["-c", "echo x > \"$1\"", "sh"]).arg(&target);
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        // SAFETY: lock_self only performs syscalls.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || lock_self(fd, bpf.as_ref()));
        }
        let st = cmd.status().map_err(|e| format!("landlock trial: {e}"))?;
        drop(p.ruleset);
        if st.success() || target.exists() {
            return Err(format!("landlock trial was not confined (status {st})"));
        }
        if abi < 4 {
            notes.push("no landlock TCP restriction (ABI < 4); relying on seccomp".into());
        }
        if abi < 6 {
            notes.push("abstract unix sockets and signals are not scoped (ABI < 6)".into());
        }
        notes.push(
            "degraded mode (no user namespaces): always offline, network cannot be routed through the egress proxy"
                .into(),
        );
        notes.push("pathname unix sockets on the host remain connectable; /tmp is not private (TMPDIR is)".into());
        notes.push("isolated execution is copy-based (workspace copied to a scratch dir)".into());
        Ok(LandlockSandbox { abi, notes })
    }

    pub(super) async fn run(
        argv: &[String],
        spec: &SandboxSpec,
        cwd: &Path,
        writable: &[PathBuf],
        tmp: &Path,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, String> {
        let (bin, args) = argv.split_first().ok_or("empty argv")?;
        let w = writable.to_vec();
        let p = tokio::task::spawn_blocking(move || prepare(&w)).await.map_err(|e| e.to_string())??;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args).env_clear().envs(env_with_path(&spec.env)).env("TMPDIR", tmp);
        if !cwd.as_os_str().is_empty() {
            cmd.current_dir(cwd);
        }
        let fd = p.ruleset.as_raw_fd();
        let bpf = p.bpf;
        // SAFETY: lock_self only performs syscalls (no allocation).
        unsafe {
            cmd.pre_exec(move || lock_self(fd, bpf.as_ref()));
        }
        let out = exec(cmd, spec.timeout_ms, cancel, || {}).await;
        drop(p.ruleset);
        out
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn sh(s: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), s.into()]
    }

    #[test]
    fn seccomp_filter_builds() {
        if matches!(std::env::consts::ARCH, "x86_64" | "aarch64" | "riscv64") {
            assert!(!imp::socket_filter().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn confines_writes_and_network() {
        let sb = match LandlockSandbox::probe() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("landlock unavailable; skipping: {e}");
                return;
            }
        };
        assert_eq!(sb.report().implementation, "landlock");
        assert!(!sb.report().egress_proxy);
        let w = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("in"), "data").unwrap();
        // A listener on the host that the sandbox must not reach.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let spec = SandboxSpec {
            cwd: w.path().to_path_buf(),
            writable: vec![w.path().to_path_buf()],
            network: vec![format!("127.0.0.1:{port}")], // ignored: degraded mode is offline
            timeout_ms: 20_000,
            ..Default::default()
        };
        let script = format!(
            "echo ok > out; cat {o}/in; echo; (echo x > {o}/nope) 2>/dev/null && echo WROTE || echo denied; \
             echo t > \"$TMPDIR/t\" && echo tmp; \
             if command -v python3 >/dev/null; then python3 -c 'import socket\n\
try:\n socket.socket(socket.AF_INET, socket.SOCK_DGRAM); print(\"udp open\")\n\
except OSError: print(\"no inet\")'; else echo no inet; fi; \
             (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo CONNECTED || echo noconn",
            o = other.path().display()
        );
        let out = sb.run(&["/bin/bash".into(), "-c".into(), script], &spec, CancellationToken::new()).await.unwrap();
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(s, "data\ndenied\ntmp\nno inet\nnoconn\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(w.path().join("out")).unwrap(), "ok\n");
        assert!(!other.path().join("nope").exists());

        // Isolated: runs on a copy, reports changes, writes nothing back.
        std::fs::write(w.path().join("a"), "1").unwrap();
        let spec = SandboxSpec { isolated: true, writable: vec![], ..spec };
        let run =
            sb.run_isolated(&sh("echo 2 > a; echo n > b; rm out"), &spec, CancellationToken::new()).await.unwrap();
        assert_eq!(run.output.status, Some(0), "{}", String::from_utf8_lossy(&run.output.stderr));
        assert_eq!(run.output.overlay_changes, vec!["a", "b", "out"]);
        assert_eq!(std::fs::read_to_string(w.path().join("a")).unwrap(), "1");
        assert!(w.path().join("out").exists());
        super::super::isolate::apply_isolated_changes(run.copy.path(), w.path(), &run.changes).unwrap();
        assert_eq!(std::fs::read_to_string(w.path().join("a")).unwrap(), "2\n");
        assert!(!w.path().join("out").exists());
        // An isolated run can not write the real workspace by absolute path.
        let script = format!("echo x > {}/a", w.path().display());
        let out = sb.run(&sh(&script), &spec, CancellationToken::new()).await.unwrap();
        assert_ne!(out.status, Some(0));
        assert_eq!(std::fs::read_to_string(w.path().join("a")).unwrap(), "2\n");
    }
}
