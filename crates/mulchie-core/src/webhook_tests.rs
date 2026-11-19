use super::*;
use crate::commits::CommitEntry;
use crate::consumers::Severity;
use crate::issues::IssueStatus;
use crate::timings::PerfEvidence;
use crate::watch::{Evidence, FindingKind};
use std::path::PathBuf;
use std::sync::Mutex;

#[test]
fn urls_take_https_anywhere_and_http_only_to_loopback() {
    for ok in [
        "https://hooks.slack.com/services/T0/B0/XXXX",
        "https://example.com",
        "https://example.com:8443/hook?x=1",
        "http://127.0.0.1:9000/hook",
        "http://127.8.9.10/hook",
        "http://localhost:3000/hook",
        "http://[::1]:8080/hook",
        "https://[2001:db8::1]/hook",
    ] {
        assert!(parse_url(ok).is_ok(), "{ok}: {:?}", parse_url(ok));
    }
    for bad in [
        "",
        "ftp://example.com/x",
        "HTTPS://example.com/x",
        "http://example.com/hook",
        "http://10.0.0.1/hook",
        "http://[2001:db8::1]/hook",
        "http://127.0.0.1.evil.com/hook",
        "http://localhost.evil.com/hook",
        "https://user:pass@example.com/hook",
        "https://token@example.com/hook",
        "https:///hook",
        "https://exa mple.com/hook",
        "https://example.com/ho ok",
        "https://example.com\\@evil.com/",
        "https://example.com:0/x",
        "https://example.com:99999/x",
        "https://example.com:abc/x",
        "https://example.com/[redacted]",
        "https://a:b:80/x",
        "https://[::1/x",
        "https://[nothost]/x",
        "https://example.com/x\n",
    ] {
        assert!(parse_url(bad).is_err(), "{bad:?} should be refused");
    }
    let long = format!("https://example.com/{}", "a".repeat(3000));
    assert!(parse_url(&long).is_err());
}

#[test]
fn errors_never_repeat_the_url() {
    for bad in [
        "http://example.com/SECRETPATH",
        "https://user:SECRETPATH@example.com/",
        "https://example.com/SECRETPATH x",
    ] {
        let e = url_problem(bad).unwrap();
        assert!(!e.contains("SECRETPATH"), "{e}");
    }
}

#[test]
fn redaction_keeps_scheme_host_and_port_only() {
    assert_eq!(
        redact_url("https://hooks.slack.com/services/T0/B0/XXXX"),
        "https://hooks.slack.com/[redacted]"
    );
    assert_eq!(
        redact_url("http://127.0.0.1:9000/hook?key=abc"),
        "http://127.0.0.1:9000/[redacted]"
    );
    assert_eq!(
        redact_url("https://example.com"),
        "https://example.com/[redacted]"
    );
    assert_eq!(redact_url(""), "");
    assert_eq!(redact_url("not a url SECRET"), "[invalid url]");
    let r = redact_url("https://hooks.slack.com/services/T0/B0/XXXX");
    assert!(is_redacted(&r));
    assert_eq!(redact_url(&r), r, "redacting twice changes nothing");
    assert!(!is_redacted("https://hooks.slack.com/services/T0/B0/XXXX"));
    assert!(!is_redacted("https://hooks.slack.com/x/[redacted]"));
    assert!(
        !is_redacted("http://example.com/[redacted]"),
        "not loopback"
    );
    // A redacted value validates (a config read back holds one).
    assert_eq!(url_problem(&r), None);
}

#[test]
fn a_redacted_value_keeps_only_the_url_it_stands_for() {
    let saved = "https://hooks.slack.com/services/T0/B0/XXXX";
    let mut same = redact_url(saved);
    keep_redacted(&mut same, saved).unwrap();
    assert_eq!(same, saved);

    let mut other = "https://other.example.com/[redacted]".to_string();
    assert!(keep_redacted(&mut other, saved).is_err());
    let mut none = redact_url(saved);
    let e = keep_redacted(&mut none, "").unwrap_err();
    assert!(!e.contains("XXXX"), "{e}");

    let mut new = "https://new.example.com/hook".to_string();
    keep_redacted(&mut new, saved).unwrap();
    assert_eq!(new, "https://new.example.com/hook");
    let mut empty = String::new();
    keep_redacted(&mut empty, saved).unwrap();
    assert_eq!(empty, "");
}

#[test]
fn scrub_removes_the_url_and_its_path() {
    let url = "https://hooks.slack.com/services/T0/B0/XXXX";
    let t = scrub(
        &format!("error sending to {url}; path /services/T0/B0/XXXX"),
        url,
    );
    assert!(!t.contains("XXXX"), "{t}");
    assert!(t.contains("https://hooks.slack.com/[redacted]"), "{t}");
}

fn issue(kind: FindingKind, severity: Severity) -> Issue {
    Issue {
        id: "0123456789abcdef".into(),
        kind,
        category: kind.category(),
        severity,
        title: "Slower build: cargo build --token=abcd1234efgh5678ijkl9012mnop".into(),
        culprit: "cargo build --password hunter2".into(),
        workspace: Some(PathBuf::from("/home/u/code/shop")),
        first_seen_unix: 100,
        last_seen_unix: 200,
        occurrences: 3,
        status: IssueStatus::Open,
        status_unix: None,
        regressions: 1,
        active: true,
        finding_id: "build_slower:1".into(),
    }
}

fn commit(head: &str, session: Option<&str>) -> CommitEntry {
    CommitEntry {
        at_unix: 150,
        workspace: PathBuf::from("/home/u/code/shop"),
        branch: Some("main".into()),
        head: head.into(),
        previous_head: None,
        session_id: session.map(String::from),
        agent: session.map(|_| "claude".to_string()),
        via: "hook".into(),
    }
}

fn slow_build() -> Finding {
    let mut f = Finding {
        id: "build_slower:1".into(),
        kind: FindingKind::BuildSlower,
        category: FindingCategory::Performance,
        severity: Severity::Medium,
        title: "Slower build".into(),
        why: String::new(),
        evidence: Evidence::default(),
        suggested: Vec::new(),
        first_seen_unix: 100,
        last_seen_unix: 100,
        dismissed: false,
        dismissed_until: None,
    };
    f.evidence.perf = Some(Box::new(PerfEvidence {
        command: "cargo build".into(),
        baseline_ms: 1000,
        baseline_runs: 5,
        recent_ms: vec![3000, 3100],
        ratio: 3.0,
        from_head: Some("aaa".into()),
        to_head: Some("ccc".into()),
        commits: vec![commit("bbb", Some("s-1-2")), commit("ccc", None)],
        commits_truncated: false,
    }));
    f
}

#[test]
fn the_payload_holds_the_issue_redacted_and_the_commit_range() {
    let f = slow_build();
    let ev = IssueEvent::new(
        WebhookEvent::NewIssue,
        issue(FindingKind::BuildSlower, Severity::Medium),
        Some(&f),
    );
    let v = payload(&ev, None);
    assert_eq!(v["event"], "new_issue");
    let i = &v["issue"];
    assert_eq!(i["id"], "0123456789abcdef");
    assert_eq!(i["kind"], "build_slower");
    assert_eq!(i["category"], "performance");
    assert_eq!(i["severity"], "medium");
    assert_eq!(i["occurrences"], 3);
    assert_eq!(i["status"], "open");
    assert_eq!(i["regressions"], 1);
    assert_eq!(i["first_seen_unix"], 100);
    assert_eq!(i["last_seen_unix"], 200);
    assert_eq!(i["workspace"], "/home/u/code/shop");
    let text = v.to_string();
    assert!(!text.contains("abcd1234efgh5678ijkl9012mnop"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(v.get("host").is_none(), "no host unless asked");
    assert_eq!(v["commits"]["from"], "aaa");
    assert_eq!(v["commits"]["to"], "ccc");
    assert_eq!(v["commits"]["commits"][0]["session_id"], "s-1-2");
    assert_eq!(v["commits"]["commits"][0]["agent"], "claude");
    assert_eq!(v["commits"]["commits"].as_array().unwrap().len(), 2);
    // Nothing else: the keys are exactly these.
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
    let ikeys = i.as_object().unwrap().len();
    assert_eq!(ikeys, 12);

    let with_host = payload(&ev, Some("box"));
    assert_eq!(with_host["host"], "box");
}

#[test]
fn only_performance_findings_carry_a_commit_range() {
    let mut f = slow_build();
    f.kind = FindingKind::OrphanedProcess;
    assert_eq!(commit_range(&f), None);
    let mut f = slow_build();
    f.evidence.perf.as_mut().unwrap().commits =
        (0..30).map(|n| commit(&format!("c{n}"), None)).collect();
    let r = commit_range(&f).unwrap();
    assert_eq!(r.commits.len(), 20);
    assert!(r.truncated);
    assert_eq!(r.commits.last().unwrap().sha, "c29", "the newest are kept");
}

#[test]
fn slack_gets_one_line_of_text() {
    let ev = IssueEvent::new(
        WebhookEvent::Regressed,
        issue(FindingKind::BuildSlower, Severity::High),
        Some(&slow_build()),
    );
    let b: Value = serde_json::from_slice(&body(WebhookFormat::Slack, &ev, Some("box"))).unwrap();
    let t = b["text"].as_str().unwrap();
    assert_eq!(b.as_object().unwrap().len(), 1);
    assert!(!t.contains('\n'));
    assert!(
        t.starts_with("Mulchie on box: issue came back (high) in shop:"),
        "{t}"
    );
    assert!(!t.contains("hunter2"), "{t}");
    let mut evil = issue(FindingKind::BuildSlower, Severity::High);
    evil.title = "<!channel> & co".into();
    let t = slack_text(&IssueEvent::new(WebhookEvent::NewIssue, evil, None), None);
    assert!(t.contains("&lt;!channel&gt; &amp; co"), "{t}");
    let t = slack_text(&IssueEvent::test(), None);
    assert!(t.contains("test event"));
}

#[test]
fn events_and_severity_filter_what_is_sent() {
    let mut cfg = crate::config::WebhookConfig::default();
    let medium = issue(FindingKind::BuildSlower, Severity::Medium);
    let low = issue(FindingKind::BuildSlower, Severity::Low);
    assert!(
        !wanted(&cfg, WebhookEvent::NewIssue, &medium),
        "no url, nothing"
    );
    cfg.url = "https://example.com/hook".into();
    assert!(wanted(&cfg, WebhookEvent::NewIssue, &medium));
    assert!(wanted(&cfg, WebhookEvent::Regressed, &medium));
    assert!(
        !wanted(&cfg, WebhookEvent::ResolvedByUser, &medium),
        "not by default"
    );
    assert!(!wanted(&cfg, WebhookEvent::NewIssue, &low));
    cfg.min_severity = Severity::Info;
    assert!(wanted(&cfg, WebhookEvent::NewIssue, &low));
}

/// Replies in order, then the last one again; records what was posted.
#[derive(Default)]
pub(crate) struct FakePost {
    pub replies: Mutex<Vec<Result<u16, String>>>,
    pub posts: Mutex<Vec<(String, Vec<u8>)>>,
}

impl FakePost {
    pub(crate) fn new(replies: Vec<Result<u16, String>>) -> Self {
        Self {
            replies: Mutex::new(replies),
            posts: Mutex::new(Vec::new()),
        }
    }
    pub(crate) fn count(&self) -> usize {
        self.posts.lock().unwrap().len()
    }
}

impl WebhookHttp for FakePost {
    fn post(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        self.posts
            .lock()
            .unwrap()
            .push((url.to_string(), body.to_vec()));
        let mut r = self.replies.lock().unwrap();
        if r.len() > 1 {
            r.remove(0)
        } else {
            r.first().cloned().unwrap_or(Ok(200))
        }
    }
}

#[test]
fn delivery_retries_only_what_may_succeed_later() {
    let url = "https://hooks.example.com/services/SECRETPATH";
    let waits = Mutex::new(Vec::new());
    let sleep = |d: Duration| waits.lock().unwrap().push(d);
    let base = Duration::from_millis(5);

    let h = FakePost::new(vec![Ok(500), Err("timed out".into()), Ok(204)]);
    assert_eq!(deliver(&h, url, b"{}", ATTEMPTS, base, &sleep), Ok(204));
    assert_eq!(h.count(), 3);
    assert_eq!(*waits.lock().unwrap(), vec![base, base * 2]);

    let h = FakePost::new(vec![Ok(503)]);
    let e = deliver(&h, url, b"{}", ATTEMPTS, base, &sleep).unwrap_err();
    assert_eq!(h.count(), 3, "at most two retries");
    assert!(e.contains("HTTP 503") && e.contains("tried 3 times"), "{e}");

    for (status, what) in [(404, "HTTP 404"), (301, "redirect"), (401, "HTTP 401")] {
        let h = FakePost::new(vec![Ok(status)]);
        let e = deliver(&h, url, b"{}", ATTEMPTS, base, &sleep).unwrap_err();
        assert_eq!(h.count(), 1, "{status} is not tried again");
        assert!(e.contains(what), "{e}");
        assert!(!e.contains("SECRETPATH"), "{e}");
    }

    // A client error that echoes the URL is scrubbed.
    let h = FakePost::new(vec![Err(format!("could not reach {url}"))]);
    let e = deliver(&h, url, b"{}", 2, base, &sleep).unwrap_err();
    assert!(!e.contains("SECRETPATH"), "{e}");
    assert!(e.contains("https://hooks.example.com/[redacted]"), "{e}");

    // A URL that does not check out is never posted to.
    let h = FakePost::new(vec![Ok(200)]);
    assert!(deliver(&h, "http://example.com/x", b"{}", 3, base, &sleep).is_err());
    assert!(deliver(&h, &redact_url(url), b"{}", 3, base, &sleep).is_err());
    assert_eq!(h.count(), 0);
}

#[test]
fn without_network_support_nothing_is_sent() {
    let e = NoNetwork.post("https://example.com/x", b"{}").unwrap_err();
    assert!(e.contains("no network support"));
}

/// The real client against a loopback server: one POST with a JSON body,
/// and a redirect is not followed.
#[cfg(feature = "network")]
#[test]
fn the_real_client_posts_to_a_loopback_server_and_follows_no_redirect() {
    use std::io::{Read, Write};
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for status in ["204 No Content", "302 Found"] {
            let (mut s, _) = srv.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Headers, then the body by Content-Length.
            loop {
                let n = s.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len || n == 0 {
                        break;
                    }
                }
            }
            seen.push(String::from_utf8_lossy(&buf).to_string());
            let extra = if status.starts_with("302") {
                "Location: http://127.0.0.1:1/elsewhere\r\n"
            } else {
                ""
            };
            write!(
                s,
                "HTTP/1.1 {status}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
        seen
    });
    let url = format!("http://127.0.0.1:{port}/hook/SECRETPATH");
    let p = Post::new();
    let body = br#"{"event":"test"}"#;
    assert_eq!(p.post(&url, body), Ok(204));
    let e = deliver(&p, &url, body, 3, Duration::ZERO, &|_| {}).unwrap_err();
    assert!(e.contains("redirect"), "{e}");
    let seen = handle.join().unwrap();
    assert_eq!(seen.len(), 2, "the redirect was not followed");
    assert!(
        seen[0].starts_with("POST /hook/SECRETPATH HTTP/1.1"),
        "{}",
        seen[0]
    );
    assert!(seen[0]
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    assert!(seen[0].ends_with(r#"{"event":"test"}"#), "{}", seen[0]);
    // Not to a host other than loopback over plain http, whatever the
    // caller passes.
    assert!(p.post("http://example.com/x", body).is_err());
}
