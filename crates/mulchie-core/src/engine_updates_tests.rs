//! The update worker's timing, the `off` switch, findings in state and
//! health, and the cache across a restart. Fakes only.

use super::*;
use crate::engine::tests::engine_with;
use crate::updates::tests::{reply, FakeHttp, FakeRunner};
use crate::updates::MULCHIE_RELEASE_URL;
use serde_json::json;

fn fake_http() -> Arc<FakeHttp> {
    let mut h = FakeHttp::default();
    h.gets.insert(
        MULCHIE_RELEASE_URL.into(),
        reply(200, json!({"tag_name": "v99.0.0"})),
    );
    Arc::new(h)
}

#[test]
fn checks_are_due_after_the_start_delay_or_on_request() {
    let (_t, e) = engine_with(Config::default());
    let http = fake_http();
    let e = e.with_update_sources(http.clone(), Arc::new(FakeRunner::default()));
    let t0 = lock(&e.updates.state).started_unix;
    e.update_tick(t0 + 10);
    assert!(
        http.calls.lock().unwrap().is_empty(),
        "not before the delay"
    );
    e.update_tick(t0 + updates::FIRST_CHECK_DELAY_SECS);
    assert_eq!(http.calls.lock().unwrap().len(), 1);
    // Not again until the interval.
    e.update_tick(t0 + updates::FIRST_CHECK_DELAY_SECS + 60);
    assert_eq!(http.calls.lock().unwrap().len(), 1);
    // On request: at once.
    e.request_update_check().unwrap();
    e.update_tick(t0 + updates::FIRST_CHECK_DELAY_SECS + 61);
    assert_eq!(http.calls.lock().unwrap().len(), 2);

    let st = e.snapshot_state();
    assert!(st
        .findings
        .iter()
        .any(|f| f.kind == crate::watch::FindingKind::MulchieUpdate));
    assert_eq!(st.updates.available, 1);
    assert!(st.updates.report.is_none(), "compact in state");
    assert_eq!(e.health_summary().updates_available, 1);
    assert!(e.updates_status(true).report.is_some());
    // A watch run keeps them.
    e.run_watch();
    assert_eq!(e.updates_available(), 1);
}

#[test]
fn off_checks_nothing_and_clears_findings() {
    let (_t, e) = engine_with(Config::default());
    let http = fake_http();
    let e = e.with_update_sources(http.clone(), Arc::new(FakeRunner::default()));
    e.run_update_check(1_000);
    assert_eq!(e.updates_available(), 1);
    let mut c = e.config();
    c.updates.check = crate::config::UpdateCheck::Off;
    e.set_config(c).unwrap();
    assert_eq!(e.request_update_check().unwrap_err().0, "updates_off");
    e.update_tick(u64::MAX / 2);
    e.run_update_check(2_000);
    assert_eq!(http.calls.lock().unwrap().len(), 1, "no request while off");
    assert_eq!(e.updates_available(), 0);
}

/// A restart shows the cached findings without asking the network.
#[test]
fn the_cache_carries_findings_across_a_restart() {
    let (t, e) = engine_with(Config::default());
    let e = e.with_update_sources(fake_http(), Arc::new(FakeRunner::default()));
    e.run_update_check(1_000);
    let paths = e.paths().clone();
    drop(e);
    let silent = Arc::new(FakeHttp::default());
    let e2 = Engine::with_processes(
        paths.clone(),
        Config::load_from(&paths),
        Arc::new(crate::process::FakeProcesses::empty()),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe))
    .with_update_sources(silent.clone(), Arc::new(FakeRunner::default()));
    let mut threads = Vec::new();
    e2.start_updates(&mut threads);
    assert_eq!(e2.updates_available(), 1);
    assert!(silent.calls.lock().unwrap().is_empty());
    e2.shutdown(Duration::from_secs(2));
    for h in threads {
        let _ = h.join();
    }
    // A non-holder starts no worker and shows nothing.
    let e3 = Engine::with_processes(
        paths.clone(),
        Config::load_from(&paths),
        Arc::new(crate::process::FakeProcesses::empty()),
    )
    .with_update_sources(silent, Arc::new(FakeRunner::default()));
    e3.set_automation_enabled(false);
    let mut threads = Vec::new();
    e3.start_updates(&mut threads);
    assert!(threads.is_empty());
    drop(t);
}
