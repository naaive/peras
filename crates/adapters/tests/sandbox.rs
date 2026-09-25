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
