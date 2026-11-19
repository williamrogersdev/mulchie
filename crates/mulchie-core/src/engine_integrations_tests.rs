//! The integrations worker through the engine: timing, the off switch,
//! status without secrets, deploys with attribution, and findings that
//! leave `build_slower` alone. Fakes only; nothing reaches the network.

use super::*;
use crate::engine::tests::engine_with;
use crate::integrations::tests::{sentry_cfg, sentry_http, sha};
use crate::integrations::{save_token, Secret};
use crate::watch::FindingKind;

fn sentry_on() -> Config {
    let mut c = Config::default();
    c.integrations.sentry = sentry_cfg();
    c
}

fn engine(
    cfg: Config,
) -> (
    tempfile::TempDir,
    Engine,
    Arc<crate::integrations::tests::FakeHttp>,
) {
    let (t, e) = engine_with(cfg);
    let http = Arc::new(sentry_http());
    let e = e.with_integration_http(http.clone());
    save_token(
        e.paths(),
        Provider::Sentry,
        &Secret::new("sntrys_test_token").unwrap(),
    )
    .unwrap();
    (t, e, http)
}

#[test]
fn polls_wait_for_the_start_delay_and_the_interval() {
    let (_t, e, http) = engine(sentry_on());
    let t0 = lock(&e.integrations.state).started_unix;
    e.integrations_tick(t0 + 5);
    assert!(
        http.calls.lock().unwrap().is_empty(),
        "not before the delay"
    );
    e.integrations_tick(t0 + integrations::FIRST_POLL_DELAY_SECS);
    assert_eq!(http.calls.lock().unwrap().len(), 2, "releases and p95");
    e.integrations_tick(t0 + integrations::FIRST_POLL_DELAY_SECS + 600);
    assert_eq!(
        http.calls.lock().unwrap().len(),
        2,
        "not before the interval"
    );
    e.integrations_tick(t0 + integrations::FIRST_POLL_DELAY_SECS + 30 * 60);
    assert_eq!(http.calls.lock().unwrap().len(), 4);
    let st = e.integrations_status();
    let s = st
        .providers
        .iter()
        .find(|p| p.provider == Provider::Sentry)
        .unwrap();
    assert!(s.enabled && s.token);
    assert_eq!(s.deploys, 5);
    assert_eq!(s.last_error, None);
    assert!(s.last_success_unix.is_some());
    assert_eq!(st.slower, 1);
}

#[test]
fn off_by_default_polls_nothing_and_refuses_a_check() {
    let (_t, e, http) = engine(Config::default());
    e.integrations_tick(u64::MAX / 4);
    assert!(http.calls.lock().unwrap().is_empty());
    assert_eq!(
        e.request_integrations_check().unwrap_err().0,
        "integrations_off"
    );
    let st = e.integrations_status();
    assert!(st
        .providers
        .iter()
        .all(|p| !p.enabled && p.next_poll_unix.is_none()));
}

#[test]
fn errors_back_off_and_say_why_without_the_token() {
    let (_t, e) = engine_with(sentry_on());
    let http = Arc::new(crate::integrations::tests::FakeHttp {
        fail: Some("tls: Bearer sntrys_test_token was refused".into()),
        ..Default::default()
    });
    let e = e.with_integration_http(http.clone());
    // No token yet.
    e.poll_provider(Provider::Sentry, &e.config().integrations, 1000);
    let st = e.integrations_status();
    let s = &st.providers[1];
    assert!(s
        .last_error
        .as_deref()
        .unwrap()
        .contains("mulchie integrations add sentry"));
    assert!(http.calls.lock().unwrap().is_empty());
    save_token(
        e.paths(),
        Provider::Sentry,
        &Secret::new("sntrys_test_token").unwrap(),
    )
    .unwrap();
    e.poll_provider(Provider::Sentry, &e.config().integrations, 2000);
    let s = e.integrations_status().providers[1].clone();
    let err = s.last_error.unwrap();
    assert!(
        !err.contains("sntrys_test_token") && err.contains("[redacted]"),
        "{err}"
    );
    assert_eq!(s.failures, 2);
    let next = lock(&e.integrations.state)
        .cache
        .as_ref()
        .unwrap()
        .providers[&Provider::Sentry]
        .next_unix;
    assert_eq!(next, 2000 + 4 * 30 * 60, "doubled twice");
}

#[test]
fn deploys_carry_metrics_and_the_sessions_behind_their_commits() {
    let (_t, e, _http) = engine(sentry_on());
    e.poll_provider(Provider::Sentry, &e.config().integrations, 100);
    let l = e.deploys(Some(Provider::Sentry));
    assert_eq!(l.deploys.len(), 5);
    assert_eq!(
        l.deploys[0].previous_deploy_id.as_deref(),
        Some(sha('d').as_str())
    );
    assert_eq!(l.deploys[0].attribution.commits[0].made, "made_elsewhere");
    assert!(e.deploys(Some(Provider::Vercel)).deploys.is_empty());
    // A disabled provider's deploys are not shown.
    let mut c = e.config();
    c.integrations.sentry.enabled = false;
    e.set_config(c).unwrap();
    assert!(e.deploys(None).deploys.is_empty());
}

#[test]
fn production_and_build_findings_leave_each_other_alone() {
    let (_t, e, _http) = engine(sentry_on());
    e.poll_provider(Provider::Sentry, &e.config().integrations, 100);
    e.publish_integration_findings(&e.config().integrations, 100);
    let kinds = |e: &Engine| -> Vec<FindingKind> {
        lock(&e.state).findings.iter().map(|f| f.kind).collect()
    };
    assert_eq!(kinds(&e), vec![FindingKind::ProductionSlower]);
    // A build_slower finding stored by the perf worker.
    let mut b = lock(&e.state).findings[0].clone();
    b.kind = FindingKind::BuildSlower;
    b.id = "build_slower:0000000000000001".into();
    b.evidence.deploy = None;
    e.replace_findings_of(
        crate::watch::FindingCategory::Performance,
        &[FindingKind::BuildSlower],
        vec![b.clone()],
        101,
    );
    assert_eq!(kinds(&e).len(), 2);
    // Turning the integration off clears its finding only.
    let mut c = e.config();
    c.integrations.sentry.enabled = false;
    e.set_config(c).unwrap();
    e.integrations_tick(200);
    assert_eq!(kinds(&e), vec![FindingKind::BuildSlower]);
    // And the perf worker's rebuild leaves production findings alone.
    let mut c = e.config();
    c.integrations.sentry.enabled = true;
    e.set_config(c).unwrap();
    e.publish_integration_findings(&e.config().integrations, 300);
    e.replace_findings_of(
        crate::watch::FindingCategory::Performance,
        &[FindingKind::BuildSlower],
        Vec::new(),
        301,
    );
    assert_eq!(kinds(&e), vec![FindingKind::ProductionSlower]);
    // It is grouped into an issue.
    let issues = e.issues(None, false, Some(FindingKind::ProductionSlower));
    assert_eq!(issues.issues.len(), 1, "{issues:?}");
}

#[test]
fn the_cache_brings_deploys_back_after_a_restart() {
    let (t, e, http) = engine(sentry_on());
    e.poll_provider(Provider::Sentry, &e.config().integrations, 100);
    drop(e);
    let e2 = Engine::with_processes(
        Paths::under(t.path()),
        LoadedConfig {
            config: sentry_on(),
            error: None,
        },
        Arc::new(crate::process::FakeProcesses::empty()),
    )
    .with_integration_http(http.clone());
    assert_eq!(e2.deploys(None).deploys.len(), 5);
    let raw = std::fs::read_to_string(integrations::cache_path(e2.paths())).unwrap();
    assert!(!raw.contains("sntrys_test_token"));
}
