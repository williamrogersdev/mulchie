//! The `mulchie` binary, run with HOME set to a fresh temp directory and
//! MULCHIE_PORT set to a free port, so nothing touches the real home or a
//! running engine.

use mulchie_core::{api, Config, Engine, Paths};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Home {
    _t: tempfile::TempDir,
    home: PathBuf,
    port: u16,
}

impl Home {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Self { _t: t, home, port }
    }

    /// The paths the binary computes from this HOME.
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

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mulchie"));
        c.args(args)
            .env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env("MULCHIE_PORT", self.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// An engine for this HOME in this process, as `serve` would run it.
    fn engine(&self) -> (Engine, mulchie_core::lock::EngineLock, api::Server) {
        let paths = self.paths();
        let engine = Engine::new(paths.clone(), Config::load_from(&paths));
        let mut lk = engine.acquire_lock().unwrap();
        let server = api::serve(engine.clone(), 0).unwrap();
        lk.set_port(Some(server.port())).unwrap();
        (engine, lk, server)
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn wait_timeout(child: &mut Child, limit: Duration) -> Output {
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            panic!("timed out after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    use std::io::Read;
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.stderr.take().unwrap().read_to_end(&mut err).unwrap();
    Output {
        status: child.wait().unwrap(),
        stdout: out,
        stderr: err,
    }
}

fn signal(pid: u32, sig: i32) {
    assert_eq!(unsafe { libc::kill(pid as i32, sig) }, 0);
}

#[test]
fn version() {
    let h = Home::new();
    let o = h.run(&["--version"]);
    assert!(o.status.success());
    assert_eq!(
        text(&o.stdout).trim(),
        format!("mulchie {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn bad_usage_exits_2() {
    let h = Home::new();
    for args in [
        &["--bogus"][..],
        &["status", "--jsno"],
        &["sweep", "now"],
        &["frob"],
    ] {
        let o = h.run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(!text(&o.stderr).is_empty());
    }
}

#[test]
fn every_subcommand_has_help() {
    let h = Home::new();
    for sub in [
        "status",
        "scan",
        "clean",
        "top",
        "worktrees",
        "sweep",
        "prune",
        "signal",
        "advice",
        "run",
        "mcp",
        "serve",
        "config",
        "doctor",
        "service",
        "completions",
    ] {
        let o = h.run(&[sub, "--help"]);
        assert!(o.status.success(), "{sub}");
        assert!(text(&o.stdout).contains("Usage: mulchie"), "{sub}");
    }
    for sub in ["install", "uninstall", "status"] {
        let o = h.run(&["service", sub, "--help"]);
        assert!(o.status.success(), "service {sub}");
    }
}

#[test]
fn completions() {
    let h = Home::new();
    let o = h.run(&["completions", "zsh"]);
    assert!(o.status.success());
    assert!(text(&o.stdout).contains("#compdef mulchie"));
    let o = h.run(&["completions", "bash"]);
    assert!(text(&o.stdout).contains("_mulchie"));
}

#[test]
fn run_without_engine_says_so_and_keeps_the_status() {
    let h = Home::new();
    let o = h.run(&["run", "--", "sh", "-c", "echo out; exit 7"]);
    assert_eq!(o.status.code(), Some(7));
    assert_eq!(text(&o.stdout), "out\n");
    let err = text(&o.stderr);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.contains("no engine is running") && err.contains("unqueued"),
        "{err}"
    );

    let o = h.run(&["run", "sh", "-c", "kill -KILL $$"]);
    assert_eq!(o.status.code(), Some(128 + libc::SIGKILL));
    let o = h.run(&["run", "--", "/nonexistent/thing"]);
    assert_eq!(o.status.code(), Some(127));
}

#[test]
fn run_forwards_sigterm_and_sigint() {
    let h = Home::new();
    // SIGTERM: `sleep` dies of it, so the status is 128 + 15.
    let mut c = h.cmd(&["run", "--", "sleep", "30"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(400));
    signal(c.id(), libc::SIGTERM);
    let o = wait_timeout(&mut c, Duration::from_secs(10));
    assert_eq!(
        o.status.code(),
        Some(128 + libc::SIGTERM),
        "{}",
        text(&o.stderr)
    );

    // SIGINT reaches the child, which decides its own exit status.
    let mut c = h
        .cmd(&[
            "run",
            "--",
            "sh",
            "-c",
            "trap 'exit 42' INT; sleep 5 & wait",
        ])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    signal(c.id(), libc::SIGINT);
    let o = wait_timeout(&mut c, Duration::from_secs(10));
    assert_eq!(o.status.code(), Some(42), "{}", text(&o.stderr));
}

#[test]
fn run_takes_and_releases_a_lease() {
    let h = Home::new();
    let (engine, _lk, server) = h.engine();
    let o = h.run(&[
        "run",
        "--label",
        "t",
        "--",
        "sh",
        "-c",
        "echo jobs=$CARGO_BUILD_JOBS",
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.starts_with("jobs=") && out.trim().len() > 5, "{out}");
    assert!(engine.snapshot_state().slots.active.is_empty());

    // Released when the run is terminated too.
    let mut c = h.cmd(&["run", "--", "sleep", "30"]).spawn().unwrap();
    let start = Instant::now();
    loop {
        let st = engine.snapshot_state();
        if let Some(l) = st.slots.active.first() {
            assert_eq!(l.pid, Some(c.id()), "leased against mulchie run's own pid");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "no lease taken");
        std::thread::sleep(Duration::from_millis(50));
    }
    signal(c.id(), libc::SIGTERM);
    let o = wait_timeout(&mut c, Duration::from_secs(10));
    assert_eq!(o.status.code(), Some(128 + libc::SIGTERM));
    assert!(engine.snapshot_state().slots.active.is_empty());
    assert!(engine.snapshot_state().slots.waiting.is_empty());
    server.stop();
}

#[test]
fn read_commands_say_when_they_work_locally() {
    let h = Home::new();
    let o = h.run(&["advice", "--json"]);
    assert!(o.status.success());
    assert!(
        text(&o.stderr).contains("no engine is running"),
        "{}",
        text(&o.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["reasons"].is_array());

    let o = h.run(&["signal", "999999", "term"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("needs a running engine"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn read_commands_use_the_engine() {
    let h = Home::new();
    let (engine, _lk, server) = h.engine();
    engine.set_snapshot(mulchie_core::Snapshot {
        at_unix: mulchie_core::monitor::now_unix(),
        cpu_count: 8,
        ..Default::default()
    });
    let o = h.run(&["status", "--json"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stderr).is_empty(), "{}", text(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["snapshot"]["cpu_count"], 8,
        "came from the engine, not a local sample"
    );
    server.stop();
}

#[test]
fn sweep_refuses_without_a_terminal() {
    let h = Home::new();
    let o = h.run(&["sweep"]);
    // Nothing to sweep in an empty home, so nothing to confirm either.
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("no engine is running"));
    assert!(text(&o.stdout).contains("No idle agent worktrees"));
}

#[test]
fn doctor_reports_and_changes_nothing() {
    let h = Home::new();
    let o = h.run(&["doctor"]);
    let out = text(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{out}");
    for name in [
        "config",
        "config dir",
        "token",
        "engine",
        "git",
        "service",
        "mcp",
    ] {
        assert!(out.contains(&format!("  {name}")), "{name}: {out}");
    }
    assert!(!h.home.join(".config").exists(), "doctor created files");

    let cfg = h.home.join(".config/mulchie");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.json"), "{\"worktree_idle_days\": 0}").unwrap();
    let o = h.run(&["doctor", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ok"], false);
    let c = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "config")
        .unwrap();
    assert_eq!(c["level"], "fail");
    assert!(
        c["detail"].as_str().unwrap().contains("worktree_idle_days"),
        "{c}"
    );
}

fn unit_file(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/dev.mulchie.serve.plist")
    } else {
        home.join(".config/systemd/user/mulchie.service")
    }
}

#[test]
fn service_install_dry_run_and_refusal() {
    let h = Home::new();
    let o = h.run(&["service", "install", "--dry-run"]);
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out}{}", text(&o.stderr));
    let file = unit_file(&h.home);
    assert!(
        out.contains(&format!("Would write {}", file.display())),
        "{out}"
    );
    assert!(
        out.contains("<string>serve</string>") || out.contains(" serve\n"),
        "{out}"
    );
    if cfg!(target_os = "macos") {
        assert!(out.contains("launchctl bootstrap gui/"), "{out}");
    } else {
        assert!(
            out.contains("systemctl --user enable mulchie.service"),
            "{out}"
        );
    }
    assert!(!file.exists(), "a dry run wrote the file");

    // A different file is not replaced without --force (refused before any
    // service manager command runs).
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "something else\n").unwrap();
    let o = h.run(&["service", "install"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("--force"), "{}", text(&o.stderr));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "something else\n");

    // Uninstall refuses a file that is not ours, in a dry run as well.
    let o = h.run(&["service", "uninstall", "--dry-run"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    assert!(file.exists());

    let o = h.run(&["service", "status", "--json"]);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["installed"], true);
    assert_eq!(v["current"], false);
}

#[test]
fn mcp_over_stdio() {
    let h = Home::new();
    let mut c = h.cmd(&["mcp"]).stdin(Stdio::piped()).spawn().unwrap();
    {
        let mut stdin = c.stdin.take().unwrap();
        for m in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_status","arguments":{}}}"#,
        ] {
            writeln!(stdin, "{m}").unwrap();
        }
    }
    let o = wait_timeout(&mut c, Duration::from_secs(10));
    assert!(o.status.success());
    let lines: Vec<serde_json::Value> = text(&o.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["result"]["isError"], true);
    let t = lines[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(t.contains("mulchie serve"), "{t}");
}
