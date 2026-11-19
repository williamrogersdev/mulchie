//! The webhook routes and the config routes with a webhook URL set: the
//! URL is redacted in every response, a round trip that does not change it
//! keeps it, and it appears in no file but the config. Fakes only.

use super::*;
use crate::config::LoadedConfig;
use crate::process::FakeProcesses;
use crate::webhook::tests::FakePost;
use serde_json::{json, Value};
use std::io::Cursor;

const SECRET: &str = "SENTINELwebhookPATH9f8e7d";

fn url() -> String {
    format!("https://hooks.example.com/services/{SECRET}")
}

fn engine(http: Arc<FakePost>) -> (tempfile::TempDir, Engine) {
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
    .with_watch_probe(Arc::new(crate::watch::NoProbe))
    .with_webhook_http(http, Duration::ZERO);
    (t, e)
}

fn call(e: &Engine, method: &str, path: &str, body: Option<&Value>) -> (u16, Value, String) {
    let b = body.map(|b| b.to_string()).unwrap_or_default();
    let raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Length: {}\r\n\r\n{b}",
        b.len()
    );
    let mut r = Cursor::new(raw.into_bytes());
    let req = read_req(
        &mut r,
        &Limits::default(),
        Instant::now() + Duration::from_secs(5),
    )
    .map_err(|_| "bad request")
    .unwrap();
    let resp = route(&req, e, false);
    let v = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
    (resp.status, v, resp.body)
}

fn saved(e: &Engine) -> String {
    crate::config::Config::load_from(e.paths())
        .config
        .webhook
        .url
}

#[test]
fn config_routes_redact_the_url_and_round_trips_keep_it() {
    let (_t, e) = engine(Arc::new(FakePost::default()));
    let (s, v, _) = call(
        &e,
        "PATCH",
        "/v1/config",
        Some(&json!({"webhook": {"url": url()}})),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(saved(&e), url());
    assert_eq!(
        v["config"]["webhook"]["url"],
        "https://hooks.example.com/[redacted]"
    );

    // GET: redacted, and the whole body sent back as it is (PUT, with the
    // revision) keeps the URL.
    let (s, got, body) = call(&e, "GET", "/v1/config", None);
    assert_eq!(s, 200);
    assert!(!body.contains(SECRET), "{body}");
    let shown = got["config"]["webhook"]["url"].clone();
    assert_eq!(shown, "https://hooks.example.com/[redacted]");
    let mut cfg = got["config"].clone();
    cfg["watch"]["interval_secs"] = json!(cfg["watch"]["interval_secs"].as_u64().unwrap() + 1);
    let path = format!(
        "/v1/config?if_revision={}",
        got["revision"].as_str().unwrap()
    );
    let (s, v, body) = call(&e, "PUT", &path, Some(&json!({ "config": cfg })));
    assert_eq!(s, 200, "{v}");
    assert!(!body.contains(SECRET), "{body}");
    assert_eq!(saved(&e), url());

    // PATCH without the webhook, and with the redacted value: kept.
    let (s, _, body) = call(
        &e,
        "PATCH",
        "/v1/config",
        Some(&json!({"webhook": {"format": "slack"}})),
    );
    assert_eq!(s, 200);
    assert!(!body.contains(SECRET));
    let (s, _, _) = call(
        &e,
        "PATCH",
        "/v1/config",
        Some(&json!({"webhook": {"url": shown}})),
    );
    assert_eq!(s, 200);
    assert_eq!(saved(&e), url());

    // Another host's redacted value: 400, nothing saved.
    let before = std::fs::read(e.paths().config_file()).unwrap();
    let (s, v, _) = call(
        &e,
        "PATCH",
        "/v1/config",
        Some(&json!({"webhook": {"url": "https://evil.example.com/[redacted]"}})),
    );
    assert_eq!(
        (s, v["code"].as_str()),
        (400, Some("invalid_config")),
        "{v}"
    );
    assert_eq!(before, std::fs::read(e.paths().config_file()).unwrap());
    let mut put = got["config"].clone();
    put["webhook"]["url"] = json!("https://evil.example.com/[redacted]");
    let (s, _, _) = call(&e, "PUT", "/v1/config", Some(&json!({ "config": put })));
    assert_eq!(s, 400);
    assert_eq!(saved(&e), url());

    // The state carries the config: redacted there too.
    let (s, _, body) = call(&e, "GET", "/v1/state", None);
    assert_eq!(s, 200);
    assert!(!body.contains(SECRET));
    assert!(body.contains("https://hooks.example.com/[redacted]"));

    // Clearing it turns the webhook off.
    let (s, _, _) = call(
        &e,
        "PATCH",
        "/v1/config",
        Some(&json!({"webhook": {"url": ""}})),
    );
    assert_eq!(s, 200);
    assert_eq!(saved(&e), "");
    let (s, v, _) = call(&e, "POST", "/v1/webhook/test", None);
    assert_eq!(s, 409, "{v}");
}

#[test]
fn the_test_route_is_full_token_only() {
    assert!(read_only_allowed("GET", "/v1/webhook"));
    assert!(!read_only_allowed("POST", "/v1/webhook/test"));
}

/// The URL never leaves the config file: not in any response, the journal,
/// the status or another file, also after the HTTP client echoed it in its
/// error.
#[cfg(feature = "network")]
#[test]
fn the_webhook_url_appears_nowhere_but_the_config() {
    let http = Arc::new(FakePost::new(vec![Err(format!(
        "could not reach {}",
        url()
    ))]));
    let (t, e) = engine(http.clone());
    let mut c = e.config();
    c.webhook.url = url();
    c.webhook.include_host = true;
    e.set_config(c).unwrap();
    let _lock = e.acquire_lock().unwrap();
    let worker = e.webhook_start_for_test();
    let (s, v, body) = call(&e, "POST", "/v1/webhook/test", None);
    assert_eq!(
        (s, v["code"].as_str()),
        (502, Some("webhook_failed")),
        "{v}"
    );
    assert!(!body.contains(SECRET), "{body}");
    assert!(
        body.contains("https://hooks.example.com/[redacted]"),
        "{body}"
    );
    assert_eq!(http.count(), 1, "a test is tried once");
    // Then a good one.
    *http.replies.lock().unwrap() = vec![Ok(200)];
    let (s, v, _) = call(&e, "POST", "/v1/webhook/test", None);
    assert_eq!((s, v["status"].as_u64()), (200, Some(200)), "{v}");
    let sent: Value = serde_json::from_slice(&http.posts.lock().unwrap()[1].1).unwrap();
    assert!(
        sent["host"].is_string(),
        "include_host adds the host: {sent}"
    );

    for path in [
        "/v1/webhook",
        "/v1/config",
        "/v1/state",
        "/v1/findings",
        "/v1/issues",
    ] {
        let (s, _, body) = call(&e, "GET", path, None);
        assert_eq!(s, 200, "{path}");
        assert!(!body.contains(SECRET), "{path} leaks the URL");
    }
    let (_, st, _) = call(&e, "GET", "/v1/webhook", None);
    assert_eq!(st["url"], "https://hooks.example.com/[redacted]");
    assert_eq!(st["sent"], 1);
    assert_eq!(st["failed"], 1);
    e.shutdown(Duration::from_secs(2));
    worker.join().unwrap();

    let config = e.paths().config_file();
    let mut stack = vec![t.path().to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).unwrap().flatten() {
            let p = ent.path();
            let ft = ent.file_type().unwrap();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && p != config {
                seen += 1;
                let b = std::fs::read(&p).unwrap();
                assert!(
                    !String::from_utf8_lossy(&b).contains(SECRET),
                    "{} holds the URL",
                    p.display()
                );
            }
        }
    }
    assert!(seen >= 1, "the journal was written");
    assert!(std::fs::read_to_string(&config).unwrap().contains(SECRET));
}
