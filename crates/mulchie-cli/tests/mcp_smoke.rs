//! MCP smoke test over real stdio: the built `mulchie mcp` binary, a temp
//! HOME and an engine serving that HOME on a free port. Runs in CI on macOS
//! and Linux, and locally with
//! `cargo test -p mulchie --test mcp_smoke`.
//!
//! Covers: initialize, tools/list, diagnose (with and without an engine), a
//! plan call, plan-then-execute refusing items outside the plan, and the
//! read-only server leaving out and refusing destructive tools, build slots
//! with the release secret kept in the server, findings, process detail with
//! secrets redacted, and a process plan carried out by execute_plan.

use mulchie_core::{api, Config, Engine, Paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Before a test engine starts: no build throttling, no limit budgets and no
/// update checks in the temp home's config, so it can neither renice the
/// user's real processes nor reach the network.
fn safe_config(paths: &Paths) -> mulchie_core::LoadedConfig {
    let mut cfg = Config::load_from(paths).config;
    cfg.throttle_builds = false;
    cfg.limits.default = None;
    cfg.limits.workspaces.clear();
    cfg.updates.check = mulchie_core::config::UpdateCheck::Off;
    cfg.save_to(paths, 0).expect("write the test config");
    Config::load_from(paths)
}

struct Home {
    _t: tempfile::TempDir,
    home: PathBuf,
}

impl Home {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self { _t: t, home }
    }

    fn paths(&self) -> Paths {
        let config_dir = self.home.join(".config/mulchie");
        Paths {
            home: self.home.clone(),
            backups_dir: config_dir.join("backups"),
            config_dir,
            temp_dir: std::env::temp_dir(),
            xdg_config_home: None,
        }
    }

    /// An engine for this HOME in this process: the API, no loops.
    fn engine(&self) -> (Engine, mulchie_core::lock::EngineLock, api::Server) {
        let paths = self.paths();
        let engine = Engine::new(paths.clone(), safe_config(&paths));
        let mut lk = engine.acquire_lock().unwrap();
        let server = api::serve(engine.clone(), 0).unwrap();
        lk.set_port(Some(server.port())).unwrap();
        (engine, lk, server)
    }

    fn server(&self, args: &[&str], env: &[(&str, &str)]) -> Server {
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut c = Command::new(env!("CARGO_BIN_EXE_mulchie"));
        c.args(args)
            .env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MULCHIE_MCP_READ_ONLY")
            .env("MULCHIE_PORT", free.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || reader(out, tx));
        Server {
            child,
            stdin,
            rx,
            next: 0,
        }
    }
}

fn reader(out: ChildStdout, tx: mpsc::Sender<Value>) {
    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { return };
        let v: Value = serde_json::from_str(&line).expect("every line is JSON");
        if tx.send(v).is_err() {
            return;
        }
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next: u64,
}

impl Server {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        // A local diagnosis samples and scans, so allow a while.
        let r = self
            .rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| panic!("no reply to {method}"));
        assert_eq!(r["id"], id, "{r}");
        r
    }

    fn notify(&mut self, method: &str) {
        writeln!(
            self.stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "method": method })
        )
        .unwrap();
    }

    fn initialize(&mut self) -> Value {
        let r = self.request(
            "initialize",
            json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "smoke", "version": "1" } }),
        );
        self.notify("notifications/initialized");
        r["result"].clone()
    }

    fn tools(&mut self) -> Vec<String> {
        let r = self.request("tools/list", json!({}));
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// (isError, the result parsed as JSON when it is JSON, else as text)
    fn call(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let res = &r["result"];
        let text = res["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"));
        let v = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (res["isError"] == true, v)
    }

    fn finish(mut self) {
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }
}

#[test]
fn read_write_server_with_an_engine() {
    let h = Home::new();
    let (engine, _lk, server) = h.engine();
    let mut s = h.server(&["mcp"], &[]);

    let init = s.initialize();
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert_eq!(init["serverInfo"]["name"], "mulchie");
    assert!(init["instructions"]
        .as_str()
        .unwrap()
        .contains("Start with the diagnose tool"));

    let tools = s.tools();
    assert_eq!(tools[0], "diagnose");
    for t in [
        "execute_plan",
        "plan_worktree_sweep",
        "plan_cache_cleanup",
        "plan_process_action",
        "list_findings",
        "inspect_process",
        "dismiss_finding",
    ] {
        assert!(tools.contains(&t.to_string()), "{t}");
    }

    let (err, d) = s.call("diagnose", json!({}));
    assert!(!err, "{d}");
    assert_eq!(d["source"], "engine");
    for k in [
        "summary",
        "health",
        "recommendations",
        "reclaimable",
        "build_queue",
    ] {
        assert!(d.get(k).is_some(), "diagnose lacks {k}: {d}");
    }

    let (err, plan) = s.call("plan_worktree_sweep", json!({}));
    assert!(!err, "{plan}");
    assert!(plan["ready"].is_array());

    // Nothing outside a plan can be executed.
    let (err, msg) = s.call(
        "execute_plan",
        json!({ "plan_id": "made-up", "items": [h.home.join("x").to_string_lossy()] }),
    );
    assert!(err, "{msg}");

    let (err, slot) = s.call(
        "acquire_build_slot",
        json!({ "label": "smoke", "ttl_secs": 30 }),
    );
    assert!(!err, "{slot}");
    // The release secret stays in the server; release sends it.
    assert!(!slot.to_string().contains("secret"), "{slot}");
    let (err, r) = s.call("release_build_slot", json!({ "ticket": slot["ticket"] }));
    assert!(!err, "{r}");
    assert!(engine.snapshot_state().slots.active.is_empty());

    // Findings: observations, with the note.
    let (err, f) = s.call("list_findings", json!({ "severity": "low" }));
    assert!(!err, "{f}");
    assert!(f["findings"].is_array());
    assert!(f["note"].as_str().unwrap().contains("not verdicts"));

    // A process of ours with a secret on its command line.
    let mut child = Command::new("sh")
        .args(["-c", "sleep 30; exit 0", "smoke", "--password", "hunter2"])
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::sleep(Duration::from_millis(200));
    let (err, d) = s.call("inspect_process", json!({ "pid": pid }));
    assert!(!err, "{d}");
    assert_eq!(d["pid"], pid);
    assert!(!d.to_string().contains("hunter2"), "a secret leaked: {d}");
    assert!(
        d.to_string().contains(mulchie_core::redact::REDACTED),
        "{d}"
    );

    // Plan, then execute: the process is signalled only by execute_plan.
    let (err, plan) = s.call(
        "plan_process_action",
        json!({ "pid": pid, "signal": "term" }),
    );
    assert!(!err, "{plan}");
    assert!(
        child.try_wait().unwrap().is_none(),
        "planning changed nothing"
    );
    let id = plan["plan_id"].as_str().unwrap_or_else(|| panic!("{plan}"));
    let target = plan["ready"][0]["target"].as_str().unwrap().to_string();
    let (err, done) = s.call("execute_plan", json!({ "plan_id": id, "items": [target] }));
    assert!(!err, "{done}");
    let status = child.wait().unwrap();
    assert!(!status.success(), "the process was signalled");

    let r = s.request("prompts/list", json!({}));
    assert_eq!(r["result"]["prompts"].as_array().unwrap().len(), 2);
    let r = s.request("resources/read", json!({ "uri": "mulchie://actions" }));
    assert!(r["result"]["contents"][0]["text"].is_string(), "{r}");

    s.finish();
    server.stop();
}

#[test]
fn read_only_server_hides_and_refuses_destructive_tools() {
    let h = Home::new();
    let (_engine, _lk, server) = h.engine();
    for (args, env) in [
        (vec!["mcp", "--read-only"], vec![]),
        (vec!["mcp"], vec![("MULCHIE_MCP_READ_ONLY", "1")]),
    ] {
        let mut s = h.server(&args, &env);
        let init = s.initialize();
        assert!(init["instructions"].as_str().unwrap().contains("read-only"));
        let tools = s.tools();
        for t in ["execute_plan", "prune_cache", "dismiss_finding"] {
            assert!(!tools.contains(&t.to_string()), "{args:?} lists {t}");
        }
        assert!(tools.contains(&"diagnose".to_string()));
        assert!(tools.contains(&"plan_cache_cleanup".to_string()));

        let (err, msg) = s.call("prune_cache", json!({ "key": "npm" }));
        assert!(err && msg.as_str().unwrap().contains("read-only"), "{msg}");
        let (err, _) = s.call("execute_plan", json!({ "plan_id": "x", "items": ["/x"] }));
        assert!(err);
        let (err, plan) = s.call("plan_cache_cleanup", json!({ "stale": true }));
        assert!(!err, "{plan}");
        assert!(plan.get("plan_id").is_none());
        let (err, d) = s.call("diagnose", json!({}));
        assert!(!err, "{d}");
        // The read-only token still takes and releases TTL build slots.
        let (err, slot) = s.call("acquire_build_slot", json!({ "ttl_secs": 30 }));
        assert!(!err, "{slot}");
        let (err, r) = s.call("release_build_slot", json!({ "ticket": slot["ticket"] }));
        assert!(!err, "{r}");
        // Plans are shown, never stored.
        let (err, plan) = s.call(
            "plan_process_action",
            json!({ "pid": std::process::id(), "signal": "stop" }),
        );
        assert!(!err, "{plan}");
        assert!(plan.get("plan_id").is_none(), "{plan}");
        s.finish();
    }
    server.stop();
}

#[test]
fn diagnose_without_an_engine_looks_locally() {
    let h = Home::new();
    let mut s = h.server(&["mcp"], &[]);
    s.initialize();
    let (err, d) = s.call("diagnose", json!({}));
    assert!(!err, "{d}");
    assert_eq!(d["source"], "local");
    assert!(d["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == "start_engine"));
    // Tools that need the engine say how to start it.
    let (err, msg) = s.call("plan_worktree_sweep", json!({}));
    assert!(
        err && msg.as_str().unwrap().contains("mulchie serve"),
        "{msg}"
    );
    s.finish();
}

/// Wait until `f` holds, for at most five seconds.
fn eventually(f: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_server_releases_its_slots_when_it_ends() {
    let h = Home::new();
    let (engine, _lk, server) = h.engine();
    let held = || engine.snapshot_state().slots.active.len();
    // Ended by EOF on stdin.
    let mut s = h.server(&["mcp"], &[]);
    s.initialize();
    let (err, slot) = s.call("acquire_build_slot", json!({ "ttl_secs": 600 }));
    assert!(!err, "{slot}");
    assert_eq!(held(), 1);
    s.finish();
    assert!(eventually(|| held() == 0), "slot kept after EOF");
    // Ended by SIGTERM, as a client does when it closes.
    let mut s = h.server(&["mcp"], &[]);
    s.initialize();
    let (err, slot) = s.call("acquire_build_slot", json!({ "ttl_secs": 600 }));
    assert!(!err, "{slot}");
    assert_eq!(held(), 1);
    assert_eq!(unsafe { libc::kill(s.child.id() as i32, libc::SIGTERM) }, 0);
    let status = s.child.wait().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    assert!(eventually(|| held() == 0), "slot kept after SIGTERM");
    server.stop();
}

#[test]
fn a_read_only_server_without_the_read_only_token_says_how_to_fix_it() {
    let h = Home::new();
    let (_engine, _lk, server) = h.engine();
    let token = api::read_only_token_path_in(&h.paths());
    let _ = std::fs::remove_file(&token);
    let mut s = h.server(&["mcp", "--read-only"], &[]);
    s.initialize();
    let (err, d) = s.call("diagnose", json!({}));
    let t = d.to_string();
    assert!(t.contains("restart the engine"), "{err} {t}");
    // No silent local look while an engine runs.
    assert_ne!(d["source"], "local", "{t}");
    s.finish();
    server.stop();
}
