//! The webhook through the engine: which issue events are posted, and that
//! the URL survives a config round trip without ever being shown. Fakes
//! only; nothing reaches the network.

use super::*;
use crate::config::{Config, LoadedConfig};
use crate::consumers::Severity;
use crate::issues::IssueStatus;
use crate::process::FakeProcesses;
use crate::webhook::tests::FakePost;
use std::path::Path;

const URL: &str = "https://hooks.example.com/services/T0/B0/SECRETPATH";

/// A process whose working directory (under home) is gone.
fn in_gone_dir(pid: u32, uid: u32, home: &Path) -> crate::process::ProcFacts {
    crate::process::ProcFacts {
        pid,
        ppid: 5,
        uid: Some(uid),
        name: "node".into(),
        exe: None,
        cmd: vec!["node".into(), "server.js".into()],
        cwd: Some(home.join("gone-worktree")),
        start_time: 1_000 + pid as u64,
    }
}

fn cfg(events: &[WebhookEvent]) -> Config {
    let mut c = Config::default();
    c.webhook.url = URL.into();
    c.webhook.events = events.to_vec();
    c.webhook.min_severity = Severity::Info;
    c
}

fn engine_at(paths: &crate::paths::Paths, c: Config, pid: u32, http: Arc<FakePost>) -> Engine {
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    procs.procs.push(in_gone_dir(pid, uid, &paths.home));
    Engine::with_processes(
        paths.clone(),
        LoadedConfig {
            config: c,
            error: None,
        },
        Arc::new(procs),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe))
    .with_webhook_http(http, Duration::ZERO)
}

fn wait_posts(h: &FakePost, n: usize) {
    let end = Instant::now() + Duration::from_secs(5);
    while h.count() < n && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Long enough for a post that should not come to show up.
    std::thread::sleep(Duration::from_millis(100));
}

fn bodies(h: &FakePost) -> Vec<serde_json::Value> {
    h.posts
        .lock()
        .unwrap()
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

#[test]
fn new_resolved_and_regressed_issues_are_posted_once_each() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let all = [
        WebhookEvent::NewIssue,
        WebhookEvent::Regressed,
        WebhookEvent::ResolvedByUser,
    ];
    let http = Arc::new(FakePost::new(vec![Ok(200)]));
    let e = engine_at(&paths, cfg(&all), 10, http.clone());
    // Not the lock holder, and no worker: nothing is queued.
    e.run_watch();
    assert_eq!(http.count(), 0);
    assert!(!e.webhook_status().sending);
    drop(e);

    // A fresh home for the holder, so the issue is new to its book.
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let e = engine_at(&paths, cfg(&all), 10, http.clone());
    let lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    e.run_watch();
    wait_posts(&http, 1);
    let b = bodies(&http);
    assert_eq!(b.len(), 1, "{b:#?}");
    assert_eq!(b[0]["event"], "new_issue");
    assert_eq!(b[0]["issue"]["kind"], "missing_working_directory");
    assert!(b[0].get("host").is_none());
    let id = b[0]["issue"]["id"].as_str().unwrap().to_string();
    assert_eq!(http.posts.lock().unwrap()[0].0, URL);

    // The same finding again is not new.
    e.run_watch();
    wait_posts(&http, 2);
    assert_eq!(http.count(), 1);

    // Resolving posts once; resolving again changes nothing.
    e.set_issue_status(&id, IssueStatus::Resolved).unwrap();
    e.set_issue_status(&id, IssueStatus::Resolved).unwrap();
    wait_posts(&http, 3);
    let b = bodies(&http);
    assert_eq!(b.len(), 2, "{b:#?}");
    assert_eq!(b[1]["event"], "resolved_by_user");
    assert_eq!(b[1]["issue"]["status"], "resolved");
    let s = e.webhook_status();
    assert_eq!((s.sent, s.failed, s.dropped), (2, 0, 0));
    assert_eq!(s.url, "https://hooks.example.com/[redacted]");
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();
    drop(lock);
    drop(e);

    // A new engine, the same command in a new process: it regressed.
    let e = engine_at(&paths, cfg(&all), 11, http.clone());
    let _lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    e.run_watch();
    wait_posts(&http, 3);
    let b = bodies(&http);
    assert_eq!(b.len(), 3, "{b:#?}");
    assert_eq!(b[2]["event"], "regressed");
    assert_eq!(b[2]["issue"]["regressions"], 1);
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();
}

#[test]
fn events_not_asked_for_and_low_severity_are_not_posted() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let http = Arc::new(FakePost::new(vec![Ok(200)]));
    let mut c = cfg(&[WebhookEvent::Regressed]);
    c.webhook.min_severity = Severity::High;
    let e = engine_at(&paths, c, 10, http.clone());
    let _lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    e.run_watch();
    let id = e.issues(None, false, None).issues[0].id.clone();
    e.set_issue_status(&id, IssueStatus::Resolved).unwrap();
    wait_posts(&http, 1);
    assert_eq!(http.count(), 0);
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();
}

#[test]
fn a_full_queue_drops_and_counts_and_clearing_the_url_stops_what_is_queued() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let http = Arc::new(FakePost::new(vec![Ok(200)]));
    let e = engine_at(&paths, cfg(&[WebhookEvent::NewIssue]), 10, http.clone());
    // A queue with no worker draining it.
    let (tx, _rx) = sync_channel::<IssueEvent>(webhook::QUEUE);
    *lock(&e.webhook.tx) = Some(tx);
    e.run_watch();
    let issue = e.issues(None, false, None).issues[0].clone();
    let many = vec![issue; webhook::QUEUE + 5];
    e.webhook_issues(WebhookEvent::NewIssue, &many, &[]);
    let s = e.webhook_status();
    assert_eq!(s.queued, webhook::QUEUE as u64);
    assert_eq!(s.dropped, 5);

    // With the URL cleared, a queued event is not sent.
    let mut c = e.config();
    c.webhook.url.clear();
    e.set_config(c).unwrap();
    assert!(e.webhook_send(&IssueEvent::test(), 1).is_err());
    assert_eq!(http.count(), 0);
}

#[test]
fn a_failing_webhook_reports_the_redacted_url_and_journals_once() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let http = Arc::new(FakePost::new(vec![Err(format!("could not reach {URL}"))]));
    let e = engine_at(&paths, cfg(&[WebhookEvent::NewIssue]), 10, http.clone());
    let _lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    assert!(e.webhook_send(&IssueEvent::test(), 3).is_err());
    assert!(e.webhook_send(&IssueEvent::test(), 3).is_err());
    assert_eq!(http.count(), 6, "three tries each");
    let s = e.webhook_status();
    assert_eq!(s.failed, 2);
    let err = s.last_error.unwrap();
    assert!(!err.contains("SECRETPATH"), "{err}");
    assert!(
        err.contains("https://hooks.example.com/[redacted]"),
        "{err}"
    );
    let notes: Vec<_> = e
        .snapshot_state()
        .actions
        .iter()
        .filter(|a| a.kind == "webhook")
        .cloned()
        .collect();
    assert_eq!(notes.len(), 1, "one journal line while failing");
    assert!(!notes[0].detail.contains("SECRETPATH"));
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();
}

#[test]
fn the_url_survives_config_round_trips_and_a_placeholder_never_reaches_disk() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let http = Arc::new(FakePost::default());
    let e = engine_at(&paths, Config::default(), 10, http);
    let mut c = e.config();
    c.webhook.url = URL.into();
    e.set_config(c).unwrap();
    let on_disk = || Config::load_from(&paths).config.webhook.url;
    assert_eq!(on_disk(), URL);

    // The state shows it redacted; the engine keeps the real one.
    let shown = e.snapshot_state().config;
    assert_eq!(shown.webhook.url, "https://hooks.example.com/[redacted]");
    assert_eq!(e.config().webhook.url, URL);

    // Writing back what was shown keeps it (PUT), as does a patch that
    // does not name it or names the redacted value.
    let mut back = shown.clone();
    back.watch.interval_secs += 1;
    e.set_config(back).unwrap();
    assert_eq!(on_disk(), URL);
    e.patch_config(&serde_json::json!({"webhook": {"min_severity": "high"}}))
        .unwrap();
    assert_eq!(on_disk(), URL);
    e.patch_config(&serde_json::json!({"webhook": {"url": shown.webhook.url}}))
        .unwrap();
    assert_eq!(on_disk(), URL);

    // A redacted value for another host, or with none saved, is refused.
    for other in [
        "https://other.example.com/[redacted]",
        "http://127.0.0.1:9/[redacted]",
    ] {
        let err = e
            .patch_config(&serde_json::json!({"webhook": {"url": other}}))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
        assert_eq!(on_disk(), URL);
    }

    // A new URL replaces it; an empty one, or removing the section, turns
    // it off.
    let new = "https://hooks.example.com/services/NEWSECRET";
    e.patch_config(&serde_json::json!({"webhook": {"url": new}}))
        .unwrap();
    assert_eq!(on_disk(), new);
    e.patch_config(&serde_json::json!({"webhook": {"url": ""}}))
        .unwrap();
    assert_eq!(on_disk(), "");
    let err = e
        .patch_config(&serde_json::json!({"webhook": {"url": shown.webhook.url}}))
        .unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)));
    assert_eq!(on_disk(), "");
    e.patch_config(&serde_json::json!({"webhook": {"url": URL}}))
        .unwrap();
    e.patch_config(&serde_json::json!({"webhook": null}))
        .unwrap();
    assert_eq!(on_disk(), "");

    // Bad URLs are refused without being repeated.
    let err = e
        .patch_config(&serde_json::json!({"webhook": {"url": "http://example.com/SECRETPATH"}}))
        .unwrap_err();
    assert!(!err.to_string().contains("SECRETPATH"), "{err}");

    // Saving a placeholder directly is refused too.
    let mut p = Config::default();
    p.webhook.url = "https://hooks.example.com/[redacted]".into();
    assert!(p.validate().is_ok(), "a config read back validates");
    assert!(p.save_to(&paths, 0).is_err());
    assert_eq!(on_disk(), "");
}

#[cfg(feature = "network")]
#[test]
fn test_events_are_refused_without_a_url_or_off_the_lock_holder() {
    let t = tempfile::tempdir().unwrap();
    let paths = crate::paths::Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let http = Arc::new(FakePost::new(vec![Ok(204)]));
    let e = engine_at(&paths, Config::default(), 10, http.clone());
    assert_eq!(e.webhook_test().unwrap_err().1, "webhook_off");
    let mut c = e.config();
    c.webhook.url = URL.into();
    e.set_config(c).unwrap();
    assert_eq!(e.webhook_test().unwrap_err().1, "not_lock_holder");
    let _lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    assert_eq!(e.webhook_test(), Ok(204));
    let b = bodies(&http);
    assert_eq!(b[0]["event"], "test");
    assert_eq!(e.webhook_status().last_event, Some(WebhookEvent::Test));
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();
}
