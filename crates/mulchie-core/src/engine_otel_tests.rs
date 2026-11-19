//! The trace receiver through the engine: started and stopped by the
//! setting on an ephemeral loopback port, a taken port as a posture
//! finding, and an endpoint regression attributed to its workspace, commit
//! and session. Nothing here signals or deletes.

use super::*;
use crate::engine::tests::{engine_with, engine_with_procs};
use crate::otel::{Attr, AttrValue, ResourceSpans, Span, TraceBatch, KIND_SERVER};
use crate::process::FakeProcesses;
use crate::watch::FindingCategory;
use std::io::{Read, Write};

/// Builds off, budgets off, memory guard, runaway chains and update checks
/// off; the receiver off unless a test turns it on (on port 0).
fn safe() -> Config {
    let mut c = Config {
        throttle_builds: false,
        ..Config::default()
    };
    c.memory_guard.action = crate::config::MemoryAction::Off;
    c.runaway.action = crate::config::MemoryAction::Off;
    c.updates.check = crate::config::UpdateCheck::Off;
    c.otel.enabled = false;
    c
}

fn on() -> Config {
    let mut c = safe();
    c.otel.enabled = true;
    c.otel.port = 0;
    c
}

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .env("HOME", home)
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {o:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commit(dir: &Path, home: &Path, msg: &str) -> String {
    git(dir, home, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(dir, home, &["rev-parse", "HEAD"])
}

fn batch(service: &str, pid: i64, op: &str, ends_ms: &[u64], dur_ms: u64) -> TraceBatch {
    let s = |k: &str, v: &str| Attr {
        key: k.into(),
        value: AttrValue::Str(v.into()),
    };
    let (method, route) = op.split_once(' ').unwrap();
    TraceBatch {
        resource_spans: vec![ResourceSpans {
            resource: vec![
                s("service.name", service),
                Attr {
                    key: "process.pid".into(),
                    value: AttrValue::Int(pid),
                },
            ],
            spans: ends_ms
                .iter()
                .map(|end| Span {
                    name: op.into(),
                    kind: KIND_SERVER,
                    is_root: true,
                    start_unix_nano: (end - dur_ms) * 1_000_000,
                    end_unix_nano: end * 1_000_000,
                    status_code: 0,
                    attributes: vec![s("http.route", route), s("http.request.method", method)],
                })
                .collect(),
        }],
        ..TraceBatch::default()
    }
}

fn post(port: u16, body: &str) -> u16 {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out[9..12].parse().unwrap()
}

fn set(e: &Engine, cfg: Config) {
    e.set_config(cfg).unwrap();
}

#[test]
fn the_receiver_is_off_by_default_and_follows_the_setting() {
    assert!(!Config::default().otel.enabled);
    assert_eq!(Config::default().otel.port, 4318);
    let (_t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let now = now_unix();
    e.perf_tick(now);
    assert_eq!(e.otel_posture(), (None, None));
    let st = e.snapshot_state();
    assert!(!st.otel.enabled && !st.otel.listening);

    set(&e, on());
    e.perf_tick(now + 1);
    let (port, err) = e.otel_posture();
    let port = port.expect("listening");
    assert!(err.is_none());
    let end = otel::now_ms() * 1_000_000;
    let body = format!(
        r#"{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"shop"}}}}]}},"scopeSpans":[{{"spans":[{{"name":"GET /cart","kind":2,"startTimeUnixNano":"{}","endTimeUnixNano":"{end}"}}]}}]}}]}}"#,
        end - 40_000_000
    );
    assert_eq!(post(port, &body), 200);
    let r = e.otel_report(None, 15);
    assert!(r.enabled && r.listening);
    assert_eq!(r.port, Some(port));
    assert_eq!(r.endpoint, Some(format!("http://127.0.0.1:{port}")));
    assert_eq!(r.services.len(), 1);
    assert_eq!(r.services[0].operations[0].operation, "GET /cart");
    assert_eq!(r.services[0].operations[0].stats.count, 1);
    assert!(e.otel_report(Some("other"), 15).services.is_empty());
    let st = e.snapshot_state();
    assert!(st.otel.listening && st.otel.services == 1);

    // Off again: stopped, and what it held is gone.
    set(&e, safe());
    e.perf_tick(now + 2);
    assert_eq!(e.otel_posture(), (None, None));
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    assert!(e.otel_report(None, 15).services.is_empty());
}

#[test]
fn an_engine_without_the_lock_runs_no_receiver() {
    let (_t, e) = engine_with(on());
    e.perf_tick(now_unix());
    assert_eq!(e.otel_posture(), (None, None));
    assert!(!e.otel_report(None, 15).listening);
}

#[test]
fn a_taken_port_is_a_posture_finding_not_a_crash() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut cfg = on();
    cfg.otel.port = port;
    let (_t, e) = engine_with(cfg);
    let _lock = e.acquire_lock().unwrap();
    let now = now_unix();
    e.perf_tick(now);
    let (listening, err) = e.otel_posture();
    assert!(listening.is_none());
    assert_eq!(err.as_ref().map(|x| x.0), Some(port));
    assert!(e.snapshot_state().otel.error.is_some());
    let f = e.run_watch();
    let down: Vec<&Finding> = f
        .iter()
        .filter(|f| f.kind == FindingKind::OtelUnavailable)
        .collect();
    assert_eq!(down.len(), 1, "{f:#?}");
    assert!(down[0].title.contains(&port.to_string()));
    // Freed: tried again after a minute, then listening.
    drop(taken);
    e.perf_tick(now + 1);
    assert!(e.otel_posture().0.is_none(), "not retried at once");
    e.perf_tick(now + RETRY_SECS + 1);
    assert_eq!(e.otel_posture(), (Some(port), None));
}

#[test]
fn a_slower_endpoint_names_its_workspace_commits_and_session() {
    let repo_t = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(repo_t.path()).unwrap();
    let dir = home.join("shop");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &home, &["init", "-q", "-b", "main"]);
    let first = commit(&dir, &home, "first");

    let procs = FakeProcesses::empty().with(4242, "node", Some(dir.clone()), 1);
    let (_t, e) = engine_with_procs(on(), procs);
    let _lock = e.acquire_lock().unwrap();
    let now = now_unix();
    let now_ms = now * 1000;
    // Mulchie saw `first` hours ago; an agent then committed `second`.
    e.check_head(&dir, None, "poll", now - 4 * 3600);
    let sid = {
        let mut sup = lock(&e.sup);
        let id = sup.tracker.register(
            now,
            4243,
            now,
            "claude",
            "claude".into(),
            Some("claude".into()),
            None,
            Some(dir.clone()),
            Some(dir.clone()),
        );
        sup.tracker.touch(&id, now);
        id
    };
    let second = commit(&dir, &home, "slow query");
    e.check_head(
        &dir,
        Some((sid.clone(), "claude".into())),
        "hook",
        now - 600,
    );

    // Twelve busy windows at ~100 ms, two to three hours ago.
    let w = 5 * 60 * 1000;
    for i in 0..12u64 {
        let end = now_ms - 3 * 3600 * 1000 + i * w + 1000;
        let ends: Vec<u64> = (0..30).map(|k| end + k * 1000).collect();
        e.otel_ingest(
            &batch("shop-web", 4242, "GET /api/orders", &ends, 100),
            now_ms,
        );
    }
    // A quiet endpoint has no baseline and says nothing.
    e.otel_ingest(
        &batch("shop-web", 4242, "GET /health", &[now_ms - 1000], 900),
        now_ms,
    );
    // The last ten minutes at ~420 ms.
    let ends: Vec<u64> = (0..40).map(|k| now_ms - 600_000 + k * 10_000).collect();
    e.otel_ingest(
        &batch("shop-web", 4242, "GET /api/orders", &ends, 420),
        now_ms,
    );

    e.perf_tick(now);
    let perf: Vec<Finding> = e
        .snapshot_state()
        .findings
        .into_iter()
        .filter(|f| f.category == FindingCategory::Performance)
        .collect();
    assert_eq!(perf.len(), 1, "{perf:#?}");
    let f = &perf[0];
    assert_eq!(f.kind, FindingKind::EndpointSlower);
    assert!(
        f.title.starts_with(
            "`GET /api/orders` in `shop-web` is 4.2× slower at p95 (420 ms vs 100 ms)"
        ),
        "{}",
        f.title
    );
    assert_eq!(f.evidence.workspace.as_deref(), Some(dir.as_path()));
    let ev = f.evidence.endpoint.as_ref().unwrap();
    assert_eq!(ev.from_revision.as_deref(), Some(first.as_str()));
    assert_eq!(ev.to_revision.as_deref(), Some(second.as_str()));
    assert_eq!(ev.commits.len(), 1);
    assert_eq!(ev.commits[0].session_id.as_deref(), Some(sid.as_str()));
    let a = ev.attribution.as_ref().unwrap();
    assert_eq!(a.via, "pid");
    assert_eq!(a.agent.as_deref(), Some("claude"));
    assert_eq!(a.head.as_deref(), Some(second.as_str()));
    assert_eq!(
        e.issues(None, false, Some(FindingKind::EndpointSlower))
            .issues
            .len(),
        1
    );
    let st = e.snapshot_state();
    assert_eq!(st.otel.endpoints_slower, 1);

    // The report: figures, baseline and attribution, with no git.
    let r = e.otel_report(Some("shop-web"), 15);
    let s = &r.services[0];
    assert_eq!(
        s.attribution.as_ref().unwrap().workspace.as_deref(),
        Some(dir.as_path())
    );
    let op = s
        .operations
        .iter()
        .find(|o| o.operation == "GET /api/orders")
        .unwrap();
    assert!(op.slower && !op.more_errors);
    assert_eq!(op.baseline.as_ref().unwrap().windows, 12);
    assert!(s
        .operations
        .iter()
        .any(|o| o.operation == "GET /health" && o.baseline.is_none() && !o.slower));

    // A watch run keeps it, and a build finding does not push it out.
    e.run_watch();
    assert_eq!(e.otel_findings().len(), 1);
    assert!(e
        .snapshot_state()
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::EndpointSlower));

    // Turned off: the finding goes.
    set(&e, safe());
    e.perf_tick(now + 1);
    assert!(!e
        .snapshot_state()
        .findings
        .iter()
        .any(|f| f.category == FindingCategory::Performance));
}

#[test]
fn a_revision_names_the_workspace_when_the_pid_cannot() {
    let repo_t = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(repo_t.path()).unwrap();
    let dir = home.join("api");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &home, &["init", "-q", "-b", "main"]);
    let head = commit(&dir, &home, "one");
    let (_t, e) = engine_with(on());
    let _lock = e.acquire_lock().unwrap();
    let now = now_unix();
    e.check_head(&dir, None, "poll", now - 60);
    let r = Resource {
        pid: Some(99_999),
        vcs_revision: Some(head[..12].to_string()),
        ..Resource::default()
    };
    let a = e.attribute_service(&r, now).unwrap();
    assert_eq!(a.via, "revision");
    assert_eq!(a.workspace.as_deref(), Some(dir.as_path()));
    assert!(e.attribute_service(&Resource::default(), now).is_none());
}
