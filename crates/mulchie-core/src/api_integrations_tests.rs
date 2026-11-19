//! The integration routes, and the rule that a provider token appears in
//! no response and no file but its own. Fakes only.

use super::*;
use crate::config::LoadedConfig;
use crate::integrations::tests::{reply, sentry_cfg, sentry_http, FakeHttp};
use crate::integrations::{save_token, Provider, Secret};
use crate::process::FakeProcesses;
use serde_json::json;
use std::io::Cursor;

fn engine() -> (tempfile::TempDir, Engine) {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let e = Engine::with_processes(
        paths,
        LoadedConfig {
            config: crate::config::Config::default(),
            error: None,
        },
        Arc::new(FakeProcesses::empty()),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe));
    (t, e)
}

fn call(e: &Engine, raw: &str) -> Resp {
    let mut r = Cursor::new(raw.as_bytes().to_vec());
    let req = read_req(
        &mut r,
        &Limits::default(),
        Instant::now() + Duration::from_secs(5),
    )
    .map_err(|_| "bad request")
    .unwrap();
    route(&req, e, false)
}

fn get(e: &Engine, path: &str) -> Resp {
    call(
        e,
        &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n"),
    )
}

/// A provider token never leaves its file: not in the config, the cache,
/// the journal, findings, errors or any API response, also after a provider
/// answered with the token in its error body and the HTTP client echoed it
/// in its own error.
#[test]
fn an_integration_token_appears_nowhere() {
    const SENTINEL: &str = "sntrys_SENTINEL_9f8e7d6c5b4a";
    let (t, e) = engine();
    let mut cfg = e.config();
    cfg.integrations.sentry = sentry_cfg();
    e.set_config(cfg).unwrap();
    save_token(e.paths(), Provider::Sentry, &Secret::new(SENTINEL).unwrap()).unwrap();
    // A refusal whose body carries the token.
    let mut refusing = sentry_http();
    for r in refusing.gets.values_mut() {
        *r = reply(401, json!({"detail": format!("Invalid token {SENTINEL}")}));
    }
    let e = e.with_integration_http(Arc::new(refusing));
    e.poll_provider(Provider::Sentry, &e.config().integrations, 1000);
    assert!(e.integrations_status().providers[1]
        .last_error
        .as_deref()
        .unwrap()
        .contains("refused the token"));
    // A client error that echoes it.
    let echoing = FakeHttp {
        fail: Some(format!("Authorization: Bearer {SENTINEL} rejected")),
        ..FakeHttp::default()
    };
    let e = e.with_integration_http(Arc::new(echoing));
    e.poll_provider(Provider::Sentry, &e.config().integrations, 2000);
    let journal_err = e.integrations_status().providers[1].last_error.clone();
    assert!(journal_err.is_some());
    // Then a good poll, so there are deploys and a finding.
    let e = e.with_integration_http(Arc::new(sentry_http()));
    e.poll_provider(Provider::Sentry, &e.config().integrations, 3000);
    e.publish_integration_findings(&e.config().integrations, 3000);
    assert!(e
        .snapshot_state()
        .findings
        .iter()
        .any(|f| f.kind == crate::watch::FindingKind::ProductionSlower));
    for path in [
        "/v1/integrations",
        "/v1/deploys",
        "/v1/deploys?provider=sentry",
        "/v1/state",
        "/v1/config",
        "/v1/findings",
        "/v1/issues",
    ] {
        let r = get(&e, path);
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        assert!(!r.body.contains(SENTINEL), "{path} leaks the token");
    }
    // Every file Mulchie wrote, other than the token file itself.
    let token_file = crate::integrations::token_path(e.paths(), Provider::Sentry);
    let mut stack = vec![t.path().to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).unwrap().flatten() {
            let p = ent.path();
            let ft = ent.file_type().unwrap();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p != token_file {
                seen += 1;
                let b = std::fs::read(&p).unwrap();
                assert!(
                    !String::from_utf8_lossy(&b).contains(SENTINEL),
                    "{} holds the token",
                    p.display()
                );
            }
        }
    }
    assert!(seen >= 2, "the config and the cache were written");
    assert!(std::fs::read_to_string(&token_file)
        .unwrap()
        .contains(SENTINEL));
}

#[test]
fn deploys_take_only_a_known_provider_and_a_check_is_refused_when_off() {
    let (_t, e) = engine();
    let r = get(&e, "/v1/deploys?provider=heroku");
    assert_eq!(r.status, 400, "{}", r.body);
    let r = get(&e, "/v1/integrations");
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["providers"].as_array().unwrap().len(), 3);
    assert!(v["sends"]
        .as_str()
        .unwrap()
        .contains("Nothing about this machine"));
    let r = call(
        &e,
        "POST /v1/integrations/check HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(r.status, 409);
    assert!(r.body.contains("integrations_off"), "{}", r.body);
    assert!(!read_only_allowed("POST", "/v1/integrations/check"));
    assert!(read_only_allowed("GET", "/v1/deploys"));
}
