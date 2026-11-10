//! API tests: parsing limits on any reader, the auth matrix over every route,
//! Host and Origin, connection limits, token file permissions, the client's
//! listener check, and plan-then-execute exactness. Everything runs against an
//! engine whose paths are under a tempdir, on a free loopback port.

use super::*;
use crate::config::LoadedConfig;
use crate::lock::EngineLock;
use crate::process::FakeProcesses;
use crate::safety::CACHEDIR_TAG_SIGNATURE;
use serde_json::{json, Value};
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;

// ------------------------------------------------------------ read_req

fn parse(raw: &[u8]) -> Result<Req, ReqError> {
    parse_with(raw, &Limits::default())
}

fn parse_with(raw: &[u8], limits: &Limits) -> Result<Req, ReqError> {
    let mut r = Cursor::new(raw.to_vec());
    read_req(&mut r, limits, Instant::now() + Duration::from_secs(5))
}

fn status(r: Result<Req, ReqError>) -> (u16, String) {
    match r {
        Ok(_) => (200, String::new()),
        Err(ReqError::Http(e)) => (e.status, e.code),
        Err(ReqError::Closed) => (0, "closed".into()),
    }
}

#[test]
fn parses_a_well_formed_request() {
    let r = parse(b"POST /v1/clean?x=1 HTTP/1.1\r\nHost: 127.0.0.1:7\r\nAuthorization: Bearer abc\r\nContent-Length: 2\r\n\r\n{}")
        .unwrap();
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/v1/clean");
    assert_eq!(r.query, "x=1");
    assert_eq!(r.host.as_deref(), Some("127.0.0.1:7"));
    assert_eq!(r.auth.as_deref(), Some("Bearer abc"));
    assert_eq!(r.body, b"{}");
    // Bare LF line ends are accepted too.
    assert!(parse(b"GET /v1/health HTTP/1.0\nHost: x\n\n").is_ok());
}

#[test]
fn malformed_requests_are_400() {
    for raw in [
        &b"GARBAGE\r\n\r\n"[..],
        b"GET /v1/health\r\n\r\n",
        b"get /v1/health HTTP/1.1\r\n\r\n",
        b"GET v1/health HTTP/1.1\r\n\r\n",
        b"GET /v1/health HTTP/2\r\n\r\n",
        b"GET /a HTTP/1.1\r\nNoColonHere\r\n\r\n",
        b"GET /a HTTP/1.1\r\nBad Name: x\r\n\r\n",
        b"GET /a HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
        b"GET /a HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nxx",
        b"GET /a HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
        b"POST /a HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort",
        b"GET /a HTTP/1.1\r\nHost: a\r\n",
        b"GET /a HTTP/1.1\r\n\xff\xfe: x\r\n\r\n",
    ] {
        assert_eq!(
            status(parse(raw)).0,
            400,
            "{:?}",
            String::from_utf8_lossy(raw)
        );
    }
    assert_eq!(status(parse(b"")).1, "closed");
}

#[test]
fn oversized_requests_are_refused_with_their_own_codes() {
    let long_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(9000));
    assert_eq!(
        status(parse(long_line.as_bytes())),
        (414, "request_line_too_long".into())
    );
    let no_newline = "G".repeat(100_000);
    assert_eq!(
        status(parse(no_newline.as_bytes())).0,
        414,
        "stops reading early"
    );

    let long_header = format!("GET / HTTP/1.1\r\nX-A: {}\r\n\r\n", "b".repeat(9000));
    assert_eq!(
        status(parse(long_header.as_bytes())),
        (431, "headers_too_large".into())
    );
    let mut many = String::from("GET / HTTP/1.1\r\n");
    for i in 0..20 {
        many.push_str(&format!("X-{i}: {}\r\n", "c".repeat(4000)));
    }
    many.push_str("\r\n");
    assert_eq!(
        status(parse(many.as_bytes())).0,
        431,
        "64 KB of headers in all"
    );

    let big = format!(
        "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        1024 * 1024 + 1
    );
    assert_eq!(
        status(parse(big.as_bytes())),
        (413, "body_too_large".into())
    );
    let chunked = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    assert_eq!(
        status(parse(chunked)),
        (501, "unsupported_transfer_encoding".into())
    );
}

/// Hands out one byte per read, slowly.
struct Slow {
    data: Vec<u8>,
    at: usize,
    delay: Duration,
}

impl Read for Slow {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.delay);
        if self.at >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[self.at];
        self.at += 1;
        Ok(1)
    }
}

#[test]
fn slow_input_hits_the_deadline() {
    let slow = Slow {
        data: b"GET /v1/state HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n".to_vec(),
        at: 0,
        delay: Duration::from_millis(20),
    };
    let mut r = BufReader::with_capacity(1, slow);
    let t = Instant::now();
    let res = read_req(
        &mut r,
        &Limits::default(),
        Instant::now() + Duration::from_millis(150),
    );
    assert_eq!(status(res), (408, "request_timeout".into()));
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn tokens_compare_in_constant_time_shape() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"abcd"));
    assert!(!constant_time_eq(b"", b"a"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn timeouts_per_route() {
    assert_eq!(timeout_for("GET", "/v1/health"), HEALTH_TIMEOUT);
    assert_eq!(timeout_for("GET", "/v1/state"), DEFAULT_TIMEOUT);
    assert_eq!(timeout_for("POST", "/v1/sweep"), LONG_TIMEOUT);
    assert_eq!(
        timeout_for("GET", "/v1/clean/plan?want_bytes=1"),
        LONG_TIMEOUT
    );
    assert_eq!(timeout_for("POST", "/v1/lease"), DEFAULT_TIMEOUT);
    // R16: a group restart can take 35 s.
    assert_eq!(timeout_for("POST", "/v1/process/execute"), LONG_TIMEOUT);
    assert_eq!(timeout_for("POST", "/v1/process"), LONG_TIMEOUT);
}

// --------------------------------------------------------- token files

#[test]
fn token_is_created_private_and_loose_files_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let tok = load_or_create_token_in(&paths).unwrap();
    assert_eq!(tok.len(), 64);
    let path = token_path_in(&paths);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&paths.config_dir), 0o700);
    assert_eq!(load_or_create_token_in(&paths).unwrap(), tok, "stable");
    assert_eq!(read_token(&paths).unwrap(), tok);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = load_or_create_token_in(&paths).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("chmod 600"), "{e}");
    assert!(read_token(&paths).is_err());
    assert_eq!(mode(&path), 0o644, "never silently changed or replaced");

    // A symlink is not followed.
    std::fs::remove_file(&path).unwrap();
    let elsewhere = t.path().join("elsewhere");
    std::fs::write(&elsewhere, "f".repeat(64)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    assert!(load_or_create_token_in(&paths).is_err());
    assert!(read_token(&paths).is_err());

    // Ours and private but truncated: replaced.
    std::fs::remove_file(&path).unwrap();
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    drop(f);
    let fresh = load_or_create_token_in(&paths).unwrap();
    assert_eq!(fresh.len(), 64);
}

#[test]
fn a_loose_token_stops_the_server_from_starting() {
    let (_t, e) = engine();
    let path = token_path_in(e.paths());
    load_or_create_token_in(e.paths()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(serve_with(e, 0, Limits::default()).is_err());
}

// ------------------------------------------------------------- server

fn engine() -> (tempfile::TempDir, Engine) {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let root = t.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = Config {
        scan_roots: vec![root.canonicalize().unwrap()],
        ..Config::default()
    };
    let e = Engine::with_processes(
        paths,
        LoadedConfig {
            config: cfg,
            error: None,
        },
        Arc::new(FakeProcesses::empty()),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe));
    (t, e)
}

struct Running {
    t: tempfile::TempDir,
    engine: Engine,
    server: Option<Server>,
    _lock: EngineLock,
    token: String,
    port: u16,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

fn running(limits: Limits) -> Running {
    let (t, engine) = engine();
    let mut lk = engine.acquire_lock().unwrap();
    let server = serve_with(engine.clone(), 0, limits).unwrap();
    let port = server.port();
    lk.set_port(Some(port)).unwrap();
    let token = read_token(engine.paths()).unwrap();
    Running {
        t,
        engine,
        server: Some(server),
        _lock: lk,
        token,
        port,
    }
}

impl Running {
    fn code_root(&self) -> PathBuf {
        self.t.path().join("code").canonicalize().unwrap()
    }
}

/// Send raw bytes, return status code and JSON body.
fn raw(port: u16, req: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn send(port: u16, method: &str, path: &str, auth: Option<&str>, body: &str) -> (u16, Value) {
    let auth = auth
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    raw(
        port,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

#[test]
fn every_route_but_health_needs_the_token() {
    let r = running(Limits::default());
    let good = format!("Bearer {}", r.token);
    let bad = format!("Bearer {}", "0".repeat(64));
    for (method, path) in ROUTES {
        for (label, auth) in [
            ("none", None),
            ("bad", Some(bad.as_str())),
            ("good", Some(good.as_str())),
        ] {
            let (code, v) = send(r.port, method, path, auth, "{}");
            if *path == "/v1/health" {
                assert_eq!(code, 200, "{method} {path} {label}");
                assert_eq!(v["name"], "mulchie");
            } else if label == "good" {
                assert!(
                    code == 200 || code == 400,
                    "{method} {path} {label}: {code} {v}"
                );
                assert_ne!(v["code"], "unauthorized");
            } else {
                assert_eq!(code, 401, "{method} {path} {label}");
                assert_eq!(v["code"], "unauthorized");
            }
        }
    }
    // Deny by default: unknown routes and wrong methods are 401 without a token.
    assert_eq!(send(r.port, "GET", "/v1/nope", None, "").0, 401);
    assert_eq!(send(r.port, "DELETE", "/v1/state", None, "").0, 401);
    assert_eq!(
        send(r.port, "GET", "/v1/nope", Some(&good), "").1["code"],
        "not_found"
    );
    let (code, v) = send(r.port, "DELETE", "/v1/state", Some(&good), "");
    assert_eq!(
        (code, v["code"].as_str()),
        (405, Some("method_not_allowed"))
    );
    // A malformed scheme is no token.
    assert_eq!(send(r.port, "GET", "/v1/state", Some(&r.token), "").0, 401);
    // Nothing destructive happened in the good-token pass: bare POSTs were refused.
    assert!(r
        .engine
        .snapshot_state()
        .actions
        .iter()
        .all(|a| a.status.as_deref() != Some("done")));
}

#[test]
fn host_and_origin_are_checked_before_anything_else() {
    let r = running(Limits::default());
    let p = r.port;
    let get = |extra: &str| raw(p, &format!("GET /v1/health HTTP/1.1\r\n{extra}\r\n"));
    assert_eq!(get(&format!("Host: 127.0.0.1:{p}\r\n")).0, 200);
    assert_eq!(get(&format!("Host: LOCALHOST:{p}\r\n")).0, 200);
    for bad in [
        String::new(),
        "Host: 127.0.0.1\r\n".into(),
        format!("Host: evil.example:{p}\r\n"),
        format!("Host: 127.0.0.1:{}\r\n", p.wrapping_add(1)),
    ] {
        let (code, v) = get(&bad);
        assert_eq!(
            (code, v["code"].as_str()),
            (403, Some("bad_host")),
            "{bad:?}"
        );
    }
    let (code, v) = get(&format!(
        "Host: 127.0.0.1:{p}\r\nOrigin: http://evil.example\r\n"
    ));
    assert_eq!((code, v["code"].as_str()), (403, Some("origin_refused")));
    let (code, _) = get(&format!(
        "Host: 127.0.0.1:{p}\r\nSec-Fetch-Site: cross-site\r\n"
    ));
    assert_eq!(code, 403);
    // Even with the token, a rebinding page is refused.
    let (code, _) = raw(
        p,
        &format!(
            "GET /v1/state HTTP/1.1\r\nHost: rebind.example:{p}\r\nAuthorization: Bearer {}\r\n\r\n",
            r.token
        ),
    );
    assert_eq!(code, 403);
}

#[test]
fn health_reports_staleness() {
    let r = running(Limits::default());
    let (code, v) = send(r.port, "GET", "/v1/health", None, "");
    assert_eq!(code, 200);
    assert_eq!(v["stale"], true, "no sample yet: {v}");
    assert_eq!(v["health"], "stale");
    assert!(v["sample_age_secs"].is_null());
}

#[test]
fn connections_beyond_the_limit_get_503() {
    let r = running(Limits {
        max_connections: 1,
        deadline: Duration::from_secs(2),
        ..Limits::default()
    });
    // Hold the only slot with a request that never finishes.
    let mut hold = TcpStream::connect(("127.0.0.1", r.port)).unwrap();
    hold.write_all(b"GET /v1/health HTTP/1.1\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let (code, v) = send(r.port, "GET", "/v1/health", None, "");
    assert_eq!(
        (code, v["code"].as_str()),
        (503, Some("too_many_connections"))
    );
    // The slow one times out with 408 and frees the slot.
    let mut out = String::new();
    hold.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _ = hold.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 408"), "{out}");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(send(r.port, "GET", "/v1/health", None, "").0, 200);
}

#[test]
fn oversized_bodies_are_refused_over_the_socket() {
    let r = running(Limits::default());
    let (code, v) = raw(
        r.port,
        &format!(
            "POST /v1/clean HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 5000000\r\n\r\n",
            r.port
        ),
    );
    assert_eq!((code, v["code"].as_str()), (413, Some("body_too_large")));
}

// ------------------------------------------------------------- client

#[test]
fn the_client_only_talks_to_the_engine_holding_the_lock() {
    let (_t, e) = engine();
    // Nobody holds the lock: the client refuses before sending anything.
    let server = serve_with(e.clone(), 0, Limits::default()).unwrap();
    let port = server.port();
    let err = Client::connect_in(e.paths(), port).unwrap_err();
    assert!(err.contains("no Mulchie engine"), "{err}");
    let mut lk = e.acquire_lock().unwrap();
    assert!(
        Client::connect_in(e.paths(), port).is_err(),
        "no port announced yet"
    );
    lk.set_port(Some(port.wrapping_add(1))).unwrap();
    assert!(Client::connect_in(e.paths(), port)
        .unwrap_err()
        .contains("listens on port"));
    lk.set_port(Some(port)).unwrap();
    let c = Client::connect_in(e.paths(), port).unwrap();
    let st = c.call("GET", "/v1/state", None).unwrap();
    assert_eq!(st["version"], VERSION);
    let err = c
        .request("POST", "/v1/sweep", Some(&json!({})), DEFAULT_TIMEOUT)
        .unwrap_err();
    assert_eq!((err.status, err.code.as_str()), (400, "items_required"));
    server.stop();
    drop(lk);
}

// -------------------------------------------------- plan then execute

fn cache(root: &Path, rel: &str) -> PathBuf {
    let dir = root.join(rel);
    std::fs::create_dir_all(dir.join("target/debug")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
    std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
    std::fs::write(dir.join("target/debug/blob"), vec![1u8; 10_000]).unwrap();
    dir.join("target")
}

#[test]
fn only_the_posted_items_are_executed() {
    let r = running(Limits::default());
    let c = Client::connect_in(r.engine.paths(), r.port).unwrap();
    let root = r.code_root();
    let a = cache(&root, "a");
    let b = cache(&root, "b");
    let plan = c
        .call("POST", "/v1/clean/plan", Some(&json!({ "paths": [a, b] })))
        .unwrap();
    let items = plan["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["ready"] == true), "{plan}");
    let only_a = vec![items[0]["request"].clone()];
    assert_eq!(only_a[0]["path"], a.display().to_string());
    let res = c
        .call("POST", "/v1/clean", Some(&json!({ "items": only_a })))
        .unwrap();
    assert_eq!(res.as_array().unwrap().len(), 1);
    assert_eq!(res[0]["status"], "done");
    assert!(!a.exists());
    assert!(b.exists(), "planned and deletable, but not approved");

    // An empty approval executes nothing.
    let res = c
        .call("POST", "/v1/clean", Some(&json!({ "items": [] })))
        .unwrap();
    assert_eq!(res, json!([]));
    assert!(b.exists());
}

#[test]
fn one_step_forms_are_refused_without_auto() {
    let r = running(Limits::default());
    let c = Client::connect_in(r.engine.paths(), r.port).unwrap();
    let err = |m: &str, p: &str, b: Value| {
        let e = c.request(m, p, Some(&b), DEFAULT_TIMEOUT).unwrap_err();
        (e.status, e.code)
    };
    assert_eq!(
        err("POST", "/v1/sweep", json!({})),
        (400, "items_required".into())
    );
    assert_eq!(
        err("POST", "/v1/clean", json!({ "stale": true })),
        (400, "plan_required".into())
    );
    assert_eq!(
        err(
            "POST",
            "/v1/worktree/remove",
            json!({ "paths": ["/x"], "backup": true })
        ),
        (400, "expected_head_required".into())
    );
    assert_eq!(
        err("POST", "/v1/worktree/remove", json!({})),
        (400, "items_required".into())
    );
    let cache_item = json!({ "action": "delete_cache", "path": "/x", "require_stale": false });
    assert_eq!(
        err("POST", "/v1/sweep", json!({ "items": [cache_item] })),
        (400, "invalid_item".into())
    );
    let wt_item = json!({ "action": "prune_worktree", "repo": "/r", "path": "/x" });
    assert_eq!(
        err("POST", "/v1/clean", json!({ "items": [wt_item] })),
        (400, "invalid_item".into())
    );
    assert_eq!(
        err("POST", "/v1/clean", json!({ "items": "x" })).1,
        "invalid_body"
    );
    assert_eq!(
        err("GET", "/v1/clean/plan?want_bytes=lots", Value::Null).1,
        "invalid_query"
    );
    // The explicit one-step form exists for scripts and runs a fresh plan.
    let res = c
        .call("POST", "/v1/sweep", Some(&json!({ "auto": true })))
        .unwrap();
    assert_eq!(res, json!([]), "nothing sweepable in an empty tree");
}

#[test]
fn leases_over_the_api() {
    let r = running(Limits::default());
    let c = Client::connect_in(r.engine.paths(), r.port).unwrap();
    let e = c
        .request(
            "POST",
            "/v1/lease",
            Some(&json!({ "label": "x" })),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (400, "lease_needs_pid_or_ttl"));
    let e = c
        .request(
            "POST",
            "/v1/lease",
            Some(&json!({ "ttl_secs": 0 })),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
    assert_eq!(e.code, "ttl_out_of_range");
    let g = c
        .call(
            "POST",
            "/v1/lease",
            Some(&json!({ "label": "x", "ttl_secs": 60 })),
        )
        .unwrap();
    assert!(g["ticket"].as_u64().is_some());
    let secret = g["release_secret"]
        .as_str()
        .expect("a release secret")
        .to_string();
    assert!(secret.len() >= 32);
    // The secret never appears in state.
    let st = c.call("GET", "/v1/state", None).unwrap().to_string();
    assert!(!st.contains(&secret));
    // Renewing needs the secret (R1): 403 without it.
    let e = c
        .request(
            "POST",
            "/v1/lease",
            Some(&json!({ "ticket": g["ticket"], "ttl_secs": 1 })),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
    assert_eq!(
        (e.status, e.code.as_str()),
        (403, "release_secret_required")
    );
    // A renewal does not hand it out again.
    let again = c
        .call(
            "POST",
            "/v1/lease",
            Some(&json!({ "ticket": g["ticket"], "secret": secret })),
        )
        .unwrap();
    assert!(again.get("release_secret").is_none(), "{again}");
    let e = c
        .request(
            "POST",
            "/v1/release",
            Some(&json!({ "ticket": g["ticket"], "secret": "0".repeat(32) })),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
    assert_eq!(e.code, "wrong_release_secret");
    let rel = c
        .call(
            "POST",
            "/v1/release",
            Some(&json!({ "ticket": g["ticket"], "secret": secret })),
        )
        .unwrap();
    assert_eq!(rel["released"], true);
    assert!(rel.get("admin").is_none(), "{rel}");
    // R14: the full token releases a lease without its secret (its holder
    // crashed); the release is journaled as an admin release.
    let g = c
        .call(
            "POST",
            "/v1/lease",
            Some(&json!({ "label": "crashed agent", "ttl_secs": 600 })),
        )
        .unwrap();
    let rel = c
        .call(
            "POST",
            "/v1/release",
            Some(&json!({ "ticket": g["ticket"] })),
        )
        .unwrap();
    assert_eq!(
        (rel["released"].as_bool(), rel["admin"].as_bool()),
        (Some(true), Some(true))
    );
    let st = c.call("GET", "/v1/state", None).unwrap();
    assert!(st["slots"]["active"].as_array().unwrap().is_empty());
    assert!(st["actions"].as_array().unwrap().iter().any(|a| a["detail"]
        .as_str()
        .is_some_and(|d| d.starts_with("Admin release"))));
    // Unknown tickets are simply not released.
    let rel = c
        .call("POST", "/v1/release", Some(&json!({ "ticket": 999_999 })))
        .unwrap();
    assert_eq!(rel["released"], false);
}

/// The read-only token may queue its own TTL build slot and release it with
/// the secret; it may not bind a lease to a pid.
#[test]
fn the_read_only_token_takes_and_releases_ttl_leases() {
    let r = running(Limits::default());
    let ro = format!("Bearer {}", read_read_only_token(r.engine.paths()).unwrap());
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/lease",
        Some(&ro),
        r#"{"label":"agent build","ttl_secs":600}"#,
    );
    assert_eq!(code, 200, "{v}");
    let secret = v["release_secret"].as_str().unwrap().to_string();
    let ticket = v["ticket"].as_u64().unwrap();
    let body = format!(r#"{{"pid": {}, "ttl_secs": 60}}"#, std::process::id());
    let (code, v) = send(r.port, "POST", "/v1/lease", Some(&ro), &body);
    assert_eq!((code, v["code"].as_str()), (403, Some("read_only_token")));
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/release",
        Some(&ro),
        &format!(r#"{{"ticket": {ticket}}}"#),
    );
    assert_eq!(
        (code, v["code"].as_str()),
        (403, Some("release_secret_required"))
    );
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/release",
        Some(&ro),
        &format!(r#"{{"ticket": {ticket}, "secret": "{secret}"}}"#),
    );
    assert_eq!((code, v["released"].as_bool()), (200, Some(true)));
}

/// Lease labels are redacted before they reach state.
#[test]
fn lease_labels_are_redacted() {
    let (_t, e) = engine();
    e.acquire(&LeaseRequest {
        label: "deploy --token ghp_ABCDEFGHIJKLMNOPQRSTUVWX".into(),
        ttl_secs: Some(60),
        ..Default::default()
    })
    .unwrap();
    let st = serde_json::to_string(&e.snapshot_state()).unwrap();
    assert!(!st.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWX"), "{st}");
    assert!(st.contains("deploy --token [redacted]"));
}

// -------------------------------------------------------- read-only token

#[test]
fn the_read_only_token_reads_and_plans_only() {
    let r = running(Limits::default());
    let ro = read_read_only_token(r.engine.paths()).unwrap();
    assert_ne!(ro, r.token, "a separate secret");
    let mode = std::fs::metadata(read_only_token_path_in(r.engine.paths()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let auth = format!("Bearer {ro}");
    for (method, path) in ROUTES {
        let (code, v) = send(r.port, method, path, Some(&auth), "{}");
        if read_only_allowed(method, path) {
            assert!(code == 200 || code == 400, "{method} {path}: {code} {v}");
            assert_ne!(v["code"], "read_only_token", "{method} {path}");
        } else {
            assert_eq!(
                (code, v["code"].as_str()),
                (403, Some("read_only_token")),
                "{method} {path}"
            );
        }
    }
    // The plan routes work with it; executing does not.
    let target = cache(&r.code_root(), "p");
    let body = json!({ "paths": [target] }).to_string();
    let (code, plan) = send(r.port, "POST", "/v1/clean/plan", Some(&auth), &body);
    assert_eq!(code, 200, "{plan}");
    assert_eq!(plan["items"][0]["ready"], true);
    let items = json!({ "items": [plan["items"][0]["request"]] }).to_string();
    let (code, _) = send(r.port, "POST", "/v1/clean", Some(&auth), &items);
    assert_eq!(code, 403);
    assert!(target.exists());
    // The full token still works for everything.
    let full = format!("Bearer {}", r.token);
    let (code, _) = send(r.port, "POST", "/v1/clean", Some(&full), &items);
    assert_eq!(code, 200);
    assert!(!target.exists());
}

#[test]
fn read_only_route_list() {
    assert!(read_only_allowed("GET", "/v1/state"));
    assert!(read_only_allowed("GET", "/v1/sweep"));
    assert!(read_only_allowed("POST", "/v1/clean/plan"));
    assert!(read_only_allowed("POST", "/v1/worktree/plan"));
    assert!(read_only_allowed("POST", "/v1/process/plan"));
    assert!(read_only_allowed("POST", "/v1/lease"));
    assert!(read_only_allowed("POST", "/v1/release"));
    for p in [
        "/v1/process/execute",
        "/v1/clean",
        "/v1/sweep",
        "/v1/worktree/remove",
        "/v1/prune",
        "/v1/process",
        "/v1/workspace",
        "/v1/scan",
    ] {
        assert!(!read_only_allowed("POST", p), "{p}");
    }
}

#[test]
fn a_loose_read_only_token_is_refused() {
    let (_t, e) = engine();
    let p = e.paths().clone();
    load_or_create_read_only_token_in(&p).unwrap();
    let f = read_only_token_path_in(&p);
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_read_only_token(&p).is_err());
    assert!(load_or_create_read_only_token_in(&p).is_err());
}

#[test]
fn the_read_only_client_cannot_change_anything() {
    let r = running(Limits::default());
    let c = Client::connect_read_only_in(r.engine.paths(), r.port).unwrap();
    assert!(c.call("GET", "/v1/state", None).is_ok());
    let e = c
        .request(
            "POST",
            "/v1/sweep",
            Some(&json!({ "auto": true })),
            DEFAULT_TIMEOUT,
        )
        .unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (403, "read_only_token"));
}

/// C7: a client trickling bytes at the 503 cannot hold the accept thread past
/// the overall budget.
#[test]
fn turning_away_a_trickling_client_is_bounded() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = stop.clone();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
            if c.write_all(b"x").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    let (s, _) = l.accept().unwrap();
    let t = Instant::now();
    reject_busy(&s);
    let took = t.elapsed();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    client.join().unwrap();
    assert!(took < Duration::from_millis(400), "took {took:?}");
}

/// Process plans with the read token; executing needs the full token and only
/// takes process requests.
#[test]
fn process_plans_and_execution_over_http() {
    let r = running(Limits::default());
    let ro = format!("Bearer {}", read_read_only_token(r.engine.paths()).unwrap());
    let full = format!("Bearer {}", r.token);
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/process/plan",
        Some(&ro),
        r#"{"pid": 4000123, "signal": "kill"}"#,
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["refused"], "unknown_pid");
    assert_eq!(v["ready"], false);
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/process/plan",
        Some(&ro),
        r#"{"group": "system", "action": "quit"}"#,
    );
    assert_eq!(code, 200);
    assert_eq!(v["refused"], "unknown_group", "{v}");
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/process/plan",
        Some(&ro),
        r#"{"group": "x", "action": "explode"}"#,
    );
    assert_eq!((code, v["code"].as_str()), (400, Some("invalid_body")));

    let items = json!({ "items": [{"action": "signal", "pid": 4000123, "signal": "term", "expected_start_time": 1}] }).to_string();
    let (code, v) = send(r.port, "POST", "/v1/process/execute", Some(&ro), &items);
    assert_eq!((code, v["code"].as_str()), (403, Some("read_only_token")));
    let (code, v) = send(r.port, "POST", "/v1/process/execute", Some(&full), &items);
    assert_eq!(code, 200, "{v}");
    assert_eq!(v[0]["status"], "skipped");
    assert_eq!(v[0]["reason"], "process_gone");
    let bad = json!({ "items": [{"action": "delete_cache", "path": "/tmp/x"}] }).to_string();
    let (code, v) = send(r.port, "POST", "/v1/process/execute", Some(&full), &bad);
    assert_eq!((code, v["code"].as_str()), (400, Some("invalid_item")));
}

/// With expected_start_time, /v1/process answers a skip with 200 and a reason;
/// without it, the old shape (400 action_refused) is unchanged.
#[test]
fn process_route_takes_an_expected_start_time() {
    let r = running(Limits::default());
    let full = format!("Bearer {}", r.token);
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/process",
        Some(&full),
        r#"{"pid": 4000321, "action": "term", "expected_start_time": 5}"#,
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["result"]["status"], "skipped");
    assert_eq!(v["result"]["reason"], "process_gone");
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/process",
        Some(&full),
        r#"{"pid": 4000321, "action": "term"}"#,
    );
    assert_eq!((code, v["code"].as_str()), (400, Some("action_refused")));
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/workspace",
        Some(&full),
        r#"{"path": "/nope", "action": "pause", "pids": [{"pid": 4000321, "expected_start_time": 5}]}"#,
    );
    assert_eq!((code, v["code"].as_str()), (400, Some("action_refused")));
}

// ------------------------------------------------------- findings, config

#[test]
fn findings_routes_with_both_tokens() {
    let r = running(Limits::default());
    let ro = format!("Bearer {}", read_read_only_token(r.engine.paths()).unwrap());
    let full = format!("Bearer {}", r.token);
    // A broken posture to find: the config dir open to others.
    std::fs::set_permissions(
        &r.engine.paths().config_dir,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let found = r.engine.run_watch();
    let id = found
        .iter()
        .find(|f| f.kind == crate::watch::FindingKind::InsecurePermissions)
        .unwrap()
        .id
        .clone();
    std::fs::set_permissions(
        &r.engine.paths().config_dir,
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();

    let (code, v) = send(r.port, "GET", "/v1/findings", Some(&ro), "");
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["counts"]["high"], 1);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["findings"][0]["kind"], "insecure_permissions");
    let (code, v) = send(
        r.port,
        "GET",
        "/v1/findings?severity=high&kind=insecure_permissions",
        Some(&ro),
        "",
    );
    assert_eq!((code, v["findings"].as_array().unwrap().len()), (200, 1));
    let (_, v) = send(
        r.port,
        "GET",
        "/v1/findings?kind=exposed_listener",
        Some(&ro),
        "",
    );
    assert!(v["findings"].as_array().unwrap().is_empty());
    let (code, v) = send(r.port, "GET", "/v1/findings?severity=dire", Some(&ro), "");
    assert_eq!((code, v["code"].as_str()), (400, Some("invalid_query")));

    let body = json!({ "id": id, "days": 7 }).to_string();
    let (code, v) = send(r.port, "POST", "/v1/findings/dismiss", Some(&ro), &body);
    assert_eq!((code, v["code"].as_str()), (403, Some("read_only_token")));
    let (code, v) = send(r.port, "POST", "/v1/findings/dismiss", Some(&full), &body);
    assert_eq!(code, 200, "{v}");
    assert!(v["until_unix"].as_u64().unwrap() > crate::monitor::now_unix());
    let (_, v) = send(r.port, "GET", "/v1/findings", Some(&ro), "");
    assert!(v["findings"].as_array().unwrap().is_empty());
    assert_eq!(v["counts"]["dismissed"], 1);
    let (_, v) = send(r.port, "GET", "/v1/findings?dismissed=1", Some(&ro), "");
    assert_eq!(v["findings"][0]["dismissed"], true);
    let (code, _) = send(r.port, "POST", "/v1/findings/undismiss", Some(&ro), &body);
    assert_eq!(code, 403);
    let (code, v) = send(r.port, "POST", "/v1/findings/undismiss", Some(&full), &body);
    assert_eq!((code, v["was_dismissed"].as_bool()), (200, Some(true)));
    let (code, v) = send(
        r.port,
        "POST",
        "/v1/findings/dismiss",
        Some(&full),
        r#"{"id":"x","days":99999}"#,
    );
    assert_eq!((code, v["code"].as_str()), (400, Some("invalid_body")));
}

#[test]
fn process_detail_route() {
    let r = running(Limits::default());
    let ro = format!("Bearer {}", read_read_only_token(r.engine.paths()).unwrap());
    let (code, v) = send(r.port, "GET", "/v1/process/abc", Some(&ro), "");
    assert_eq!((code, v["code"].as_str()), (400, Some("invalid_pid")));
    let (code, v) = send(r.port, "GET", "/v1/process/4000000", Some(&ro), "");
    assert_eq!((code, v["code"].as_str()), (404, Some("unknown_pid")));
    // The plan route is still a POST route.
    let (code, v) = send(r.port, "GET", "/v1/process/plan", Some(&ro), "");
    assert_eq!(
        (code, v["code"].as_str()),
        (405, Some("method_not_allowed"))
    );
    let (code, _) = send(r.port, "POST", "/v1/process/12", Some(&ro), "{}");
    assert_eq!(code, 403);
}

#[test]
fn config_routes_with_both_tokens() {
    let r = running(Limits::default());
    let ro = format!("Bearer {}", read_read_only_token(r.engine.paths()).unwrap());
    let full = format!("Bearer {}", r.token);
    let (code, v) = send(r.port, "GET", "/v1/config", Some(&ro), "");
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["config"]["watch"]["interval_secs"], 60);
    assert!(v["config_error"].is_null());
    let mut cfg = v["config"].clone();
    cfg["watch"]["stale_process_hours"] = json!(24);
    let body = json!({ "config": cfg }).to_string();
    let (code, v) = send(r.port, "PUT", "/v1/config", Some(&ro), &body);
    assert_eq!((code, v["code"].as_str()), (403, Some("read_only_token")));
    let (code, v) = send(r.port, "PUT", "/v1/config", Some(&full), "{}");
    assert_eq!((code, v["code"].as_str()), (400, Some("config_required")));
    let mut bad = cfg.clone();
    bad["watch"]["interval_secs"] = json!(1);
    let (code, v) = send(
        r.port,
        "PUT",
        "/v1/config",
        Some(&full),
        &json!({ "config": bad }).to_string(),
    );
    assert_eq!(
        (code, v["code"].as_str()),
        (400, Some("invalid_config")),
        "{v}"
    );
    assert_eq!(r.engine.config().watch.interval_secs, 60, "unchanged");
    let (code, v) = send(r.port, "PUT", "/v1/config", Some(&full), &body);
    assert_eq!(code, 200, "{v}");
    assert_eq!(r.engine.config().watch.stale_process_hours, 24);
    assert_eq!(
        Config::load_from(r.engine.paths())
            .config
            .watch
            .stale_process_hours,
        24,
        "saved"
    );
}

/// R16: a client that hangs up before the reply does not stop the request:
/// it runs to the end and its result is journaled, and the server goes on.
#[test]
fn an_execute_whose_client_hung_up_still_runs_and_is_recorded() {
    use std::io::Write;
    let r = running(Limits::default());
    let body =
        r#"{"items":[{"action":"signal","pid":4000123,"signal":"term","expected_start_time":7}]}"#;
    let mut s = TcpStream::connect(("127.0.0.1", r.port)).unwrap();
    write!(
        s,
        "POST /v1/process/execute HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
        r.port,
        r.token,
        body.len()
    )
    .unwrap();
    s.flush().unwrap();
    drop(s);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let st = r.engine.snapshot_state();
        if st
            .actions
            .iter()
            .any(|a| a.target.as_deref().is_some_and(|t| t.contains("4000123")))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "not recorded");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (code, _) = send(r.port, "GET", "/v1/health", None, "");
    assert_eq!(code, 200);
}
