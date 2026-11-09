//! Tests for the pure parts of the app: the local/remote backend against a
//! real API server on a free port with temporary paths, plan bookkeeping,
//! updater detection and path checks.

use super::*;
use mulchie_core::process::FakeProcesses;
use mulchie_core::safety::CACHEDIR_TAG_SIGNATURE;
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

    // Settings belong to the engine.
    let err = b.set_config(Config::default()).unwrap_err();
    assert!(err.contains("config.json"), "{err}");

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
    let st = json!({
        "snapshot": {"workspaces": [{"path": ws}]},
        "scan": {"items": []},
        "diskmap": null
    });
    assert!(revealable(&st, &ws, &backups));
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
