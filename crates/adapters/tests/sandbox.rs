use agent_adapters::sandbox::{detect, probe, DirectExec};
use agent_runtime::{SandboxPort, SandboxSpec};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

#[test]
fn probe_reports() {
    let r = probe();
    eprintln!("sandbox probe: {r:?}");
    assert!(!r.implementation.is_empty());
    let d = detect();
    assert_eq!(d.report().implementation == "none", !d.report().available);
}

#[tokio::test]
async fn direct_exec_output_env_cwd() {
    let d = tempfile::tempdir().unwrap();
    let spec = SandboxSpec {
        cwd: d.path().to_path_buf(),
        env: vec![("FOO".into(), "bar".into())],
        timeout_ms: 10_000,
        ..Default::default()
    };
    std::env::set_var("LEAKY_SECRET", "x");
    let out = DirectExec::default()
        .run(&sh("echo $FOO; pwd; echo \"[$LEAKY_SECRET]\"; echo err >&2; exit 3"), &spec, CancellationToken::new())
        .await
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "bar");
    assert_eq!(std::fs::canonicalize(lines[1]).unwrap(), std::fs::canonicalize(d.path()).unwrap());
    assert_eq!(lines[2], "[]", "environment is cleared");
    assert_eq!(out.stderr, b"err\n");
    assert_eq!(out.status, Some(3));
    assert!(!out.timed_out);
}

#[tokio::test]
async fn timeout_kills_process_group() {
    let spec = SandboxSpec { timeout_ms: 200, ..Default::default() };
    let t = Instant::now();
    // The background grandchild must die too (process group kill).
    let out = DirectExec::default().run(&sh("sleep 30 & sleep 30"), &spec, CancellationToken::new()).await.unwrap();
    assert!(out.timed_out);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn cancel_stops_command() {
    let spec = SandboxSpec::default();
    let c = CancellationToken::new();
    let c2 = c.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let t = Instant::now();
    let r = DirectExec::default().run(&sh("sleep 30"), &spec, c).await;
    assert_eq!(r.unwrap_err(), "cancelled");
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn direct_refuses_isolated() {
    let spec = SandboxSpec { isolated: true, ..Default::default() };
    assert!(DirectExec::default().run(&sh("true"), &spec, CancellationToken::new()).await.is_err());
}

#[tokio::test]
async fn bwrap_enforces_spec_when_available() {
    let Ok(b) = agent_adapters::BwrapSandbox::probe() else {
        eprintln!("bwrap unavailable; skipping");
        return;
    };
    assert!(b.report().available);
    let w = tempfile::tempdir().unwrap();
    let ro = tempfile::tempdir().unwrap();
    std::fs::write(ro.path().join("in"), "data\n").unwrap();
    let spec = SandboxSpec {
        cwd: w.path().to_path_buf(),
        readable: vec![ro.path().to_path_buf()],
        writable: vec![w.path().to_path_buf()],
        env: vec![("FOO".into(), "bar".into())],
        timeout_ms: 10_000,
        ..Default::default()
    };
    let script = format!(
        "echo ok > out && cat {ro}/in && (echo x > {ro}/nope 2>/dev/null && echo WROTE || echo denied) && echo $FOO && cat /proc/net/dev",
        ro = ro.path().display()
    );
    let out = b.run(&sh(&script), &spec, CancellationToken::new()).await.unwrap();
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status, Some(0), "{s} {}", String::from_utf8_lossy(&out.stderr));
    assert!(s.starts_with("data\ndenied\nbar\n"), "{s}");
    assert!(!s.contains("eth"), "network unshared: {s}");
    assert_eq!(std::fs::read_to_string(w.path().join("out")).unwrap(), "ok\n");
    assert!(!ro.path().join("nope").exists());

    let spec = SandboxSpec { timeout_ms: 200, ..spec };
    let out = b.run(&sh("sleep 30"), &spec, CancellationToken::new()).await.unwrap();
    assert!(out.timed_out);
}

/// Host-side TCP echo server (stands in for an internet destination).
async fn echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

/// bash script: CONNECT through the in-sandbox bridge, print the status line
/// and the echoed payload; also try to reach `port` directly.
fn egress_script(port: u16, denied: u16) -> String {
    format!(
        "exec 3<>/dev/tcp/127.0.0.1/3128; printf 'CONNECT 127.0.0.1:{port} HTTP/1.1\\r\\n\\r\\nping\\n' >&3; \
         timeout 5 head -n 3 <&3 | tr -d '\\r'; exec 3<&-; \
         exec 4<>/dev/tcp/127.0.0.1/3128; printf 'CONNECT 127.0.0.1:{denied} HTTP/1.1\\r\\n\\r\\n' >&4; \
         timeout 5 head -n 1 <&4 | tr -d '\\r'; \
         (exec 5<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo DIRECT || echo nodirect; \
         echo \"$HTTPS_PROXY\""
    )
}

#[tokio::test]
async fn bwrap_egress_through_proxy_only() {
    let Ok(b) = agent_adapters::BwrapSandbox::probe() else {
        eprintln!("bwrap unavailable; skipping");
        return;
    };
    let b = b.with_bridge(env!("CARGO_BIN_EXE_agent-netbridge"));
    assert!(b.report().egress_proxy);
    let (port, denied) = (echo_server().await, echo_server().await);
    let spec = SandboxSpec { network: vec![format!("127.0.0.1:{port}")], timeout_ms: 20_000, ..Default::default() };
    let argv = vec!["/bin/bash".into(), "-c".into(), egress_script(port, denied)];
    let out = b.run(&argv, &spec, CancellationToken::new()).await.unwrap();
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        s,
        "HTTP/1.1 200 Connection Established\n\nping\nHTTP/1.1 403 Forbidden\nnodirect\nhttp://127.0.0.1:3128\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status, Some(0));
}

#[tokio::test]
async fn bwrap_copy_isolation() {
    let Ok(b) = agent_adapters::BwrapSandbox::probe() else {
        eprintln!("bwrap unavailable; skipping");
        return;
    };
    assert!(b.report().isolation);
    let w = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(w.path()).unwrap();
    std::fs::write(ws.join("a"), "1\n").unwrap();
    std::fs::write(ws.join("del"), "x").unwrap();
    let git = agent_adapters::sandbox::which("git").is_some();
    let g = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&ws)
            .output()
            .unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        String::from_utf8_lossy(&st.stdout).to_string()
    };
    if git {
        g(&["init", "-q"]);
        g(&["add", "a", "del"]);
        g(&["commit", "-qm", "init"]);
    }
    let spec = SandboxSpec {
        cwd: ws.clone(),
        writable: vec![ws.clone()],
        timeout_ms: 20_000,
        isolated: true,
        ..Default::default()
    };
    let mut script = format!("echo 2 > a && rm del && mkdir d && echo n > d/n && echo abs > {}/abs", ws.display());
    if git {
        script.push_str(
            " && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false commit -qam change && git log --oneline | wc -l",
        );
    }
    let argv = sh(&script);
    let run = b.run_isolated(&argv, &spec, CancellationToken::new()).await.unwrap();
    let stderr = String::from_utf8_lossy(&run.output.stderr).to_string();
    assert_eq!(run.output.status, Some(0), "{stderr}");
    let ch = &run.output.overlay_changes;
    for p in ["a", "abs", "d", "d/n", "del"] {
        assert!(ch.contains(&p.to_string()), "{p} missing from {ch:?}");
    }
    // Nothing written back.
    assert_eq!(std::fs::read_to_string(ws.join("a")).unwrap(), "1\n");
    assert!(ws.join("del").exists() && !ws.join("abs").exists() && !ws.join("d").exists());
    if git {
        assert_eq!(String::from_utf8_lossy(&run.output.stdout).trim(), "2", "{stderr}");
        assert!(ch.iter().any(|p| p.starts_with(".git/objects/")), "{ch:?}");
        assert_eq!(g(&["log", "--oneline"]).lines().count(), 1);
    }
    agent_adapters::apply_isolated_changes(run.copy.path(), &ws, &run.changes).unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("a")).unwrap(), "2\n");
    assert_eq!(std::fs::read_to_string(ws.join("d/n")).unwrap(), "n\n");
    assert!(!ws.join("del").exists());
    if git {
        assert_eq!(g(&["log", "--oneline"]).lines().count(), 2, "merged commit is readable");
        assert_eq!(g(&["status", "--porcelain"]).trim(), "?? abs\n?? d/");
    }
    // Via the port: same change list, offline, timeout discards changes.
    let out = b.run(&sh("echo 3 > a"), &spec, CancellationToken::new()).await.unwrap();
    assert_eq!(out.overlay_changes, vec!["a"]);
    let spec = SandboxSpec { timeout_ms: 300, ..spec };
    let out = b.run(&sh("echo 4 > a; sleep 30"), &spec, CancellationToken::new()).await.unwrap();
    assert!(out.timed_out && out.overlay_changes.is_empty());
    assert_eq!(std::fs::read_to_string(ws.join("a")).unwrap(), "2\n");
}

#[tokio::test]
async fn container_runs_when_available() {
    let c = agent_adapters::Container::ephemeral().bridge(Some(env!("CARGO_BIN_EXE_agent-netbridge").into()));
    if let Err(e) = c.runtime_ready() {
        eprintln!("container runtime unavailable; skipping: {e}");
        return;
    }
    let img = std::process::Command::new(if agent_adapters::sandbox::which("docker").is_some() {
        "docker"
    } else {
        "podman"
    })
    .args(["image", "inspect", agent_adapters::sandbox::container::DEFAULT_IMAGE])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false);
    if !img {
        eprintln!("image {} not present locally; skipping", agent_adapters::sandbox::container::DEFAULT_IMAGE);
        return;
    }
    let w = tempfile::tempdir().unwrap();
    std::fs::write(w.path().join("a"), "1\n").unwrap();
    let spec = SandboxSpec { cwd: w.path().to_path_buf(), timeout_ms: 60_000, ..Default::default() };
    let run = c
        .run_isolated(
            &sh("cat a; echo 2 > a; echo n > b; cat /proc/net/dev | grep -c eth || true"),
            &spec,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(run.output.status, Some(0), "{}", String::from_utf8_lossy(&run.output.stderr));
    assert_eq!(String::from_utf8_lossy(&run.output.stdout), "1\n0\n");
    assert_eq!(run.output.overlay_changes, vec!["a", "b"]);
    assert_eq!(std::fs::read_to_string(w.path().join("a")).unwrap(), "1\n");

    // Allowlisted egress through the proxy; the container has no other network.
    let (port, denied) = (echo_server().await, echo_server().await);
    let spec = SandboxSpec { network: vec![format!("127.0.0.1:{port}")], ..spec };
    let argv = vec!["/bin/bash".into(), "-c".into(), egress_script(port, denied)];
    let out = c.run(&argv, &spec, CancellationToken::new()).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "HTTP/1.1 200 Connection Established\n\nping\nHTTP/1.1 403 Forbidden\nnodirect\nhttp://127.0.0.1:3128\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
