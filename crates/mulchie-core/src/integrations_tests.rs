//! Providers against recorded replies shaped like the real APIs, token
//! files, the request guard, attribution and the finding. Nothing here
//! reaches the network: every request goes to [`FakeHttp`].

use super::*;
use crate::config::{PostHogIntegration, SentryIntegration, VercelIntegration};
use serde_json::json;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

/// A fake provider API: GET answers by URL, POST by the HogQL text it
/// carries (first match of a key in it). Every call is recorded with the
/// token it carried.
#[derive(Default)]
pub(crate) struct FakeHttp {
    pub gets: HashMap<String, HttpReply>,
    pub posts: Vec<(String, HttpReply)>,
    pub calls: Mutex<Vec<String>>,
    pub tokens: Mutex<Vec<String>>,
    /// Every call fails with this error text.
    pub fail: Option<String>,
}

pub(crate) fn reply(status: u16, v: Value) -> HttpReply {
    HttpReply {
        status,
        body: serde_json::to_vec(&v).unwrap(),
    }
}

impl AuthHttp for FakeHttp {
    fn get(&self, url: &str, token: &Secret, _max: usize) -> Result<HttpReply, String> {
        self.calls.lock().unwrap().push(format!("GET {url}"));
        self.tokens.lock().unwrap().push(token.expose().to_string());
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.gets
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no fixture for {url}"))
    }
    fn post_json(
        &self,
        url: &str,
        token: &Secret,
        body: &[u8],
        _max: usize,
    ) -> Result<HttpReply, String> {
        self.calls.lock().unwrap().push(format!("POST {url}"));
        self.tokens.lock().unwrap().push(token.expose().to_string());
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        let v: Value = serde_json::from_slice(body).unwrap();
        let q = v["query"]["query"].as_str().unwrap_or_default();
        self.posts
            .iter()
            .find(|(k, _)| q.contains(k.as_str()))
            .map(|(_, r)| r.clone())
            .ok_or_else(|| "no fixture for the query".to_string())
    }
}

pub(crate) fn sha(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

pub(crate) fn sentry_cfg() -> SentryIntegration {
    SentryIntegration {
        enabled: true,
        org: "acme".into(),
        project: "web".into(),
        ..SentryIntegration::default()
    }
}

/// Five releases a day apart (`a`..`e`, oldest `a`), with p95 of 120 ms
/// for the first four and 380 ms for the newest, as Sentry returns them.
pub(crate) fn sentry_http() -> FakeHttp {
    let cfg = sentry_cfg();
    let mut h = FakeHttp::default();
    let rel = |c: char, day: u32, commit: bool| {
        json!({
            "id": 1000 + day,
            "version": if commit { format!("web@{}", &sha(c)[..12]) } else { sha(c) },
            "shortVersion": &sha(c)[..12],
            "dateCreated": format!("2026-11-{day:02}T10:00:00.000000Z"),
            "dateReleased": null,
            "lastCommit": if commit { json!({"id": sha(c), "message": "x"}) } else { Value::Null },
            "lastDeploy": null,
            "newGroups": 0,
            "projects": [{"id": 1, "slug": "web", "name": "web"}],
            "ref": null,
            "url": null
        })
    };
    // Newest first, as the API lists them.
    h.gets.insert(
        sentry::releases_url(&cfg),
        reply(
            200,
            json!([
                rel('e', 5, true),
                rel('d', 4, false),
                rel('c', 3, false),
                rel('b', 2, false),
                rel('a', 1, false)
            ]),
        ),
    );
    let row = |v: String, p: f64| json!({"release": v, "p95(span.duration)": p, "count()": 500});
    h.gets.insert(
        sentry::events_url(&cfg),
        reply(
            200,
            json!({
                "data": [
                    row(format!("web@{}", &sha('e')[..12]), 380.0),
                    row(sha('d'), 121.0),
                    row(sha('c'), 119.0),
                    row(sha('b'), 120.0),
                    row(sha('a'), 120.0),
                ],
                "meta": {"fields": {"release": "string", "p95(span.duration)": "duration", "count()": "integer"}}
            }),
        ),
    );
    h
}

fn tok(s: &str) -> Secret {
    Secret::new(s).unwrap()
}

// ------------------------------------------------------------ providers

#[test]
fn vercel_reads_production_deployments_and_build_duration() {
    let cfg = VercelIntegration {
        enabled: true,
        project: "prj_123".into(),
        team: "team_9".into(),
        workspace: None,
    };
    let url = vercel::deployments_url(&cfg);
    assert_eq!(
        url,
        "https://api.vercel.com/v7/deployments?projectId=prj_123&target=production&state=READY&limit=40&teamId=team_9"
    );
    let mut h = FakeHttp::default();
    h.gets.insert(
        url,
        reply(
            200,
            json!({
                "deployments": [
                    {
                        "uid": "dpl_new", "name": "web", "url": "web-1.vercel.app",
                        "created": 1_700_000_100_000u64, "createdAt": 1_700_000_100_000u64,
                        "buildingAt": 1_700_000_101_000u64, "ready": 1_700_000_161_000u64,
                        "state": "READY", "readyState": "READY", "target": "production",
                        "meta": {"githubCommitSha": sha('b'), "githubCommitRef": "main"},
                        "creator": {"uid": "u1"}, "inspectorUrl": "https://vercel.com/x", "type": "LAMBDAS", "projectId": "prj_123"
                    },
                    {
                        "uid": "dpl_preview", "created": 1_700_000_050_000u64, "readyState": "READY",
                        "target": null, "meta": {"githubCommitSha": sha('c')}
                    },
                    {
                        "uid": "dpl_failed", "created": 1_700_000_040_000u64, "readyState": "ERROR",
                        "target": "production", "meta": {}
                    },
                    {
                        "uid": "dpl_old", "created": 1_700_000_000_000u64,
                        "buildingAt": 1_700_000_001_000u64, "ready": 1_700_000_031_000u64,
                        "readyState": "READY", "target": "production",
                        "meta": {"githubCommitSha": sha('a')}
                    }
                ],
                "pagination": {"count": 4, "next": null, "prev": null}
            }),
        ),
    );
    let src = Provider::Vercel.source(&IntegrationsConfig {
        vercel: cfg,
        ..IntegrationsConfig::default()
    });
    let d = poll(&*src, &h, &tok("vc_token"), 0).unwrap();
    assert_eq!(
        d.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        vec!["dpl_new", "dpl_old"],
        "production and READY only, newest first"
    );
    assert_eq!(d[0].sha.as_deref(), Some(sha('b').as_str()));
    assert_eq!(d[0].at_unix, 1_700_000_100);
    assert_eq!(d[0].metric("build_duration").unwrap().value_ms, 60_000.0);
    assert_eq!(d[1].metric("build_duration").unwrap().value_ms, 30_000.0);
    assert_eq!(*h.tokens.lock().unwrap(), vec!["vc_token".to_string()]);
}

#[test]
fn sentry_reads_releases_and_p95_per_release() {
    let h = sentry_http();
    let cfg = sentry_cfg();
    assert!(sentry::events_url(&cfg).contains("dataset=spans"));
    assert!(sentry::events_url(&cfg).contains("field=p95%28span.duration%29"));
    assert!(sentry::events_url(&cfg).contains("query=is_transaction%3Atrue"));
    let src = sentry::Sentry(cfg);
    let d = poll(&src, &h, &tok("sntrys_x"), 0).unwrap();
    assert_eq!(d.len(), 5);
    assert_eq!(d[0].id, format!("web@{}", &sha('e')[..12]));
    assert_eq!(
        d[0].sha.as_deref(),
        Some(sha('e').as_str()),
        "lastCommit.id"
    );
    assert_eq!(
        d[1].sha.as_deref(),
        Some(sha('d').as_str()),
        "the version is a SHA"
    );
    assert_eq!(d[0].metric("p95").unwrap().value_ms, 380.0);
    assert_eq!(d[0].metric("p95").unwrap().samples, Some(500));
    assert_eq!(d[4].at_unix, parse_time("2026-11-01T10:00:00Z").unwrap());
    assert_eq!(h.calls.lock().unwrap().len(), 2);
}

#[test]
fn sentry_skips_releases_with_too_few_transactions() {
    let v = json!({"data": [{"release": "r1", "p95(span.duration)": 900.0, "count()": 3}]});
    let mut d = vec![Deploy {
        provider: Provider::Sentry,
        id: "r1".into(),
        sha: None,
        at_unix: 1,
        metrics: vec![],
    }];
    let mut h = FakeHttp::default();
    h.gets
        .insert(sentry::events_url(&sentry_cfg()), reply(200, v));
    sentry::Sentry(sentry_cfg())
        .metrics(&h, &tok("t"), &mut d)
        .unwrap();
    assert!(d[0].metrics.is_empty());
}

#[test]
fn posthog_reads_web_vitals_per_release_with_a_hogql_query() {
    let cfg = PostHogIntegration {
        enabled: true,
        project: "12345".into(),
        ..PostHogIntegration::default()
    };
    assert_eq!(
        posthog::query_url(&cfg),
        "https://us.posthog.com/api/projects/12345/query/"
    );
    let q = posthog::releases_query(&cfg).unwrap();
    assert!(q.contains("properties.$app_version") && q.contains("LIMIT 50"));
    let mut h = FakeHttp::default();
    h.posts.push((
        "first_seen".into(),
        reply(
            200,
            json!({
                "columns": ["release", "first_seen"],
                "types": [["release", "String"], ["first_seen", "Int64"]],
                "results": [[sha('b'), 1_700_100_000], ["1.4.0", 1_700_000_000]],
                "hasMore": false
            }),
        ),
    ));
    h.posts.push((
        "quantile".into(),
        reply(
            200,
            json!({
                "columns": ["release", "lcp", "lcp_n", "inp", "inp_n"],
                "results": [[sha('b'), 2400.5, 300, 180.0, 5], ["1.4.0", 1800.0, 250, 150.0, 90]],
                "hasMore": false
            }),
        ),
    ));
    let d = poll(&posthog::PostHog(cfg), &h, &tok("phx_x"), 0).unwrap();
    assert_eq!(d[0].sha.as_deref(), Some(sha('b').as_str()));
    assert_eq!(d[1].sha, None, "a version that is not a SHA");
    assert_eq!(d[0].metric("lcp_p75").unwrap().value_ms, 2400.5);
    assert!(
        d[0].metric("inp_p75").is_none(),
        "5 measurements is too few"
    );
    assert_eq!(d[1].metric("inp_p75").unwrap().value_ms, 150.0);
    assert!(h
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|c| c.starts_with("POST https://us.posthog.com/")));
}

#[test]
fn a_posthog_property_that_is_not_plain_is_never_put_into_hogql() {
    for bad in ["x) OR 1=1 --", "a.b", "", "$", "a b", "'q'"] {
        let cfg = PostHogIntegration {
            release_property: bad.into(),
            ..PostHogIntegration::default()
        };
        assert!(posthog::releases_query(&cfg).is_none(), "{bad}");
        assert!(posthog::vitals_query(&cfg).is_none(), "{bad}");
    }
}

// ------------------------------------------------------------ helpers

#[test]
fn release_names_give_their_sha() {
    assert_eq!(sha_in(&sha('a')), Some(sha('a')));
    assert_eq!(sha_in("web@ABCDEF1"), Some("abcdef1".into()));
    assert_eq!(sha_in("1.2.3+deadbeef"), Some("deadbeef".into()));
    assert_eq!(sha_in("1.2.3"), None);
    assert_eq!(sha_in("abc"), None);
    assert_eq!(sha_in("release-2026"), None);
}

#[test]
fn times_parse_as_utc() {
    assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_time("2026-11-19T12:30:15Z"), Some(1_795_091_415));
    assert_eq!(
        parse_time("2026-11-19T12:30:15.123456Z"),
        parse_time("2026-11-19T12:30:15Z")
    );
    assert_eq!(parse_time("2026-11-19T13:30:15+01:00"), Some(1_795_091_415));
    assert_eq!(parse_time("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    assert_eq!(parse_time("nope"), None);
    assert_eq!(parse_time("2026-13-01T00:00:00Z"), None);
    assert_eq!(parse_time("2026-11-19T12:30:15 junk"), None);
}

#[test]
fn backoff_doubles_up_to_a_day() {
    assert_eq!(next_poll(0, 1800, 0), 1800);
    assert_eq!(next_poll(0, 1800, 1), 3600);
    assert_eq!(next_poll(0, 1800, 3), 14_400);
    assert_eq!(next_poll(0, 1800, 30), MAX_BACKOFF_SECS);
}

// ------------------------------------------------------- request guard

#[test]
fn requests_go_only_to_the_providers_host_over_https() {
    let h = FakeHttp::default();
    for url in [
        "http://api.vercel.com/v7/deployments",
        "https://evil.example/v7/deployments",
        "https://api.vercel.com.evil.example/x",
        "https://user@api.vercel.com/x",
        "https://api.vercel.com:8443/x",
    ] {
        let e = fetch_json(&h, "api.vercel.com", url, &tok("t"), None).unwrap_err();
        assert!(e.contains("refused"), "{url}: {e}");
    }
    assert!(h.calls.lock().unwrap().is_empty(), "nothing was sent");
    assert_eq!(https_host("https://sentry.io/api/0/"), Some("sentry.io"));
}

#[test]
fn redirects_are_errors_and_bodies_never_reach_errors() {
    let secret = "SECRET-TOKEN-1234567890";
    let mut h = FakeHttp::default();
    let url = "https://api.vercel.com/x";
    h.gets.insert(
        url.into(),
        reply(302, json!({"location": "https://elsewhere.example"})),
    );
    let e = fetch_json(&h, "api.vercel.com", url, &tok(secret), None).unwrap_err();
    assert!(e.contains("redirect"), "{e}");
    h.gets.insert(
        url.into(),
        reply(401, json!({"error": format!("bad token {secret}")})),
    );
    let e = fetch_json(&h, "api.vercel.com", url, &tok(secret), None).unwrap_err();
    assert!(
        e.contains("refused the token") && !e.contains(secret),
        "{e}"
    );
    // An error from the client itself that echoes the token is scrubbed.
    let h = FakeHttp {
        fail: Some(format!("header Authorization: Bearer {secret} rejected")),
        ..FakeHttp::default()
    };
    let e = fetch_json(&h, "api.vercel.com", url, &tok(secret), None).unwrap_err();
    assert!(!e.contains(secret) && e.contains("[redacted]"), "{e}");
}

#[test]
fn secrets_do_not_print() {
    let s = tok("abc-very-secret");
    assert_eq!(format!("{s:?}"), "Secret([redacted])");
    assert!(Secret::new("").is_err());
    assert!(Secret::new("two words").is_err());
    assert!(Secret::new(&"x".repeat(MAX_TOKEN + 1)).is_err());
    assert_eq!(Secret::new("  t0k\n").unwrap().expose(), "t0k");
}

#[test]
fn a_build_without_network_says_so() {
    let e = fetch_json(
        &NoNetwork,
        "sentry.io",
        "https://sentry.io/x",
        &tok("tok-123456"),
        None,
    )
    .unwrap_err();
    assert!(e.contains(NO_NETWORK), "{e}");
}

// ---------------------------------------------------------- token files

fn paths() -> (tempfile::TempDir, Paths) {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::under(t.path());
    (t, p)
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn tokens_are_private_files_and_read_back() {
    let (_t, p) = paths();
    save_token(&p, Provider::Sentry, &tok("sntrys_abc")).unwrap();
    assert_eq!(mode(&token_dir(&p)), 0o700);
    assert_eq!(mode(&token_path(&p, Provider::Sentry)), 0o600);
    assert_eq!(
        read_token(&p, Provider::Sentry).unwrap().expose(),
        "sntrys_abc"
    );
    assert!(token_present(&p, Provider::Sentry));
    assert!(!token_present(&p, Provider::Vercel));
    // Replaced whole.
    save_token(&p, Provider::Sentry, &tok("sntrys_new")).unwrap();
    assert_eq!(
        read_token(&p, Provider::Sentry).unwrap().expose(),
        "sntrys_new"
    );
}

#[test]
fn loose_or_odd_token_files_are_refused() {
    let (_t, p) = paths();
    save_token(&p, Provider::Vercel, &tok("v1")).unwrap();
    let f = token_path(&p, Provider::Vercel);
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        read_token(&p, Provider::Vercel).is_err(),
        "readable by others"
    );
    std::fs::remove_file(&f).unwrap();
    // A symbolic link to a private file is not followed.
    let elsewhere = p.config_dir.join("other");
    std::fs::write(&elsewhere, "stolen").unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &f).unwrap();
    assert!(read_token(&p, Provider::Vercel).is_err(), "symlink");
    assert!(save_token(&p, Provider::Vercel, &tok("v2")).is_err());
    std::fs::remove_file(&f).unwrap();
    // A directory in its place.
    std::fs::create_dir(&f).unwrap();
    assert!(read_token(&p, Provider::Vercel).is_err(), "directory");
    std::fs::remove_dir(&f).unwrap();
    // A FIFO is never opened for a blocking read.
    let c = std::ffi::CString::new(f.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(read_token(&p, Provider::Vercel).is_err(), "fifo");
    // A loose directory.
    std::fs::remove_file(&f).unwrap();
    save_token(&p, Provider::Vercel, &tok("v3")).unwrap();
    std::fs::set_permissions(token_dir(&p), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(read_token(&p, Provider::Vercel).is_err(), "loose directory");
}

#[test]
fn removing_deletes_only_the_regular_token_file() {
    let (_t, p) = paths();
    assert!(!remove_token(&p, Provider::Posthog).unwrap(), "none yet");
    save_token(&p, Provider::Posthog, &tok("phx")).unwrap();
    save_token(&p, Provider::Sentry, &tok("snt")).unwrap();
    assert!(remove_token(&p, Provider::Posthog).unwrap());
    assert!(!token_path(&p, Provider::Posthog).exists());
    assert!(token_path(&p, Provider::Sentry).exists(), "others are kept");
    // A symbolic link in its place is refused and left alone, and so is
    // what it points to.
    let target = p.config_dir.join("keep-me");
    std::fs::write(&target, "x").unwrap();
    let link = token_path(&p, Provider::Posthog);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(remove_token(&p, Provider::Posthog).is_err());
    assert!(std::fs::symlink_metadata(&link).is_ok());
    assert!(target.exists());
    // A directory too.
    std::fs::remove_file(&link).unwrap();
    std::fs::create_dir(&link).unwrap();
    assert!(remove_token(&p, Provider::Posthog).is_err());
    assert!(link.is_dir());
}

// ------------------------------------------------------- attribution

fn entry(at: u64, ws: &str, prev: Option<&str>, head: &str, sid: Option<&str>) -> CommitEntry {
    CommitEntry {
        at_unix: at,
        workspace: PathBuf::from(ws),
        branch: Some("main".into()),
        head: head.into(),
        previous_head: prev.map(String::from),
        session_id: sid.map(String::from),
        agent: sid.map(|_| "claude".into()),
        via: "poll".into(),
    }
}

#[test]
fn commits_in_a_deploy_name_their_sessions_or_say_made_elsewhere() {
    let (a, b, c) = (sha('a'), sha('b'), sha('c'));
    let log = vec![
        entry(1, "/w", None, &a, None),
        entry(2, "/w", Some(&a), &b, Some("s-1-1")),
        entry(3, "/w", Some(&b), &c, None),
    ];
    let r = attribute(&log, None, Some(&a), Some(&c));
    assert_eq!(r.workspace.as_deref(), Some(Path::new("/w")));
    assert_eq!(r.commits.len(), 2);
    assert_eq!(r.commits[0].session_id.as_deref(), Some("s-1-1"));
    assert_eq!(r.commits[0].agent.as_deref(), Some("claude"));
    assert_eq!(r.commits[0].made, "session");
    assert_eq!(r.commits[1].made, "no_session");
    // Never seen here.
    let r = attribute(&log, None, Some(&a), Some(&sha('f')));
    assert_eq!(r.workspace, None);
    assert_eq!(r.commits[0].made, "made_elsewhere");
    // The configured workspace is the one looked in.
    let r = attribute(&log, Some(Path::new("/other")), Some(&a), Some(&c));
    assert_eq!(r.commits[0].made, "made_elsewhere");
    // No commit: nothing to say.
    assert!(attribute(&log, None, None, None).commits.is_empty());
}

// ------------------------------------------------------------ finding

#[test]
fn a_slower_deploy_is_a_production_slower_finding_with_its_commits() {
    let h = sentry_http();
    let d = poll(&sentry::Sentry(sentry_cfg()), &h, &tok("t"), 0).unwrap();
    let (dd, e) = (sha('d'), sha('e'));
    let log = vec![
        entry(10, "/w", None, &dd, None),
        entry(20, "/w", Some(&dd), &e, Some("s-5-5")),
    ];
    let cfg = IntegrationsConfig::default();
    let f = findings(&[(Provider::Sentry, d)], &log, &cfg, 99);
    assert_eq!(f.len(), 1, "{f:?}");
    let f = &f[0];
    assert_eq!(f.kind, FindingKind::ProductionSlower);
    assert_eq!(f.category, FindingCategory::Performance);
    assert_eq!(
        f.title,
        "Production transaction p95 on Sentry went from 120 ms to 380 ms after deploy eeeeeee"
    );
    let ev = f.evidence.deploy.as_ref().unwrap();
    assert_eq!(ev.provider, Provider::Sentry);
    assert_eq!(ev.before_ms, 120.0);
    assert_eq!(ev.after_ms, 380.0);
    assert_eq!(ev.previous_sha.as_deref(), Some(dd.as_str()));
    assert_eq!(ev.attribution.commits.len(), 1);
    assert_eq!(
        ev.attribution.commits[0].session_id.as_deref(),
        Some("s-5-5")
    );
    assert!(f.why.contains("claude"), "{}", f.why);
    assert_eq!(f.evidence.workspace.as_deref(), Some(Path::new("/w")));
    // It groups into an issue by provider and metric.
    let (_, culprit, _) = crate::issues::fingerprint(f).unwrap();
    assert_eq!(culprit, "sentry p95");
}

#[test]
fn a_commit_never_seen_here_is_made_elsewhere() {
    let d = poll(&sentry::Sentry(sentry_cfg()), &sentry_http(), &tok("t"), 0).unwrap();
    let f = findings(
        &[(Provider::Sentry, d)],
        &[],
        &IntegrationsConfig::default(),
        1,
    );
    let ev = f[0].evidence.deploy.as_ref().unwrap();
    assert_eq!(ev.attribution.commits[0].made, "made_elsewhere");
    assert!(f[0].why.contains("made elsewhere"), "{}", f[0].why);
}

#[test]
fn steady_or_short_histories_raise_nothing() {
    let mk = |vals: &[f64]| -> Vec<Deploy> {
        vals.iter()
            .enumerate()
            .rev()
            .map(|(i, v)| Deploy {
                provider: Provider::Vercel,
                id: format!("dpl_{i}"),
                sha: None,
                at_unix: i as u64,
                metrics: vec![MetricValue {
                    metric: "build_duration".into(),
                    value_ms: *v,
                    samples: None,
                }],
            })
            .collect()
    };
    let cfg = IntegrationsConfig::default();
    // Steady.
    let f = findings(&[(Provider::Vercel, mk(&[100.0; 8]))], &[], &cfg, 1);
    assert!(f.is_empty());
    // Too few before the slow one.
    let f = findings(
        &[(Provider::Vercel, mk(&[100.0, 100.0, 900.0]))],
        &[],
        &cfg,
        1,
    );
    assert!(f.is_empty());
    // Slower, but by less than min_increase_ms.
    let f = findings(
        &[(Provider::Vercel, mk(&[50.0, 50.0, 50.0, 140.0]))],
        &[],
        &cfg,
        1,
    );
    assert!(f.is_empty());
    // Slower by both rules: one finding, and Vercel's says what it measures.
    let f = findings(
        &[(
            Provider::Vercel,
            mk(&[30_000.0, 31_000.0, 29_000.0, 90_000.0]),
        )],
        &[],
        &cfg,
        1,
    );
    assert_eq!(f.len(), 1);
    assert!(
        f[0].title
            .starts_with("Production build duration on Vercel went from 30s to 1m30s"),
        "{}",
        f[0].title
    );
    assert!(f[0].why.contains("not how fast it serves"));
}

#[test]
fn the_cache_holds_no_token_and_survives_a_reload() {
    let (_t, p) = paths();
    let mut c = Cache::default();
    c.providers.insert(
        Provider::Sentry,
        ProviderCache {
            last_error: Some("x".into()),
            ..ProviderCache::default()
        },
    );
    save_cache(&p, &c).unwrap();
    assert_eq!(mode(&cache_path(&p)), 0o600);
    assert_eq!(load_cache(&p), c);
}

#[test]
fn identifiers_and_hosts_are_checked() {
    let mut c = crate::config::Config::default();
    assert!(c.validate().is_ok());
    c.integrations.sentry.enabled = true;
    assert!(c.validate().is_err(), "an enabled provider needs its ids");
    c.integrations.sentry.org = "acme".into();
    c.integrations.sentry.project = "web".into();
    assert!(c.validate().is_ok());
    for bad in [
        "evil.example",
        "sentry.io.evil.example",
        "x.y.sentry.io",
        "SENTRY.IO",
    ] {
        c.integrations.sentry.host = bad.into();
        assert!(c.validate().is_err(), "{bad}");
    }
    c.integrations.sentry.host = "de.sentry.io".into();
    c.integrations.sentry.project = "web/../x".into();
    assert!(c.validate().is_err());
    c.integrations.sentry.project = "web".into();
    c.integrations.posthog.host = "i.posthog.com".into();
    assert!(c.validate().is_err());
    c.integrations.posthog.host = "eu.posthog.com".into();
    c.integrations.posthog.project = "12a".into();
    assert!(c.validate().is_err());
    c.integrations.posthog.project = "12".into();
    c.integrations.interval_mins = 10;
    assert!(c.validate().is_err(), "at least 15 minutes");
    c.integrations.interval_mins = 15;
    assert!(c.validate().is_ok());
    assert!(!crate::config::Config::default().integrations.any_enabled());
}
