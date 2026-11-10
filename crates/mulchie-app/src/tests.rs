//! Tests for the pure parts of the app: the local/remote backend against a
//! real API server on a free port with temporary paths, plan bookkeeping,
//! updater detection and path checks.

use super::*;
use mulchie_core::process::FakeProcesses;
use mulchie_core::safety::CACHEDIR_TAG_SIGNATURE;
use mulchie_core::Config;
use std::collections::HashMap;

struct Fixture {
    t: tempfile::TempDir,
    paths: Paths,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let root = t.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    Fixture { t, paths, root }
}

impl Fixture {
    fn opts(&self) -> Options {
        Options {
            port: Some(0),
            start_loops: false,
            procs: Some(Arc::new(FakeProcesses::empty())),
            config: Some(Config {
                scan_roots: vec![self.root.clone()],
                throttle_builds: false,
                ..Config::default()
            }),
            takeover_after: Duration::ZERO,
        }
    }

    fn cache(&self, rel: &str) -> PathBuf {
        let dir = self.root.join(rel);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
        std::fs::write(dir.join("target/debug/blob"), vec![1u8; 10_000]).unwrap();
        dir.join("target")
    }
}

fn clean(paths: Vec<PathBuf>) -> PlanRequest {
    PlanRequest::Clean { paths }
}

#[test]
fn first_backend_runs_the_engine_and_serves_the_api() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let info = a.info();
    assert_eq!(info.mode, Mode::Engine);
    assert_eq!(info.engine_pid, Some(std::process::id()));
    assert!(info.port.is_some_and(|p| p != 0), "{info:?}");
    assert!(info.detail.is_none(), "{info:?}");
    let holder = mulchie_core::lock::running_engine(&f.paths).unwrap();
    assert_eq!(holder.port, info.port);
    assert!(a.state().unwrap()["automation"].is_boolean());
    a.shutdown();
    assert!(mulchie_core::lock::running_engine(&f.paths).is_none());
}

#[test]
fn second_backend_is_a_client_and_acts_through_the_api() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = Backend::start(f.paths.clone(), f.opts());
    let info = b.info();
    assert_eq!(info.mode, Mode::Client, "{info:?}");
    assert_eq!(info.port, a.info().port);
    assert_eq!(info.engine_pid, Some(std::process::id()));

    let st = b.state().unwrap();
    assert_eq!(st["version"], a.state().unwrap()["version"]);
    assert!(b.history_since(0).unwrap().is_empty());
    b.rescan().unwrap();
    assert_eq!(b.tray_info().unwrap().low_disk, None);

    // Plan through the client, approve one of two items, execute it.
    let x = f.cache("x");
    let y = f.cache("y");
    let plan = b.plan(&clean(vec![x.clone(), y.clone()])).unwrap();
    assert_eq!(plan.items.len(), 2);
    assert!(plan.items.iter().all(|i| i.ready), "{plan:?}");
    let first = vec![plan.items[0].request.clone()];
    let res = b.execute(Batch::Clean, first.clone()).unwrap();
    assert_eq!(res.len(), 1);
    assert!(res[0].is_done(), "{res:?}");
    assert!(res[0].freed_bytes() > 0);
    assert_ne!(x.exists(), y.exists(), "exactly one cache is gone");

    // The same approval cannot be sent twice, and nothing outside the plan.
    assert!(b.execute(Batch::Clean, first).is_err());
    let other = Request::DeleteCache {
        path: f.root.join("elsewhere/target"),
        require_stale: false,
    };
    assert!(b
        .execute(Batch::Clean, vec![other])
        .unwrap_err()
        .contains("not a ready item"));

    // Settings save through the engine's PUT /v1/config.
    let mut cfg = a.state().unwrap()["config"].clone();
    cfg["low_disk_gb"] = json!(33);
    cfg["watch"]["allow_ids"] = json!(["runaway_cpu:1:2"]);
    b.set_config(cfg).unwrap();
    let saved = a.state().unwrap()["config"].clone();
    assert_eq!(saved["low_disk_gb"], 33);
    assert_eq!(saved["watch"]["allow_ids"][0], "runaway_cpu:1:2");
    // An invalid config is refused with the engine's reason, by both paths.
    let mut bad = saved.clone();
    bad["watch"]["runaway_minutes"] = json!(0);
    let parsed: Config = serde_json::from_value(bad.clone()).unwrap();
    assert!(parsed.validate().is_err(), "a zero window is invalid");
    assert!(b.set_config(bad.clone()).is_err());
    assert!(a.set_config(bad).is_err());
    assert_eq!(
        a.state().unwrap()["config"]["watch"]["runaway_minutes"],
        saved["watch"]["runaway_minutes"]
    );

    // The engine journaled the client's action.
    let actions = a.state().unwrap()["actions"].clone();
    assert!(actions
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["kind"] == "reclaim" && x["source"] == "api"));
    drop(b);
    a.shutdown();
}

#[test]
fn client_goes_disconnected_then_takes_over_when_the_engine_stays_gone() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            ..f.opts()
        },
    );
    assert_eq!(b.mode(), Mode::Client);
    a.shutdown();
    b.tick();
    assert_eq!(b.mode(), Mode::Disconnected);
    assert!(b.state().unwrap_err().contains("not reachable"));
    // Within the grace period it waits for the engine to come back.
    b.tick();
    assert_eq!(b.mode(), Mode::Disconnected);
    // The engine comes back: the client reconnects.
    let a2 = Backend::start(f.paths.clone(), f.opts());
    assert_eq!(a2.mode(), Mode::Engine);
    b.tick();
    assert_eq!(b.mode(), Mode::Client);
    a2.shutdown();
    b.tick();
    assert_eq!(b.mode(), Mode::Disconnected);

    // With no grace period left, it takes the lock and becomes the engine.
    let c = Backend::start(f.paths.clone(), f.opts());
    assert_eq!(c.mode(), Mode::Engine);
    let d = Backend::start(f.paths.clone(), f.opts());
    assert_eq!(d.mode(), Mode::Client);
    c.shutdown();
    d.tick();
    assert_eq!(d.mode(), Mode::Disconnected);
    d.tick();
    assert_eq!(d.mode(), Mode::Engine);
    assert!(d.state().is_ok());
    d.shutdown();
    drop(f.t);
}

#[test]
fn local_engine_plans_and_executes_and_refuses_items_of_the_wrong_kind() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let x = f.cache("x");
    let plan = a.plan(&clean(vec![x.clone()])).unwrap();
    let items = plan.approved();
    assert_eq!(items.len(), 1);
    // A cache request is not a worktree batch.
    assert!(a.execute(Batch::Worktree, items.clone()).is_err());
    let res = a.execute(Batch::Clean, items).unwrap();
    assert!(res[0].is_done(), "{res:?}");
    assert!(!x.exists());
    // A path that is not a cache plans as not ready, with a reason.
    let plan = a.plan(&clean(vec![f.root.join("nope")])).unwrap();
    assert!(!plan.items[0].ready);
    assert!(plan.items[0].reason.is_some());
    assert!(plan.approved().is_empty());
    assert!(a.tray_info().unwrap().low_disk.is_some());
    a.shutdown();
}

#[test]
fn plan_book_accepts_subsets_of_the_last_plan_only() {
    let req = |p: &str| Request::DeleteCache {
        path: PathBuf::from(p),
        require_stale: true,
    };
    let item = |r: Request, ready: bool| mulchie_core::PlanItem {
        target: r.target(),
        request: r,
        name: String::new(),
        repo_name: None,
        head: None,
        branch: None,
        bytes: 1,
        dirty_files: 0,
        idle_days: None,
        ignored_files: 0,
        ignored_bytes: 0,
        would_backup: vec![],
        ready,
        reason: None,
        verdict: String::new(),
    };
    let plan = Plan {
        created_unix: 0,
        items: vec![
            item(req("/a"), true),
            item(req("/b"), true),
            item(req("/c"), false),
        ],
        ready_bytes: 2,
    };
    let mut book = backend::PlanBook::default();
    assert!(
        book.take(Batch::Clean, &[req("/a")]).is_err(),
        "no plan yet"
    );
    book.record(Batch::Clean, &plan);
    assert!(book.take(Batch::Clean, &[]).is_err());
    assert!(book.take(Batch::Clean, &[req("/c")]).is_err(), "not ready");
    assert!(book.take(Batch::Sweep, &[req("/a")]).is_err(), "wrong kind");
    book.take(Batch::Clean, &[req("/b")]).unwrap();
    assert!(book.take(Batch::Clean, &[req("/b")]).is_err(), "used once");
    book.take(Batch::Clean, &[req("/a")]).unwrap();
    // A new plan replaces the old one.
    book.record(Batch::Clean, &plan);
    book.take(Batch::Clean, &[req("/a"), req("/b")]).unwrap();

    // Process plans are kept per target, so two can wait for approval.
    let sig = |pid| Request::Signal {
        pid,
        signal: mulchie_core::safety::Signal::Stop,
        expected_start_time: 7,
    };
    book.record_process("pid 5", vec![sig(5)]);
    book.record_process("app:/A.app", vec![sig(6), sig(8)]);
    assert!(book.take(Batch::Process, &[sig(9)]).is_err());
    assert!(book.take(Batch::Clean, &[sig(5)]).is_err(), "wrong kind");
    book.take(Batch::Process, &[sig(8), sig(5)]).unwrap();
    assert!(book.take(Batch::Process, &[sig(5)]).is_err(), "used once");
    // A new plan for a target replaces that target's items only.
    book.record_process("app:/A.app", vec![sig(8)]);
    assert!(book.take(Batch::Process, &[sig(6)]).is_err());
    book.take(Batch::Process, &[sig(8)]).unwrap();
}

#[test]
fn plan_requests_parse_from_the_ui_and_map_to_batches() {
    let p: PlanRequest =
        serde_json::from_value(json!({"kind": "worktrees", "paths": ["/w"], "allow_dirty": true}))
            .unwrap();
    assert_eq!(
        p,
        PlanRequest::Worktrees {
            paths: vec![PathBuf::from("/w")],
            allow_dirty: true
        }
    );
    assert_eq!(p.batch(), Batch::Worktree);
    let p: PlanRequest = serde_json::from_value(json!({"kind": "stale"})).unwrap();
    assert_eq!(p, PlanRequest::Stale { want_bytes: None });
    assert_eq!(p.batch(), Batch::Clean);
    let p: PlanRequest = serde_json::from_value(json!({"kind": "sweep"})).unwrap();
    assert_eq!(p.batch(), Batch::Sweep);
    let b: Batch = serde_json::from_value(json!("worktree")).unwrap();
    assert_eq!(b, Batch::Worktree);
    // A request the UI sends back is exactly the plan's.
    let r: Request = serde_json::from_value(json!({
        "action": "remove_worktree", "path": "/w", "expected_head": "abc",
        "allow_dirty": false, "require_idle": true
    }))
    .unwrap();
    assert!(Batch::Sweep.allows(&r) && !Batch::Clean.allows(&r));
}

#[test]
fn updater_is_configured_only_with_a_public_key() {
    let mut p: HashMap<String, Value> = HashMap::new();
    assert_eq!(update::pubkey(&p), None);
    assert!(!update::Updates::from_plugins(&p).configured());
    p.insert("updater".into(), json!({"endpoints": [update::ENDPOINT]}));
    assert_eq!(update::pubkey(&p), None);
    p.insert("updater".into(), json!({"pubkey": "  "}));
    assert_eq!(update::pubkey(&p), None);
    p.insert(
        "updater".into(),
        json!({"pubkey": "dW50cnVzdGVk", "endpoints": []}),
    );
    assert_eq!(update::pubkey(&p).as_deref(), Some("dW50cnVzdGVk"));
    assert!(update::Updates::from_plugins(&p).configured());
}

#[test]
fn shipped_config_has_no_updater_key_and_a_strict_script_policy() {
    let conf: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    let plugins: HashMap<String, Value> =
        serde_json::from_value(conf["plugins"].clone()).unwrap_or_default();
    // release-setup.sh adds the key; until then the build must work without it.
    assert!(update::pubkey(&plugins).is_none() || conf["bundle"]["createUpdaterArtifacts"] == true);
    let csp = conf["app"]["security"]["csp"].as_str().unwrap();
    let script = csp
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with("script-src"))
        .unwrap();
    assert!(
        !script.contains("unsafe-inline") && !script.contains("unsafe-eval"),
        "{csp}"
    );
    assert!(
        csp.contains("connect-src ipc: http://ipc.localhost"),
        "{csp}"
    );
}

#[test]
fn ui_has_no_inline_script_or_handlers() {
    let html = include_str!("../ui/index.html");
    for tag in html.match_indices("<script").map(|(i, _)| &html[i..]) {
        let open = &tag[..tag.find('>').unwrap()];
        assert!(open.contains("src="), "inline script: {open}");
        assert!(tag[open.len() + 1..].starts_with("</script>"), "{open}");
    }
    let lower = html.to_ascii_lowercase();
    for h in [
        " onclick=",
        " onload=",
        " oninput=",
        " onchange=",
        "javascript:",
    ] {
        assert!(!lower.contains(h), "inline handler {h}");
    }
}

#[test]
fn reveal_allows_reported_paths_and_backups_only() {
    let f = fixture();
    let backups = f.paths.backups_dir.clone();
    std::fs::create_dir_all(&backups).unwrap();
    let patch = backups.join("w.patch");
    std::fs::write(&patch, "x").unwrap();
    let ws = f.root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let app = f.root.join("Some.app");
    std::fs::create_dir_all(&app).unwrap();
    let st = json!({
        "snapshot": {"workspaces": [{"path": ws}], "consumers": {"by_cpu": [], "by_mem": [{"bundle_path": app}]}},
        "scan": {"items": []},
        "diskmap": null
    });
    assert!(revealable(&st, &ws, &backups));
    assert!(revealable(&st, &app, &backups), "an app the engine ranked");
    assert!(revealable(&st, &patch, &backups));
    assert!(revealable(&st, &backups, &backups));
    assert!(!revealable(&st, &f.root, &backups));
    assert!(!revealable(&Value::Null, Path::new("/etc"), &backups));
}

#[test]
fn tray_title_marks_bad_health() {
    let t = TrayInfo {
        health: "critical".into(),
        disk_free: 12_000_000_000,
        disk_total: 1,
        cpu_pct: 40.0,
        ..TrayInfo::default()
    };
    assert_eq!(tray_title(&t), "‼︎ 12G · 40%");
    assert_eq!(tray_title(&TrayInfo::default()), "");
}

#[test]
fn failures_drop_the_connection_only_when_it_is_really_gone() {
    use mulchie_core::api::ApiError;
    let e = |status, code| ApiError::new(status, code, "x");
    assert_eq!(backend::failure_kind(&e(0, "unreachable")), Some(true));
    assert_eq!(backend::failure_kind(&e(401, "unauthorized")), Some(true));
    assert_eq!(backend::failure_kind(&e(0, "timeout")), Some(false));
    assert_eq!(backend::failure_kind(&e(0, "io_error")), Some(false));
    assert_eq!(backend::failure_kind(&e(500, "internal")), None);
    assert_eq!(backend::failure_kind(&e(400, "bad_request")), None);
}

/// A throwaway child to signal, listed in the engine's (fake) process table
/// with a start time of 1000.
fn sleeper(f: &Fixture) -> (std::process::Child, Options) {
    let (mut kids, opts) = sleepers(f, 1);
    (kids.pop().unwrap(), opts)
}

/// `n` throwaway children, each listed with a start time of 1000.
fn sleepers(f: &Fixture, n: usize) -> (Vec<std::process::Child>, Options) {
    let kids: Vec<_> = (0..n)
        .map(|_| {
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap()
        })
        .collect();
    let procs = kids.iter().fold(FakeProcesses::empty(), |p, c| {
        p.with(c.id(), "sleep", Some(f.root.clone()), 1000)
    });
    let opts = Options {
        procs: Some(Arc::new(procs)),
        ..f.opts()
    };
    (kids, opts)
}

fn client_of(f: &Fixture, opts: Options) -> Backend {
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            ..opts
        },
    );
    assert_eq!(b.mode(), Mode::Client);
    b
}

fn gone_within(child: &mut std::process::Child, secs: u64) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn signals_go_through_a_process_plan_and_only_its_items() {
    let f = fixture();
    let (mut child, opts) = sleeper(&f);
    let pid = child.id();
    let a = Backend::start(f.paths.clone(), opts.clone());
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            ..opts
        },
    );
    assert_eq!(b.mode(), Mode::Client);

    let req = |pid: u32, signal: &str| -> ProcessPlanRequest {
        serde_json::from_value(json!({ "pid": pid, "signal": signal })).unwrap()
    };
    // Nothing is accepted before a plan, nor a request the plan did not offer.
    let term = Request::Signal {
        pid,
        signal: mulchie_core::safety::Signal::Term,
        expected_start_time: 1000,
    };
    assert!(b.execute(Batch::Process, vec![term.clone()]).is_err());

    // Plan through the client: one ready item, carrying the start time seen.
    let plan = b.process_plan(req(pid, "term")).unwrap();
    assert_eq!(plan["ready"], true, "{plan}");
    assert_eq!(plan["affected"][0]["start_time"], 1000);
    let items = backend::ready_requests(&plan);
    assert_eq!(items, vec![term.clone()]);
    // Same pid, different start time: not the confirmed process.
    let reused = Request::Signal {
        pid,
        signal: mulchie_core::safety::Signal::Term,
        expected_start_time: 999,
    };
    assert!(b
        .execute(Batch::Process, vec![reused])
        .unwrap_err()
        .contains("not a ready item"));
    // A process request is not a cache batch.
    assert!(b.execute(Batch::Clean, items.clone()).is_err());
    let res = b.execute(Batch::Process, items.clone()).unwrap();
    assert!(res[0].is_done(), "{res:?}");
    assert!(gone_within(&mut child, 5), "the sleeper got SIGTERM");
    assert!(b.execute(Batch::Process, items).is_err(), "used once");

    // pid 1 is never planned as ready.
    let init = a.process_plan(req(1, "term")).unwrap();
    assert_eq!(init["ready"], false, "{init}");
    assert!(backend::ready_requests(&init).is_empty());
    drop(b);
    a.shutdown();
}

#[test]
fn process_detail_and_renice_check_the_process() {
    let f = fixture();
    let (mut child, opts) = sleeper(&f);
    let pid = child.id();
    let a = Backend::start(f.paths.clone(), opts);
    let d = a.process_detail(pid).unwrap().unwrap();
    assert_eq!(d["pid"], pid);
    assert_eq!(d["start_time"], 1000);
    assert!(d["parents"].is_array() && d["findings"].is_array(), "{d}");
    assert!(a.process_detail(u32::MAX - 1).unwrap().is_none());
    // A renice for a start time the process does not have is skipped.
    let r = a.renice(pid, 999, 10).unwrap();
    assert!(!r.is_done(), "{r:?}");
    // The confirmed process is lowered, here and through a client.
    let r = a.renice(pid, 1000, 10).unwrap();
    assert!(r.is_done(), "{r:?}");
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            procs: Some(Arc::new(FakeProcesses::empty())),
            ..f.opts()
        },
    );
    assert_eq!(b.mode(), Mode::Client);
    let r = b.renice(pid, 1000, 12).unwrap();
    assert!(r.is_done(), "{r:?}");
    assert!(!b.renice(pid, 999, 12).unwrap().is_done());
    // Process detail through the client: the process, and none for a pid
    // that does not exist.
    let d = b.process_detail(pid).unwrap().unwrap();
    assert_eq!(d["pid"], pid);
    assert_eq!(d["start_time"], 1000);
    assert!(b.process_detail(u32::MAX - 1).unwrap().is_none());
    drop(b);
    let _ = child.kill();
    let _ = child.wait();
    a.shutdown();
}

#[test]
fn process_detail_tells_an_exited_process_from_an_engine_without_the_route() {
    use mulchie_core::api::ApiError;
    let gone = backend::detail_reply(Err(ApiError::new(404, "unknown_pid", "no process 7")));
    assert_eq!(gone, Ok(None));
    let old = backend::detail_reply(Err(ApiError::new(404, "not_found", "no route")));
    assert!(old.unwrap_err().contains("not supported"));
    let other = backend::detail_reply(Err(ApiError::new(500, "internal", "boom")));
    assert_eq!(other, Err("boom".to_string()));
    assert_eq!(
        backend::detail_reply(Ok(json!({"pid": 7}))),
        Ok(Some(json!({"pid": 7})))
    );
}

#[test]
fn reveal_never_opens_an_app_or_a_file() {
    let f = fixture();
    let backups = f.paths.backups_dir.clone();
    std::fs::create_dir_all(&backups).unwrap();
    let patch = backups.join("w.patch");
    std::fs::write(&patch, "x").unwrap();
    let ws = f.root.join("ws");
    let wt = f.root.join("wt");
    let app = f.root.join("Flagged.app");
    let cache = f.root.join("ws/target");
    for d in [&ws, &wt, &app, &cache] {
        std::fs::create_dir_all(d).unwrap();
    }
    let st = json!({
        "snapshot": {"workspaces": [{"path": ws}], "consumers": {"by_cpu": [{"bundle_path": app, "workspace": null}], "by_mem": []}},
        "scan": {"items": [{"path": cache}]},
        "diskmap": {"worktrees": [{"path": wt}], "categories": []}
    });
    let args = |p: &Path, term| reveal_args(&st, p, &backups, term);
    let os = |v: &[&dyn AsRef<std::ffi::OsStr>]| -> Vec<OsString> {
        v.iter().map(|x| x.as_ref().to_os_string()).collect()
    };
    // Workspace and worktree folders and the backups folder open in place.
    assert_eq!(args(&ws, false).unwrap(), os(&[&ws]));
    assert_eq!(args(&wt, false).unwrap(), os(&[&wt]));
    assert_eq!(args(&backups, false).unwrap(), os(&[&backups]));
    assert_eq!(args(&ws, true).unwrap(), os(&[&"-a", &"Terminal", &ws]));
    // An app bundle, a file and a cache folder are revealed, never opened.
    for p in [&app, &patch, &cache] {
        let a = args(p, false).unwrap();
        assert_ne!(a, os(&[p]), "{p:?} would be opened");
        if cfg!(target_os = "macos") {
            assert_eq!(a, os(&[&"-R", p]));
        }
        assert!(args(p, true).is_err(), "{p:?} in Terminal");
    }
    // Even an app listed as a workspace is revealed.
    let st2 = json!({"snapshot": {"workspaces": [{"path": app}]}});
    assert!(!opens_in_place(&st2, &app, &backups));
}

#[test]
fn plan_book_forgets_only_the_oldest_process_plan() {
    use backend::PROCESS_PLANS_KEPT;
    let sig = |pid| Request::Signal {
        pid,
        signal: mulchie_core::safety::Signal::Term,
        expected_start_time: 7,
    };
    let mut book = backend::PlanBook::default();
    let n = PROCESS_PLANS_KEPT as u32 + 1;
    for pid in 1..=n {
        book.record_process(&format!("pid {pid}"), vec![sig(pid)]);
    }
    assert!(!book.has_process("pid 1"), "the oldest is forgotten");
    assert!(book.take(Batch::Process, &[sig(1)]).is_err());
    for pid in 2..=n {
        assert!(book.has_process(&format!("pid {pid}")), "pid {pid} kept");
    }
    // Re-recording a kept target refreshes it, so the next one out is pid 3.
    book.record_process("pid 2", vec![sig(2)]);
    book.record_process("pid 99", vec![sig(99)]);
    assert!(book.has_process("pid 2") && !book.has_process("pid 3"));
    book.take(Batch::Process, &(4..=n).map(sig).collect::<Vec<_>>())
        .unwrap();
    book.clear();
    assert!(book.take(Batch::Process, &[sig(2)]).is_err(), "cleared");
}

#[test]
fn a_bulk_selection_beyond_the_kept_plans_is_one_reviewed_plan() {
    let f = fixture();
    let n = backend::PROCESS_PLANS_KEPT + 8;
    let (mut kids, opts) = sleepers(&f, n);
    let a = Backend::start(f.paths.clone(), opts.clone());
    let b = client_of(&f, opts);
    let reqs: Vec<ProcessPlanRequest> = kids
        .iter()
        .map(|c| serde_json::from_value(json!({"pid": c.id(), "signal": "term"})).unwrap())
        .collect();
    let bulk = b.process_plans(reqs).unwrap();
    let plans = bulk["plans"].as_array().unwrap();
    assert_eq!(plans.len(), n);
    assert!(b.keeps_process_plan(bulk["target"].as_str().unwrap()));
    let items: Vec<Request> = plans.iter().flat_map(backend::ready_requests).collect();
    assert_eq!(items.len(), n, "{bulk}");
    let res = b.execute(Batch::Process, items.clone()).unwrap();
    assert_eq!(res.iter().filter(|r| r.is_done()).count(), n, "{res:?}");
    for k in kids.iter_mut() {
        assert!(gone_within(k, 5));
    }
    assert!(b.execute(Batch::Process, items).is_err(), "used once");
    assert!(b.process_plans(vec![]).is_err());
    drop(b);
    a.shutdown();
}

#[test]
fn plans_are_forgotten_on_reconnect_and_on_takeover() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = client_of(&f, f.opts());
    // Reconnect: a plan made through the old engine is not executed by the new one.
    let x = f.cache("x");
    let items = b.plan(&clean(vec![x.clone()])).unwrap().approved();
    a.shutdown();
    b.tick();
    assert_eq!(b.mode(), Mode::Disconnected);
    let a2 = Backend::start(f.paths.clone(), f.opts());
    b.tick();
    assert_eq!(b.mode(), Mode::Client);
    assert!(b
        .execute(Batch::Clean, items)
        .unwrap_err()
        .contains("not a ready item"));
    assert!(x.exists());
    drop(b);
    // Takeover: the client becomes the engine and forgets the plan too.
    let c = Backend::start(f.paths.clone(), f.opts());
    assert_eq!(c.mode(), Mode::Client);
    let items = c.plan(&clean(vec![x.clone()])).unwrap().approved();
    assert_eq!(items.len(), 1);
    a2.shutdown();
    c.tick();
    c.tick();
    assert_eq!(c.mode(), Mode::Engine);
    assert!(c
        .execute(Batch::Clean, items)
        .unwrap_err()
        .contains("not a ready item"));
    assert!(x.exists(), "nothing ran without a new plan");
    c.shutdown();
}

#[test]
fn a_failed_execute_says_whether_the_action_may_have_run() {
    use mulchie_core::api::ApiError;
    let unreachable = backend::execute_error(&ApiError::new(0, "unreachable", "refused"));
    assert!(unreachable.contains("nothing was sent"), "{unreachable}");
    let timeout = backend::execute_error(&ApiError::new(0, "timeout", "timed out"));
    assert!(
        timeout.contains("may have run") && timeout.contains("Activity"),
        "{timeout}"
    );
    // Through a client whose engine went away after the plan was shown.
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = client_of(&f, f.opts());
    let items = b.plan(&clean(vec![f.cache("x")])).unwrap().approved();
    a.shutdown();
    let e = b.execute(Batch::Clean, items).unwrap_err();
    assert!(
        e.contains("nothing was sent") || e.contains("may have run"),
        "{e}"
    );
}

#[test]
fn client_settings_send_unknown_keys_unchanged_and_the_engine_refuses_them() {
    let cfg = Config {
        low_disk_gb: 9,
        ..Config::default()
    };
    let mut v = serde_json::to_value(&cfg).unwrap();
    v["from_a_newer_engine"] = json!({"keep": [1, 2]});
    v["watch"]["newer_detector"] = json!(true);
    let body = backend::config_put_body(v.clone());
    assert_eq!(body["config"], v, "sent unchanged");
    // An engine refuses keys it does not know (it would drop them), from a
    // client and as the engine, naming the key; nothing is saved.
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = client_of(&f, f.opts());
    for (who, x) in [("client", &b), ("engine", &a)] {
        let err = x.set_config(v.clone()).unwrap_err();
        assert!(err.contains("from_a_newer_engine"), "{who}: {err}");
    }
    assert_ne!(a.state().unwrap()["config"]["low_disk_gb"], 9);
    // The keys it knows save through either side.
    let mut known = serde_json::to_value(&cfg).unwrap();
    b.set_config(known.clone()).unwrap();
    assert_eq!(a.state().unwrap()["config"]["low_disk_gb"], 9);
    known["low_disk_gb"] = json!(10);
    a.set_config(known).unwrap();
    assert_eq!(a.state().unwrap()["config"]["low_disk_gb"], 10);
    assert!(a.set_config(json!({"low_disk_gb": "lots"})).is_err());
    // The client reports the running engine's settings file.
    assert_eq!(b.config_path(), f.paths.config_file());
    drop(b);
    a.shutdown();
}

#[test]
fn findings_dismiss_and_undismiss_through_the_config() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            ..f.opts()
        },
    );
    let id = "stale_process:42:1000";
    let until = b.dismiss_finding(id, Some(2)).unwrap();
    assert!(until > now() + 86_000 && until <= now() + 2 * 86_400 + 5);
    let cfg = a.state().unwrap()["config"]["watch"]["dismissed"].clone();
    assert_eq!(cfg[0]["id"], id, "{cfg}");
    assert!(b.dismiss_finding(id, Some(0)).is_err(), "days 1 to 3650");
    assert!(b.undismiss_finding(id).unwrap());
    assert!(!a.undismiss_finding(id).unwrap());
    drop(b);
    a.shutdown();
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn a_rejected_token_disconnects_and_the_next_tick_reconnects() {
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = Backend::start(
        f.paths.clone(),
        Options {
            takeover_after: Duration::from_secs(3600),
            ..f.opts()
        },
    );
    assert_eq!(b.mode(), Mode::Client);
    // Rotate the token: the client's calls now get 401.
    let tok = mulchie_core::api::token_path_in(&f.paths);
    std::fs::remove_file(&tok).unwrap();
    a.shutdown();
    let a = Backend::start(f.paths.clone(), f.opts());
    assert_eq!(a.mode(), Mode::Engine);
    let old = b.info().port;
    let err = b.state();
    if old == a.info().port {
        // Same port, new token: refused, then reconnected with the new one.
        assert!(err.is_err());
        assert_eq!(b.mode(), Mode::Disconnected);
    }
    b.tick();
    b.tick();
    assert_eq!(b.mode(), Mode::Client, "{:?}", b.info());
    assert!(b.state().is_ok());
    drop(b);
    a.shutdown();
}

#[test]
fn cli_link_state_tells_our_link_from_other_files() {
    let f = fixture();
    let bin = f.root.join("mulchie-bundled");
    std::fs::write(&bin, "x").unwrap();
    let link = f.root.join("link");
    assert_eq!(link_state(&link, Some(&bin)), LinkState::Missing);
    std::os::unix::fs::symlink(&bin, &link).unwrap();
    assert_eq!(link_state(&link, Some(&bin)), LinkState::Ours);
    assert!(matches!(link_state(&link, None), LinkState::OtherLink(_)));
    std::fs::remove_file(&link).unwrap();
    std::fs::write(&link, "a regular file").unwrap();
    assert_eq!(link_state(&link, Some(&bin)), LinkState::OtherFile);
}

#[test]
fn capabilities_grant_only_event_listening() {
    let caps: Value = serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
    let perms: Vec<&str> = caps["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(
        perms,
        ["core:event:allow-listen", "core:event:allow-unlisten"]
    );
}

#[test]
fn panel_height_stays_within_limits() {
    assert_eq!(panel_height(500.0), 500.0);
    assert_eq!(panel_height(10.0), 320.0);
    assert_eq!(panel_height(5000.0), 760.0);
    assert_eq!(panel_height(f64::NAN), 620.0);
}

#[test]
fn sessions_limits_and_updates_through_the_engine_and_a_client() {
    use mulchie_core::config::{UpdateCheck, UpdatesConfig};
    let f = fixture();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let procs = FakeProcesses::empty().with(pid, "claude", Some(f.root.clone()), 1000);
    let opts = Options {
        procs: Some(Arc::new(procs)),
        config: Some(Config {
            scan_roots: vec![f.root.clone()],
            throttle_builds: false,
            // No update worker may reach the network from a test.
            updates: UpdatesConfig {
                check: UpdateCheck::Off,
                ..UpdatesConfig::default()
            },
            ..Config::default()
        }),
        ..f.opts()
    };
    let a = Backend::start(f.paths.clone(), opts.clone());
    let b = client_of(&f, opts);
    // An agent hook registers the session, as `mulchie hook` would.
    let api = mulchie_core::api::Client::connect_in(&f.paths, a.info().port.unwrap()).unwrap();
    let hook = api
        .call(
            "POST",
            "/v1/hook",
            Some(&json!({"client": "claude", "event": "session_start", "pid": pid, "cwd": f.root})),
        )
        .unwrap();
    let id = hook["session_id"].as_str().expect("a session").to_string();
    assert_eq!(id, format!("s-1000-{pid}"));

    for (who, x) in [("engine", &a), ("client", &b)] {
        let list = x.sessions().unwrap();
        assert_eq!(list["enabled"], true, "{who} {list}");
        assert!(list["write_bytes_available"].is_boolean(), "{who}");
        assert!(
            list["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == id && s["source"] == "hook"),
            "{who} {list}"
        );
        let d = x.session_detail(&id).unwrap().expect("known");
        assert_eq!(d["session"]["id"], id, "{who}");
        assert!(
            d["breaches"].is_array() && d["paused_by_mulchie"].is_array(),
            "{who} {d}"
        );
        assert_eq!(x.session_detail("s-1-1").unwrap(), None, "{who}");
        assert!(
            x.session_detail("../config").is_err(),
            "{who}: checked before the URL"
        );
        let ev = x.session_events(&id, 0, 50).unwrap();
        assert!(
            ev["events"].is_array() && ev["next_since"].is_u64(),
            "{who} {ev}"
        );
        assert!(x.session_events("s-1-1", 0, 50).is_err(), "{who}");
        let lim = x.limits_status().unwrap();
        assert_eq!(lim["report_only"], true, "{who} {lim}");
        assert!(
            lim["enforcing"].is_boolean() && lim["breaches"].is_array(),
            "{who}"
        );
        let up = x.updates_status().unwrap();
        assert_eq!(up["enabled"], false, "{who} {up}");
        let off = x.check_updates().unwrap_err();
        assert!(off.contains("off"), "{who}: {off}");
        // A session plan goes through the same plan book as the others.
        let req: ProcessPlanRequest =
            serde_json::from_value(json!({"session": id, "action": "pause"})).unwrap();
        let plan = x.process_plans(vec![req]).unwrap();
        assert_eq!(plan["plans"].as_array().unwrap().len(), 1, "{who} {plan}");
    }

    // Limits through PATCH: a budget makes the engine enforce; a bad value
    // is refused with the field in the message, from either side.
    let budget = json!({"limits": {"default": {"cpu_cores": {"max": 2.0, "sustain_secs": 30, "action": "pause"}}}});
    let cfg = b.patch_config(budget).unwrap();
    assert_eq!(cfg["limits"]["default"]["cpu_cores"]["action"], "pause");
    assert_eq!(a.limits_status().unwrap()["report_only"], false);
    let bad = json!({"limits": {"default": {"cpu_cores": {"max": -1.0}}}});
    for x in [&a, &b] {
        let e = x.patch_config(bad.clone()).unwrap_err();
        assert_eq!(e.code, "invalid_config", "{e:?}");
        assert!(e.message.contains("limits.default.cpu_cores.max"), "{e:?}");
    }
    // `null` removes a key, so the budget goes and the engine only reports.
    a.patch_config(json!({"limits": {"default": null}}))
        .unwrap();
    assert_eq!(b.limits_status().unwrap()["report_only"], true);
    assert!(a.patch_config(json!([1])).is_err(), "a patch is an object");

    drop(b);
    let _ = child.kill();
    let _ = child.wait();
    a.shutdown();
}

/// A config for tests whose engine samples the real process table: nothing
/// is reclaimed, swept or throttled, no alert or update check runs.
fn quiet_config(f: &Fixture) -> Config {
    use mulchie_core::config::{UpdateCheck, UpdatesConfig};
    Config {
        scan_roots: vec![f.root.clone()],
        throttle_builds: false,
        auto_reclaim: false,
        auto_sweep_worktrees: false,
        low_disk_gb: 0,
        alert_below_gb: 0,
        updates: UpdatesConfig {
            check: UpdateCheck::Off,
            ..UpdatesConfig::default()
        },
        ..Config::default()
    }
}

#[test]
fn config_refusals_carry_the_api_codes_on_both_sides() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let a = Backend::start(f.paths.clone(), f.opts());
    let b = client_of(&f, f.opts());
    let e = a.local_engine().expect("a runs the engine");
    let patch = json!({"guard": {"rules": {"sudo": "block"}}});

    // An engine that does not hold the lock never writes the settings.
    e.set_automation_enabled(false);
    for (who, x) in [("engine", &a), ("client", &b)] {
        let err = x.patch_config(patch.clone()).unwrap_err();
        assert_eq!(err.code, "not_lock_holder", "{who}: {err:?}");
    }
    e.set_automation_enabled(true);

    // A settings folder that cannot be written to.
    let dir = f.paths.config_dir.clone();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let results: Vec<_> = [&a, &b]
        .iter()
        .map(|x| x.patch_config(patch.clone()))
        .collect();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (who, r) in ["engine", "client"].iter().zip(results) {
        let err = r.unwrap_err();
        assert_eq!(err.code, "config_io", "{who}: {err:?}");
    }

    // A key the engine does not know.
    for (who, x) in [("engine", &a), ("client", &b)] {
        let err = x
            .patch_config(json!({"limits": {"no_such_key": 1}}))
            .unwrap_err();
        assert_eq!(err.code, "unknown_key", "{who}: {err:?}");
    }

    // And a bad value is still invalid_config, so the codes are not constant.
    let bad = json!({"updates": {"interval_hours": 1}});
    for (who, x) in [("engine", &a), ("client", &b)] {
        let err = x.patch_config(bad.clone()).unwrap_err();
        assert_eq!(err.code, "invalid_config", "{who}: {err:?}");
        assert!(
            err.message.contains("updates.interval_hours"),
            "{who}: {err:?}"
        );
    }
    let ok = b.patch_config(patch).unwrap();
    assert_eq!(ok["guard"]["rules"]["sudo"], "block");
    drop(b);
    a.shutdown();
}

/// Kills its throwaway processes when the test ends, passed or not.
struct Reap(Vec<u32>);
impl Drop for Reap {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

fn stat_of(pid: u32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn wait_stat(pid: u32, stopped: bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < end {
        if stat_of(pid).starts_with('T') == stopped {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn session_plans_record_paging_end_and_unreadable_record() {
    use mulchie_core::engine::SampleCtx;
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    // The real process table, so the session's members are real processes:
    // a throwaway perl (the "agent") with a sleep it started.
    let opts = Options {
        procs: None,
        config: Some(quiet_config(&f)),
        ..f.opts()
    };
    let a = Backend::start(f.paths.clone(), opts.clone());
    let b = client_of(&f, opts);
    let e = a.local_engine().unwrap();
    // Detached (its parent exits, so it is reparented), so the hook cannot
    // bind it to the session of an agent that runs this test.
    let out = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            // A non-shell root (the hook registers the nearest non-shell)
            // with one child.
            "/usr/bin/perl -e 'fork or exec qw(sleep 60); wait' </dev/null >/dev/null 2>&1 & echo $!",
        ])
        .current_dir(&f.root)
        .output()
        .unwrap();
    let rpid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let ppid_of = |pid: u32| {
        let o = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let end = std::time::Instant::now() + Duration::from_secs(5);
    while ppid_of(rpid) != "1" && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _reap_root = Reap(vec![rpid]);
    assert_eq!(ppid_of(rpid), "1", "the throwaway agent is detached");
    let kill_all = |pids: &[u32]| {
        for p in pids {
            let _ = std::process::Command::new("kill")
                .args(["-9", &p.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    };
    let api = mulchie_core::api::Client::connect_in(&f.paths, a.info().port.unwrap()).unwrap();
    let hook = api
        .call(
            "POST",
            "/v1/hook",
            Some(
                &json!({"client": "claude", "event": "session_start", "pid": rpid, "cwd": f.root}),
            ),
        )
        .unwrap();
    let id = hook["session_id"].as_str().expect("a session").to_string();
    if !id.ends_with(&format!("-{rpid}")) {
        kill_all(&[rpid]);
        panic!("the hook joined another session ({id}); nothing is signalled");
    }
    let mut ctx = SampleCtx::default();
    let mut kid = None;
    for _ in 0..5 {
        e.sample_tick(&mut ctx);
        let d = a.session_detail(&id).unwrap().unwrap();
        kid = d["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pid"] != rpid && p["name"] == "sleep")
            .and_then(|p| p["pid"].as_u64())
            .map(|p| p as u32);
        if kid.is_some() {
            break;
        }
    }
    let Some(kid) = kid else {
        kill_all(&[rpid]);
        panic!("the sleep is not a member of the session");
    };
    let _reap = Reap(vec![kid, rpid]);
    // Never execute a plan that names anything but these two processes.
    let only_ours = |plan: &Value| {
        let mine = |p: u64| p == rpid as u64 || p == kid as u64;
        let ok = plan["affected"]
            .as_array()
            .is_some_and(|a| a.iter().all(|p| p["pid"].as_u64().is_some_and(mine)))
            && backend::ready_requests(plan).iter().all(|r| match r {
                Request::Signal { pid, .. } => mine(*pid as u64),
                _ => false,
            });
        if !ok {
            kill_all(&[kid, rpid]);
            panic!("the plan names other processes; nothing was sent: {plan}");
        }
    };

    // Pause without the agent: only the sleep stops; then with the agent.
    for (who, x) in [("engine", &a), ("client", &b)] {
        for include in [false, true] {
            let req: ProcessPlanRequest = serde_json::from_value(
                json!({"session": id, "action": "pause", "include_agent": include}),
            )
            .unwrap();
            let plan = x.process_plan(req).unwrap();
            only_ours(&plan);
            let pids: Vec<u64> = plan["affected"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|p| p["pid"].as_u64())
                .collect();
            assert!(pids.contains(&(kid as u64)), "{who} {plan}");
            assert_eq!(pids.contains(&(rpid as u64)), include, "{who} {plan}");
            let res = x
                .execute(Batch::Process, backend::ready_requests(&plan))
                .unwrap();
            assert!(res.iter().all(|r| r.is_done()), "{who} {res:?}");
            assert!(wait_stat(kid, true), "{who}: the sleep is paused");
            assert_eq!(
                stat_of(rpid).starts_with('T'),
                include,
                "{who} include_agent={include}: agent {}",
                stat_of(rpid)
            );
            let req: ProcessPlanRequest = serde_json::from_value(
                json!({"session": id, "action": "resume", "include_agent": true}),
            )
            .unwrap();
            let plan = x.process_plan(req).unwrap();
            only_ours(&plan);
            x.execute(Batch::Process, backend::ready_requests(&plan))
                .unwrap();
            assert!(wait_stat(kid, false) && wait_stat(rpid, false), "{who}");
        }
    }

    // Paging: one event per page follows `next_since` through the whole
    // record and ends where a single read ends.
    for (who, x) in [("engine", &a), ("client", &b)] {
        let all = x.session_events(&id, 0, 1000).unwrap();
        let all = all["events"].as_array().unwrap().clone();
        assert!(all.len() >= 2, "{who} {all:?}");
        let (mut since, mut paged) = (0, Vec::new());
        loop {
            let r = x.session_events(&id, since, 1).unwrap();
            let ev = r["events"].as_array().unwrap();
            if ev.is_empty() {
                assert_eq!(
                    r["next_since"], since,
                    "{who}: an empty page keeps the cursor"
                );
                break;
            }
            assert_eq!(
                r["next_since"], ev[0]["seq"],
                "{who}: the cursor is the last seq"
            );
            since = r["next_since"].as_u64().unwrap();
            paged.push(ev[0].clone());
            assert!(paged.len() <= all.len(), "{who}: paging never repeats");
        }
        assert_eq!(paged, all, "{who}");
    }

    // The session ends with its agent; its record stays readable.
    kill_all(&[kid, rpid]);
    for _ in 0..5 {
        e.sample_tick(&mut ctx);
        if a.session_detail(&id).unwrap().unwrap()["session"]["status"] == "ended" {
            break;
        }
    }
    for (who, x) in [("engine", &a), ("client", &b)] {
        let d = x
            .session_detail(&id)
            .unwrap()
            .expect("an ended session is kept");
        assert_eq!(d["session"]["status"], "ended", "{who} {d}");
        let ev = x.session_events(&id, 0, 1000).unwrap();
        assert!(
            ev["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["type"] == "session_end"),
            "{who} {ev}"
        );
    }

    // A record that cannot be read is an error, not an empty record.
    let file = mulchie_core::record::file_of(&f.paths.config_dir.join("sessions"), &id);
    assert!(file.exists(), "{}", file.display());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let results: Vec<_> = [&a, &b]
        .iter()
        .map(|x| x.session_events(&id, 0, 10))
        .collect();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    for (who, r) in ["engine", "client"].iter().zip(results) {
        let err = r.unwrap_err();
        assert!(
            err.contains("could not be read")
                || err.contains("record_unreadable")
                || err.to_lowercase().contains("permission"),
            "{who}: {err}"
        );
    }
    drop(b);
    a.shutdown();
}
