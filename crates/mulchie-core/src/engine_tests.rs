//! Engine tests: leases, stale health, supervision, the job channel, plan then
//! execute, throttling. Paths live under a tempdir; the only real processes
//! touched are children the test spawned.

use super::*;
use crate::git::Git;
use crate::process::FakeProcesses;
use crate::safety::{SkipReason, CACHEDIR_TAG_SIGNATURE};

pub(crate) fn engine_with(cfg: Config) -> (tempfile::TempDir, Engine) {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
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

pub(crate) fn engine_with_procs(cfg: Config, procs: FakeProcesses) -> (tempfile::TempDir, Engine) {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let e = Engine::with_processes(
        paths,
        LoadedConfig {
            config: cfg,
            error: None,
        },
        Arc::new(procs),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe));
    (t, e)
}

fn engine() -> (tempfile::TempDir, Engine) {
    engine_with(Config::default())
}

fn lease(label: &str, pid: Option<u32>, ticket: Option<u64>, ttl: Option<u64>) -> LeaseRequest {
    LeaseRequest {
        label: label.into(),
        pid,
        ticket,
        ttl_secs: ttl,
        secret: None,
    }
}

/// Renew or poll the ticket of `r` with its secret.
fn again(r: &LeaseReply, ttl: Option<u64>) -> LeaseRequest {
    LeaseRequest {
        label: String::new(),
        pid: None,
        ticket: Some(r.ticket),
        ttl_secs: ttl,
        secret: r.release_secret.clone(),
    }
}

fn one_slot(e: &Engine) {
    let mut st = e.state_mut();
    st.slots.capacity = 1;
    st.slots.jobs_per_slot = 4;
    let mut s = (*st.snapshot).clone();
    s.mem_total = 100;
    s.mem_available = 90;
    st.snapshot = Arc::new(s);
}

/// A child that exits at once; its pid is then free (and not ours).
fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

#[test]
fn slots_queue_in_order_and_release() {
    let (_t, e) = engine();
    one_slot(&e);
    let me = Some(std::process::id());
    let a = e.acquire(&lease("a", me, None, None)).unwrap();
    assert!(a.granted);
    let b = e.acquire(&lease("b", me, None, None)).unwrap();
    assert!(!b.granted);
    let c = e.acquire(&lease("c", me, None, None)).unwrap();
    assert_eq!(c.position, 1);
    assert!(e.release(a.ticket));
    let c2 = e.acquire(&again(&c, None)).unwrap();
    assert!(!c2.granted);
    let b2 = e.acquire(&again(&b, None)).unwrap();
    assert!(b2.granted);
    assert_eq!(b2.jobs, 4);
}

#[test]
fn a_lease_needs_a_live_pid_or_a_ttl() {
    let (_t, e) = engine();
    one_slot(&e);
    assert_eq!(
        e.acquire(&lease("x", None, None, None)).unwrap_err(),
        LeaseError::NeedsPidOrTtl
    );
    assert_eq!(
        e.acquire(&lease("x", Some(0), None, None)).unwrap_err(),
        LeaseError::NeedsPidOrTtl,
        "pid 0 is no pid"
    );
    let gone = dead_pid();
    assert_eq!(
        e.acquire(&lease("x", Some(gone), None, None)).unwrap_err(),
        LeaseError::PidNotAlive(gone)
    );
    assert_eq!(
        e.acquire(&lease("x", None, None, Some(0))).unwrap_err(),
        LeaseError::TtlOutOfRange(0)
    );
    assert_eq!(
        e.acquire(&lease("x", None, None, Some(MAX_LEASE_TTL_SECS + 1)))
            .unwrap_err()
            .code(),
        "ttl_out_of_range"
    );
    assert!(
        e.acquire(&lease("x", None, None, Some(60)))
            .unwrap()
            .granted
    );
}

#[test]
fn ttl_leases_expire_unless_renewed() {
    let (_t, e) = engine();
    one_slot(&e);
    let a = e
        .acquire_at(&lease("a", None, None, Some(10)), 100)
        .unwrap();
    assert!(a.granted);
    assert_eq!(a.expires_unix, Some(110));
    // Renew: the same ticket again.
    let r = e.acquire_at(&again(&a, None), 105).unwrap();
    assert!(r.granted && r.reason == "renewed");
    assert_eq!(r.expires_unix, Some(115));
    e.expire_leases_at(114);
    assert_eq!(e.snapshot_state().slots.active.len(), 1);
    e.expire_leases_at(115);
    assert!(e.snapshot_state().slots.active.is_empty(), "expired");
    // An expired ticket is unknown again: it must say how it ends.
    assert_eq!(
        e.acquire_at(&lease("a", None, Some(a.ticket), None), 116)
            .unwrap_err(),
        LeaseError::NeedsPidOrTtl
    );
    // The slot is free again for the next caller.
    let b = e
        .acquire_at(&lease("b", None, None, Some(10)), 116)
        .unwrap();
    assert!(b.granted);
}

#[test]
fn a_pid_lease_ends_when_its_process_exits() {
    let (_t, e) = engine();
    one_slot(&e);
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let a = e.acquire(&lease("a", Some(pid), None, None)).unwrap();
    assert!(a.granted);
    e.expire_leases_at(now_unix());
    assert_eq!(e.snapshot_state().slots.active.len(), 1);
    child.kill().unwrap();
    child.wait().unwrap();
    e.expire_leases_at(now_unix());
    assert!(e.snapshot_state().slots.active.is_empty());
}

#[test]
fn eperm_counts_as_alive() {
    // pid 1 belongs to root: kill(1, 0) is EPERM for a normal user (and
    // succeeds for root). Either way it is alive.
    assert!(pid_alive(1));
    assert!(!pid_alive(0));
    // u32::MAX would be kill(-1, 0): "every process". Never.
    assert!(!pid_alive(u32::MAX));
    assert!(!pid_alive(i32::MAX as u32 + 1));
}

#[test]
fn auto_reclaim_never_means_everything() {
    let cfg = Config {
        low_disk_gb: 100,
        auto_reclaim_target_gb: 60,
        ..Config::default()
    };
    // The audit's case: free space already above the target.
    assert_eq!(auto_reclaim_need(&cfg, 80_000_000_000), None);
    assert_eq!(auto_reclaim_need(&cfg, 60_000_000_000), None);
    assert_eq!(
        auto_reclaim_need(&cfg, 50_000_000_000),
        Some(10_000_000_000)
    );
    // And such a config does not validate in the first place.
    assert!(cfg.validate().is_err());
}

#[test]
fn config_error_turns_automation_off_and_is_in_state() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), b"{ broken").unwrap();
    let loaded = Config::load_from(&paths);
    let e = Engine::with_processes(paths.clone(), loaded, Arc::new(FakeProcesses::empty()));
    let st = e.snapshot_state();
    assert!(st.config_error.is_some());
    assert!(!st.automation);
    assert!(!st.config.auto_reclaim && !st.config.auto_sweep_worktrees);
    assert!(!e.automation_allowed());
    e.set_config(Config::default()).unwrap();
    assert!(e.config_error().is_none() && e.automation_allowed());
    e.set_automation_enabled(false);
    assert!(!e.automation_allowed() && !e.snapshot_state().automation);
}

#[test]
fn activity_survives_a_restart() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let mk = || {
        Engine::with_processes(
            paths.clone(),
            LoadedConfig {
                config: Config::default(),
                error: None,
            },
            Arc::new(FakeProcesses::empty()),
        )
    };
    let e = mk();
    e.log("alert", "one".into(), 0, None, "test");
    let r = e.clean_paths(&[PathBuf::from("relative/target")], "test");
    assert_eq!(r[0].reason(), Some(SkipReason::NotAbsolute));
    let e2 = mk();
    let st = e2.snapshot_state();
    assert_eq!(st.actions.len(), 2);
    assert_eq!(st.actions[0].status.as_deref(), Some("skipped"));
    assert_eq!(st.actions[1].detail, "one");
}

#[test]
fn health_is_stale_without_a_recent_sample() {
    let (_t, e) = engine();
    let st = e.snapshot_state();
    assert_eq!(st.health, Health::Stale, "no sample yet");
    assert_eq!(st.sample_age_secs, None);
    let now = now_unix();
    e.set_snapshot(Snapshot {
        at_unix: now - 20,
        ..Snapshot::default()
    });
    let st = e.snapshot_state();
    assert_eq!(st.health, Health::Stale);
    assert!(st.sample_age_secs.unwrap() >= 20);
    let h = e.health_summary();
    assert!(h.stale && h.health == Health::Stale);
    assert!(!e.advice().ok_to_start_heavy_work);

    e.set_snapshot(Snapshot {
        at_unix: now,
        ..Snapshot::default()
    });
    let h = e.health_summary();
    assert!(!h.stale, "fresh: {:?}", h.health);
    assert!(h.sample_age_secs.unwrap() <= 2);
}

#[test]
fn a_panicking_loop_restarts_and_counts() {
    let (_t, e) = engine();
    e.set_restart_backoff(Duration::from_millis(1));
    let mut ticks = 0;
    let mut contexts = 0;
    let made = std::cell::Cell::new(0);
    e.supervise(
        LoopName::Sample,
        || made.set(made.get() + 1),
        |eng, _| {
            ticks += 1;
            if ticks <= 2 {
                panic!("injected failure {ticks}");
            }
            eng.begin_shutdown();
        },
    );
    contexts += made.get();
    assert_eq!(ticks, 3);
    assert_eq!(contexts, 3, "fresh context after each restart");
    let st = e.snapshot_state();
    assert_eq!(st.loops.sample.restarts, 2);
    assert_eq!(st.loops.scan.restarts, 0);
    assert_eq!(
        st.loops.sample.last_panic.as_deref(),
        Some("injected failure 2")
    );
    assert!(st
        .actions
        .iter()
        .any(|a| a.kind == "error" && a.detail.contains("sample loop failed")));
    assert_eq!(e.health_summary().restarts, 2);
}

#[test]
fn a_poisoned_state_lock_does_not_wedge_the_engine() {
    let (_t, e) = engine();
    let e2 = e.clone();
    let _ = std::thread::spawn(move || {
        let _g = e2.state_mut();
        panic!("poison the state lock");
    })
    .join();
    assert!(e.state.is_poisoned());
    e.log("alert", "still works".into(), 0, None, "test");
    assert_eq!(e.snapshot_state().actions[0].detail, "still works");
}

#[test]
fn reclaim_is_requested_from_the_sample_loop_and_run_on_the_scan_loop() {
    let cfg = Config {
        auto_reclaim: true,
        low_disk_gb: 50,
        auto_reclaim_target_gb: 60,
        alert_below_gb: 10,
        ..Config::default()
    };
    let (_t, e) = engine_with(cfg);
    let low = Snapshot {
        at_unix: now_unix(),
        disk_total: 500_000_000_000,
        disk_free: 20_000_000_000,
        ..Snapshot::default()
    };
    let mut ctx = SampleCtx::default();
    e.apply_sample(low.clone(), &[], &mut ctx);
    assert_eq!(
        e.pending_jobs(),
        vec![Job::Reclaim {
            want_bytes: 40_000_000_000,
            source: "optimizer".into()
        }]
    );
    // Cooldown: the next sample does not queue another.
    e.apply_sample(low.clone(), &[], &mut ctx);
    assert_eq!(e.pending_jobs().len(), 1);
    // A second engine (lock held elsewhere) never asks.
    let (_t2, e2) = engine_with(e.config());
    e2.set_automation_enabled(false);
    e2.apply_sample(low, &[], &mut SampleCtx::default());
    assert!(e2.pending_jobs().is_empty());

    // The scan loop takes it (after its first scan).
    let mut sctx = ScanCtx::default();
    e.scan_tick(&mut sctx, Duration::ZERO);
    assert!(e.snapshot_state().scan.is_some(), "first tick scans");
    e.scan_tick(&mut sctx, Duration::ZERO);
    assert!(e.pending_jobs().is_empty(), "job consumed");
}

#[test]
fn scan_requests_are_deduplicated() {
    let (_t, e) = engine();
    assert!(e.request_scan());
    assert!(!e.request_scan());
    assert_eq!(e.pending_jobs(), vec![Job::Scan]);
    let mut ctx = ScanCtx {
        last_scan: Some(Instant::now()),
        last_watch: None,
    };
    e.scan_tick(&mut ctx, Duration::ZERO);
    assert!(e.pending_jobs().is_empty());
    assert!(e.snapshot_state().diskmap.is_some());
}

#[test]
fn shutdown_stops_both_loops() {
    let (_t, e) = engine();
    e.start();
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    e.shutdown(Duration::from_secs(20));
    assert!(e.is_shut_down());
    assert!(lock(&e.threads).is_empty());
    assert!(t.elapsed() < Duration::from_secs(20));
}

fn cache_at(root: &Path, rel: &str) -> PathBuf {
    let dir = root.join(rel);
    std::fs::create_dir_all(dir.join("target/debug")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
    std::fs::write(dir.join("target/CACHEDIR.TAG"), CACHEDIR_TAG_SIGNATURE).unwrap();
    std::fs::write(dir.join("target/debug/blob"), vec![1u8; 10_000]).unwrap();
    dir.join("target")
}

fn code_root(t: &tempfile::TempDir) -> PathBuf {
    let root = t.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

#[test]
fn only_approved_caches_are_deleted() {
    let t = tempfile::tempdir().unwrap();
    let root = code_root(&t);
    let paths = Paths::under(t.path());
    let e = Engine::with_processes(
        paths,
        LoadedConfig {
            config: Config {
                scan_roots: vec![root.clone()],
                ..Config::default()
            },
            error: None,
        },
        Arc::new(FakeProcesses::empty()),
    );
    let a = cache_at(&root, "a");
    let b = cache_at(&root, "b");
    let plan = e.clean_plan(&[a.clone(), b.clone()]);
    assert_eq!(
        plan.items.iter().filter(|i| i.ready).count(),
        2,
        "both deletable"
    );
    let approved: Vec<Request> = plan
        .approved()
        .into_iter()
        .filter(|r| r.target() == a.display().to_string())
        .collect();
    let r = e.execute(&approved, "test");
    assert_eq!(r.len(), 1);
    assert!(r[0].is_done());
    assert!(!a.exists());
    assert!(b.exists(), "not in the approved list, so untouched");
}

#[test]
fn only_approved_worktrees_are_removed() {
    let t = tempfile::tempdir().unwrap();
    let root = code_root(&t);
    let paths = Paths::under(t.path());
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        Git::new(&paths, dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .text()
            .unwrap()
    };
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wa = repo.join(".claude/worktrees/agent-a");
    let wb = repo.join(".claude/worktrees/agent-b");
    for (p, b) in [(&wa, "a"), (&wb, "b")] {
        git(
            &repo,
            &["worktree", "add", "-q", "-b", b, p.to_str().unwrap()],
        );
    }
    let e = Engine::with_processes(
        paths.clone(),
        LoadedConfig {
            config: Config {
                scan_roots: vec![root.clone()],
                ..Config::default()
            },
            error: None,
        },
        Arc::new(FakeProcesses::empty()),
    );
    let plan = e.worktree_plan(&[wa.clone(), wb.clone()], false);
    assert!(plan.items.iter().all(|i| i.ready), "{plan:?}");
    assert!(plan
        .items
        .iter()
        .all(|i| i.head.as_deref().map(str::len) == Some(40)));
    let only_a: Vec<Request> = plan
        .approved()
        .into_iter()
        .filter(|r| r.target() == wa.display().to_string())
        .collect();
    assert_eq!(only_a.len(), 1);
    let r = e.execute(&only_a, "test");
    assert!(r[0].is_done(), "{r:?}");
    assert!(!wa.exists());
    assert!(wb.exists(), "B was planned and ready but not approved");

    // A request without the HEAD the caller saw is refused at execution.
    let r = e.execute(
        &[Request::RemoveWorktree {
            path: wb.clone(),
            expected_head: String::new(),
            allow_dirty: false,
            require_idle: false,
        }],
        "test",
    );
    assert_eq!(r[0].reason(), Some(SkipReason::MissingExpectedHead));
    assert!(wb.exists());
}

#[test]
fn throttling_tracks_and_restores_only_own_builds() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    // The test may itself run niced (a background job); aim above that.
    let before = nice_of(pid).unwrap();
    if before >= 19 {
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    let target = (before + 3).min(20);
    let cfg = Config {
        throttle_builds: true,
        throttle_cpu_pct: 50.0,
        build_nice: target,
        ..Config::default()
    };
    let (_t, e) = engine_with_procs(cfg, FakeProcesses::empty().with(pid, "sleep", None, 42));
    let kinds = [
        KindInfo {
            pid,
            kind: ProcKind::Build,
            own: true,
            start_time: 42,
        },
        // Someone else's build: never touched (and the pid is our own child
        // too, so a wrong renice would be visible).
        KindInfo {
            pid: std::process::id(),
            kind: ProcKind::Build,
            own: false,
            start_time: 1,
        },
    ];
    let hot = Snapshot {
        at_unix: now_unix(),
        cpu_pct: 95.0,
        ..Snapshot::default()
    };
    let mut ctx = SampleCtx::default();
    e.apply_sample(hot.clone(), &kinds, &mut ctx);
    assert_eq!(nice_of(pid), Some(target));
    assert_eq!(nice_of(std::process::id()), Some(before));
    assert_eq!(e.snapshot_state().throttled, 1);
    assert_eq!(lock(&e.reniced).get(&pid).map(|r| r.original), Some(before));
    // Pressure clears: restore is attempted. Raising priority back needs
    // privilege on most systems; then the process stays counted until it exits.
    let calm = Snapshot {
        cpu_pct: 5.0,
        ..hot
    };
    e.apply_sample(calm.clone(), &kinds, &mut ctx);
    let restored = nice_of(pid) == Some(before);
    let tracked = lock(&e.reniced).get(&pid).cloned();
    assert!(
        restored && tracked.is_none()
            || !restored && tracked.map(|r| r.restore_failed) == Some(true)
    );
    assert!(e
        .snapshot_state()
        .actions
        .iter()
        .any(|a| a.kind == "throttle" && a.detail.contains("estore")));
    child.kill().unwrap();
    child.wait().unwrap();
    // Gone from the table: forgotten.
    e.apply_sample(calm, &kinds[1..], &mut ctx);
    assert_eq!(e.snapshot_state().throttled, 0);
}

/// C6: journal failures reach state and health; recording keeps going.
#[test]
fn journal_failures_show_in_state_and_health() {
    let (_t, e) = engine();
    let jf = e.paths().journal_file();
    std::fs::create_dir_all(&jf).unwrap(); // a directory where the file goes
    e.log("note", "hello".into(), 0, None, "t");
    let st = e.snapshot_state();
    assert!(
        st.actions.iter().any(|a| a.detail == "hello"),
        "kept in memory"
    );
    assert!(st
        .journal_error
        .as_deref()
        .unwrap_or("")
        .contains("journal"));
    assert!(e.health_summary().journal_error.is_some());
    std::fs::remove_dir(&jf).unwrap();
    e.log("note", "again".into(), 0, None, "t");
    assert!(e.snapshot_state().journal_error.is_none());
    assert!(e.health_summary().journal_error.is_none());
    // R7: the loss is counted since start and noted in the file.
    assert_eq!(e.snapshot_state().journal_lost, 1);
    assert_eq!(e.health_summary().journal_lost, 1);
    let text = std::fs::read_to_string(&jf).unwrap();
    assert!(text.contains("journal gap: 1 entries lost"), "{text}");
}

/// C8: a head waiter with a long TTL that stops polling no longer blocks the
/// queue for its TTL; it leaves after the poll window.
#[test]
fn a_silent_head_waiter_leaves_after_the_poll_window() {
    let (_t, e) = engine();
    one_slot(&e);
    let t0 = 1_000_000;
    let a = e
        .acquire_at(&lease("a", None, None, Some(600)), t0)
        .unwrap();
    assert!(a.granted);
    let b = e
        .acquire_at(&lease("b", None, None, Some(3600)), t0)
        .unwrap();
    assert_eq!((b.granted, b.position), (false, 0));
    let c = e.acquire_at(&lease("c", None, None, Some(60)), t0).unwrap();
    assert_eq!(c.position, 1);
    assert!(e.release(a.ticket));
    // c keeps polling; b went quiet.
    let c1 = e.acquire_at(&again(&c, None), t0 + 20).unwrap();
    assert!(!c1.granted, "b still within the poll window");
    let c2 = e
        .acquire_at(&again(&c, None), t0 + WAITER_POLL_SECS + 1)
        .unwrap();
    assert!(c2.granted, "{c2:?}");
}

/// C10: pid 1 and other users' processes cannot hold a lease; a reused pid
/// does not keep one.
#[test]
fn leases_refuse_pid1_and_end_when_the_pid_is_reused() {
    let (_t, e) = engine();
    one_slot(&e);
    assert_eq!(
        e.acquire(&lease("x", Some(1), None, None)).unwrap_err(),
        LeaseError::PidNotAllowed(1)
    );
    assert_eq!(LeaseError::PidNotAllowed(1).code(), "pid_not_allowed");

    // Our own child, recorded with its start time by a fake table.
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let procs = Arc::new(FakeProcesses::empty().with(pid, "sleep", None, 500));
    let e2 = Engine::with_processes(
        paths,
        LoadedConfig {
            config: Config::default(),
            error: None,
        },
        procs.clone(),
    );
    one_slot(&e2);
    let l = e2.acquire(&lease("build", Some(pid), None, None)).unwrap();
    assert!(l.granted);
    assert_eq!(
        e2.snapshot_state().slots.active[0].pid_start_time,
        Some(500)
    );
    e2.expire_leases_at(now_unix());
    assert_eq!(e2.snapshot_state().slots.active.len(), 1, "same process");
    let _ = child.kill();
    let _ = child.wait();
    e2.expire_leases_at(now_unix());
    assert!(e2.snapshot_state().slots.active.is_empty(), "exited");

    // A live pid whose start time no longer matches: reused.
    let me = std::process::id();
    let procs = FakeProcesses::empty().with(me, "me", None, 700);
    let t = tempfile::tempdir().unwrap();
    let e3 = Engine::with_processes(
        Paths::under(t.path()),
        LoadedConfig {
            config: Config::default(),
            error: None,
        },
        Arc::new(procs),
    );
    one_slot(&e3);
    assert!(
        e3.acquire(&lease("me", Some(me), None, None))
            .unwrap()
            .granted
    );
    e3.state_mut().slots.active[0].pid_start_time = Some(699);
    e3.expire_leases_at(now_unix());
    assert!(e3.snapshot_state().slots.active.is_empty(), "pid reused");
}

/// C9: restoring priorities (shutdown, pressure gone) and Deprioritize read
/// the process now and leave a reused pid alone.
#[test]
fn priority_changes_skip_a_reused_pid() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let before = nice_of(pid).unwrap();
    if before >= 15 {
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    // The table says this pid started at 500; we tracked one that started at 400.
    let (_t, e) = engine_with_procs(
        Config::default(),
        FakeProcesses::empty().with(pid, "sleep", None, 500),
    );
    lock(&e.reniced).insert(
        pid,
        Reniced {
            start_time: 400,
            original: before + 4,
            restore_failed: false,
        },
    );
    e.restore_priorities("t");
    assert_eq!(nice_of(pid), Some(before), "a reused pid is not reniced");
    assert!(lock(&e.reniced).is_empty(), "and no longer tracked");

    // The same process (start time matches) is restored.
    lock(&e.reniced).insert(
        pid,
        Reniced {
            start_time: 500,
            original: before + 4,
            restore_failed: false,
        },
    );
    e.restore_priorities("t");
    assert_eq!(nice_of(pid), Some(before + 4));
    let _ = child.kill();
    let _ = child.wait();
}

/// A scan loop that stopped producing scans shows as stale.
#[test]
fn an_old_scan_is_flagged_stale() {
    let (_t, e) = engine();
    assert_eq!(e.snapshot_state().scan_age_secs, None);
    let interval = e.config().scan_interval_mins * 60;
    e.state_mut().scan = Some(Arc::new(reclaim::ScanReport {
        at_unix: now_unix() - 10,
        ..Default::default()
    }));
    let st = e.snapshot_state();
    assert!(st.scan_age_secs.unwrap() >= 10 && !st.scan_stale);
    e.state_mut().scan = Some(Arc::new(reclaim::ScanReport {
        at_unix: now_unix() - 2 * interval - 700,
        ..Default::default()
    }));
    assert!(e.snapshot_state().scan_stale);
    assert!(e.health_summary().scan_stale);
}

fn member(f: &crate::process::ProcFacts, kind: ProcKind) -> crate::consumers::ProcSample {
    crate::consumers::ProcSample {
        pid: f.pid,
        ppid: f.ppid,
        own: true,
        name: f.name.clone(),
        exe: f.exe.clone(),
        workspace: None,
        kind,
        cpu_pct: 50.0,
        mem_bytes: 1,
        start_time: f.start_time,
        stopped: false,
    }
}

/// Group plans come from the latest sample, carry start times, refuse the
/// caller's ancestors, and execute through the gate.
#[test]
fn group_plans_refuse_ancestors_and_execute_through_the_gate() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut procs = FakeProcesses::empty().with(pid, "sleep", None, 777);
    procs.procs[0].exe = Some(PathBuf::from("/bin/sleep"));
    procs.procs[0].ppid = 1;
    // A caller whose parent is the child (so the child is its ancestor).
    let mut caller = procs.procs[0].clone();
    caller.pid = 4_000_001;
    caller.ppid = pid;
    caller.name = "agent".into();
    procs.procs.push(caller);
    let (_t, e) = engine_with_procs(Config::default(), procs.clone());
    let key = "process:sleep".to_string();
    let snap = Snapshot {
        members: Arc::new([(key.clone(), vec![member(&procs.procs[0], ProcKind::Other)])].into()),
        ..Default::default()
    };
    e.state_mut().snapshot = Arc::new(snap);

    let req = |caller: Option<u32>, action| crate::procplan::ProcessPlanRequest {
        group: Some(key.clone()),
        action: Some(action),
        caller_pid: caller,
        ..Default::default()
    };
    let p = e.process_plan(&req(Some(4_000_001), crate::consumers::GroupAction::Quit));
    assert_eq!(p.refused.as_deref(), Some("protected"), "{p:?}");
    assert!(!p.ready && p.items.is_empty());

    let p = e.process_plan(&req(None, crate::consumers::GroupAction::Restart));
    assert_eq!(p.refused.as_deref(), Some("restart_unavailable"));

    let p = e.process_plan(&req(None, crate::consumers::GroupAction::Quit));
    assert!(p.ready, "{p:?}");
    assert_eq!(
        p.approved(),
        vec![Request::Signal {
            pid,
            signal: Signal::Term,
            expected_start_time: 777
        }]
    );
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["items"][0]["request"]["expected_start_time"], 777);
    assert_eq!(v["affected"][0]["how"], "signalled");
    let r = e.execute(&p.approved(), "t");
    assert!(r[0].is_done(), "{r:?}");
    let status = child.wait().unwrap();
    assert!(!status.success(), "terminated by the signal");

    let p = e.process_plan(&crate::procplan::ProcessPlanRequest {
        group: Some("app:/Applications/Nothing.app".into()),
        action: Some(crate::consumers::GroupAction::Quit),
        ..Default::default()
    });
    assert_eq!(p.refused.as_deref(), Some("unknown_group"));
}

fn info(pid: u32, start_time: u64, kind: ProcKind) -> crate::monitor::ProcInfo {
    crate::monitor::ProcInfo {
        pid,
        ppid: 1,
        name: "sleep".into(),
        kind,
        group: "r".into(),
        workspace: Some(PathBuf::from("/code/r")),
        own: true,
        cpu_pct: 0.0,
        mem_bytes: 0,
        read_bps: 0,
        write_bps: 0,
        nice: 0,
        stopped: false,
        run_secs: 1,
        start_time,
        cmd: "sleep 30".into(),
    }
}

/// CLI review 1: process and workspace actions act on what the caller
/// previewed (pid + start time), not on the latest snapshot.
#[test]
fn actions_take_the_previewed_start_times() {
    let mut a = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let mut b = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let procs = FakeProcesses::empty()
        .with(a.id(), "sleep", None, 777)
        .with(b.id(), "sleep", None, 888);
    let (_t, e) = engine_with_procs(Config::default(), procs);

    let r = e.process_action_expecting(a.id(), ProcAction::Term, 776, "t");
    assert_eq!(r.reason(), Some(SkipReason::StartTimeMismatch), "{r:?}");
    assert!(a.try_wait().unwrap().is_none(), "still running");

    let ws = crate::monitor::Workspace {
        path: PathBuf::from("/code/r"),
        label: "r".into(),
        repo: "r".into(),
        name: "r".into(),
        branch: None,
        is_worktree: false,
        exists: true,
        paused: false,
        throttled: 0,
        agents: 0,
        builds: 2,
        cpu_pct: 0.0,
        mem_bytes: 0,
        read_bps: 0,
        write_bps: 0,
        orphan: None,
        procs: vec![
            info(a.id(), 777, ProcKind::Build),
            info(b.id(), 888, ProcKind::Build),
        ],
    };
    e.state_mut().snapshot = Arc::new(Snapshot {
        workspaces: vec![ws],
        ..Default::default()
    });
    let only_a = [
        PidRef {
            pid: a.id(),
            expected_start_time: 777,
        },
        PidRef {
            pid: 4_000_999,
            expected_start_time: 1,
        },
    ];
    let res = e
        .workspace_action_on(
            Path::new("/code/r"),
            WorkspaceAction::StopBuilds,
            &only_a,
            "t",
        )
        .unwrap();
    assert_eq!(res[0].reason(), Some(SkipReason::UnknownTarget));
    assert!(res[1].is_done(), "{res:?}");
    assert!(!a.wait().unwrap().success(), "a was stopped");
    assert!(
        b.try_wait().unwrap().is_none(),
        "b was not in the previewed set"
    );
    let _ = b.kill();
    let _ = b.wait();
}

// ------------------------------------------------------------------ watch

struct FakeProbe(Vec<crate::watch::Listener>);

impl crate::watch::WatchProbe for FakeProbe {
    fn listeners(&self) -> Option<Vec<crate::watch::Listener>> {
        Some(self.0.clone())
    }
    fn signature(&self, _: &Path) -> Option<crate::watch::SigStatus> {
        None
    }
    fn quarantined(&self, _: &Path) -> bool {
        false
    }
}

fn fact(
    pid: u32,
    ppid: u32,
    uid: u32,
    name: &str,
    exe: Option<PathBuf>,
) -> crate::process::ProcFacts {
    crate::process::ProcFacts {
        pid,
        ppid,
        uid: Some(uid),
        name: name.into(),
        exe,
        cmd: vec![name.into(), "--password".into(), "hunter2hunter2".into()],
        cwd: Some(PathBuf::from("/")),
        start_time: 1_000 + pid as u64,
    }
}

#[test]
fn watch_findings_reach_state_health_and_the_journal_and_can_be_dismissed() {
    use crate::consumers::Severity;
    use crate::watch::{FindingKind, Listener};
    let dir = tempfile::tempdir().unwrap();
    // A tempdir is under the system temporary directory.
    let exe = dir.path().canonicalize().unwrap().join("payload");
    std::fs::write(&exe, b"x").unwrap();
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    procs
        .procs
        .push(fact(10, 5, uid, "payload", Some(exe.clone())));
    let (_t, e) = engine_with_procs(Config::default(), procs);
    let e = e.with_watch_probe(Arc::new(FakeProbe(vec![Listener {
        pid: 10,
        addr: "0.0.0.0".into(),
        port: 4444,
    }])));
    let f = e.run_watch();
    let kinds: Vec<FindingKind> = f.iter().map(|f| f.kind).collect();
    assert!(kinds.contains(&FindingKind::TempExecutable), "{f:#?}");
    assert!(kinds.contains(&FindingKind::ExposedListener), "{f:#?}");
    let temp = f
        .iter()
        .find(|f| f.kind == FindingKind::TempExecutable)
        .unwrap();
    assert_eq!(
        temp.severity,
        Severity::High,
        "listening beyond loopback from a temp dir"
    );
    assert!(!temp.evidence.cmd.as_deref().unwrap().contains("hunter2"));

    let st = e.snapshot_state();
    assert_eq!(st.findings.len(), f.len());
    assert!(st.findings_unix.is_some());
    assert_eq!(e.health_summary().findings.high, 1);
    let journaled: Vec<&Action> = st.actions.iter().filter(|a| a.kind == "finding").collect();
    assert_eq!(journaled.len(), f.len());
    assert!(journaled.iter().all(|a| a.source == "watch"));

    // A second run journals nothing new and keeps first_seen.
    let again = e.run_watch();
    assert_eq!(again[0].first_seen_unix, f[0].first_seen_unix);
    let n = e
        .snapshot_state()
        .actions
        .iter()
        .filter(|a| a.kind == "finding")
        .count();
    assert_eq!(n, f.len());

    // Dismiss: saved in the config, applied now and on later runs.
    let id = temp.id.clone();
    let until = e.dismiss_finding(&id, Some(2)).unwrap();
    assert!(until > now_unix() + 86_400);
    assert_eq!(e.config().watch.dismissed.len(), 1);
    assert!(Config::load_from(e.paths()).config.watch.dismissed[0].id == id);
    assert_eq!(e.health_summary().findings.high, 0);
    assert_eq!(e.health_summary().findings.dismissed, 1);
    e.run_watch();
    assert!(e
        .findings(None, None, true)
        .iter()
        .any(|f| f.id == id && f.dismissed));
    assert!(!e.findings(None, None, false).iter().any(|f| f.id == id));
    assert_eq!(
        e.dismiss_finding(&id, Some(0)).unwrap_err().0,
        "invalid_body"
    );
    assert!(e.undismiss_finding(&id).unwrap());
    assert!(!e.undismiss_finding(&id).unwrap());
    assert_eq!(e.health_summary().findings.high, 1);

    // Turning watch off clears findings.
    let mut cfg = e.config();
    cfg.watch.enabled = false;
    e.set_config(cfg).unwrap();
    assert!(e.run_watch().is_empty());
    assert!(e.snapshot_state().findings.is_empty());
}

#[test]
fn findings_that_go_away_are_journaled() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().canonicalize().unwrap().join("payload");
    std::fs::write(&exe, b"x").unwrap();
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    procs.procs.push(fact(10, 5, uid, "payload", Some(exe)));
    let (_t, e) = engine_with_procs(Config::default(), procs);
    assert!(!e.run_watch().is_empty());
    // Same engine state, a process table without it.
    let e2 = Engine {
        procs: Arc::new(FakeProcesses::empty()),
        ..e.clone()
    };
    assert!(e2.run_watch().is_empty());
    assert!(e2
        .snapshot_state()
        .actions
        .iter()
        .any(|a| a.kind == "finding" && a.detail.starts_with("No longer seen")));
}

#[test]
fn dismissing_is_refused_while_the_config_file_is_broken() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let e = Engine::with_processes(
        paths,
        LoadedConfig {
            config: Config::default(),
            error: Some(ConfigError::Invalid(vec!["x".into()])),
        },
        Arc::new(FakeProcesses::empty()),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe));
    assert_eq!(
        e.dismiss_finding("a", None).unwrap_err().0,
        "config_not_loaded"
    );
    let f = e.run_watch();
    assert!(f
        .iter()
        .any(|f| f.kind == crate::watch::FindingKind::ConfigNotLoaded));
}

#[test]
fn process_detail_shows_the_family_and_hides_other_users_command_lines() {
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    procs
        .procs
        .push(fact(2, 1, uid, "shell", Some("/bin/zsh".into())));
    procs
        .procs
        .push(fact(3, 2, uid, "node", Some("/usr/local/bin/node".into())));
    procs.procs.push(fact(4, 3, uid, "esbuild", None));
    procs.procs.push(fact(5, 3, uid, "vite", None));
    procs.procs.push(fact(6, 1, uid + 1, "theirs", None));
    let (_t, e) = engine_with_procs(Config::default(), procs);
    let d = e.process_detail(3).unwrap();
    assert!(d.own);
    assert_eq!(d.start_time, 1_003);
    assert_eq!(
        d.parents.iter().map(|p| p.pid).collect::<Vec<_>>(),
        vec![2],
        "pid 1 is not in the fake table"
    );
    assert_eq!(
        d.children.iter().map(|c| c.pid).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert_eq!(d.cmd, vec!["node", "--password", crate::redact::REDACTED]);
    assert!(d.protected.is_none());
    assert_eq!(d.listening, None, "unknown before a watch run");
    let o = e.process_detail(6).unwrap();
    assert!(!o.own && o.cmd.is_empty() && o.cwd.is_none());
    assert_eq!(o.protected.as_deref(), Some("another user's process"));
    assert!(e.process_detail(99).is_none());
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["parents"][0]["name"], "shell");
}

/// R1: a ticket is renewed or polled only with its secret, and a renewal
/// never shortens a lease or adds a TTL to a pid lease.
#[test]
fn renewing_or_polling_needs_the_secret_and_never_shortens() {
    let (_t, e) = engine();
    one_slot(&e);
    let a = e
        .acquire_at(&lease("a", None, None, Some(600)), 100)
        .unwrap();
    assert!(a.granted);
    let b = e
        .acquire_at(&lease("b", None, None, Some(600)), 100)
        .unwrap();
    assert!(!b.granted);
    // Someone who only read the ticket id from state.
    let steal = lease("x", None, Some(a.ticket), Some(1));
    assert_eq!(
        e.acquire_at(&steal, 101).unwrap_err(),
        LeaseError::SecretRequired
    );
    let wrong = LeaseRequest {
        secret: Some("0".repeat(32)),
        ..steal.clone()
    };
    assert_eq!(
        e.acquire_at(&wrong, 101).unwrap_err(),
        LeaseError::WrongSecret
    );
    assert_eq!(
        e.acquire_at(&lease("x", None, Some(b.ticket), None), 101)
            .unwrap_err(),
        LeaseError::SecretRequired,
        "polling a waiter needs its secret too"
    );
    // The owner asking for a shorter TTL keeps the later expiry.
    let r = e.acquire_at(&again(&a, Some(1)), 101).unwrap();
    assert_eq!(r.expires_unix, Some(700));
    e.expire_leases_at(200);
    assert_eq!(e.snapshot_state().slots.active.len(), 1);
    let r = e.acquire_at(&again(&a, Some(900)), 102).unwrap();
    assert_eq!(r.expires_unix, Some(1002), "extending is fine");
    // A pid lease never gains a TTL.
    let (_t2, e2) = engine();
    one_slot(&e2);
    let p = e2
        .acquire_at(&lease("p", Some(std::process::id()), None, None), 100)
        .unwrap();
    assert!(p.granted);
    let r = e2.acquire_at(&again(&p, Some(5)), 101).unwrap();
    assert_eq!(r.expires_unix, None);
    e2.expire_leases_at(10_000);
    assert_eq!(e2.snapshot_state().slots.active[0].ttl_secs, None);
}

/// R11: when no random secret can be made, no lease is made either.
#[test]
fn no_secret_no_lease() {
    let (_t, e) = engine();
    one_slot(&e);
    FAIL_SECRETS.with(|f| f.set(true));
    let r = e.acquire_at(&lease("a", None, None, Some(60)), 100);
    FAIL_SECRETS.with(|f| f.set(false));
    assert!(matches!(r, Err(LeaseError::SecretUnavailable(_))), "{r:?}");
    let st = e.snapshot_state();
    assert!(st.slots.active.is_empty() && st.slots.waiting.is_empty());
    assert!(e
        .acquire_at(&lease("a", None, None, Some(60)), 100)
        .unwrap()
        .release_secret
        .is_some());
}

/// R5: a pid or tree plan marks the caller and its ancestors (its shell,
/// terminal, agent) as blocked, with a reason.
#[test]
fn pid_and_tree_plans_block_the_callers_ancestors() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    // child (the agent) -> 4_000_001 (its shell) -> 4_000_002 (the caller).
    let mut procs = FakeProcesses::empty().with(pid, "agent", None, 777);
    procs.procs[0].ppid = 1;
    let mut shell = procs.procs[0].clone();
    shell.pid = 4_000_001;
    shell.ppid = pid;
    shell.name = "zsh".into();
    let mut caller = shell.clone();
    caller.pid = 4_000_002;
    caller.ppid = 4_000_001;
    caller.name = "mulchie".into();
    procs.procs.push(shell);
    procs.procs.push(caller);
    let (_t, e) = engine_with_procs(Config::default(), procs);
    let req = |tree| crate::procplan::ProcessPlanRequest {
        pid: Some(pid),
        tree,
        caller_pid: Some(4_000_002),
        ..Default::default()
    };
    for tree in [false, true] {
        let p = e.process_plan(&req(tree));
        assert!(!p.ready, "tree={tree}: {p:?}");
        assert!(!p.items.is_empty());
        for i in &p.items {
            assert!(!i.ready, "{i:?}");
            assert_eq!(i.reason, Some(SkipReason::ProtectedProcess), "{i:?}");
            assert!(i.verdict.contains("never signalled"), "{}", i.verdict);
        }
    }
    // Without a caller the same pid can be planned.
    let p = e.process_plan(&crate::procplan::ProcessPlanRequest {
        pid: Some(pid),
        ..Default::default()
    });
    assert!(p.ready, "{p:?}");
    let _ = child.kill();
    let _ = child.wait();
}

/// R8: config writes are one read-modify-write under a lock. A full
/// replacement over a file that changed on disk is refused (and the engine
/// takes the file's content); a dismissal is merged into the file's content.
#[test]
fn config_writes_detect_changes_on_disk() {
    let (_t, a) = engine();
    let paths = a.paths().clone();
    let b = Engine::with_processes(
        paths.clone(),
        Config::load_from(&paths),
        Arc::new(FakeProcesses::empty()),
    )
    .with_watch_probe(Arc::new(crate::watch::NoProbe));

    // A writes; B's copy is now stale, so B's replacement is refused.
    let mut ca = a.config();
    ca.build_nice = 15;
    a.set_config(ca).unwrap();
    let mut cb = b.config();
    cb.worktree_idle_days = 9;
    let err = b.set_config(cb).unwrap_err();
    assert!(matches!(err, ConfigError::Changed(_)), "{err}");
    assert_eq!(b.config().build_nice, 15, "B took the file's content");
    assert_eq!(
        Config::load_from(&paths).config.worktree_idle_days,
        Config::default().worktree_idle_days
    );
    // Read again, reapply: accepted, and A's change is kept.
    let mut cb = b.config();
    cb.worktree_idle_days = 9;
    b.set_config(cb).unwrap();
    let on_disk = Config::load_from(&paths).config;
    assert_eq!((on_disk.build_nice, on_disk.worktree_idle_days), (15, 9));

    // A dismissal on A (whose copy is stale again) merges into the file.
    a.dismiss_finding("f1", Some(1)).unwrap();
    let on_disk = Config::load_from(&paths).config;
    assert_eq!(on_disk.worktree_idle_days, 9, "B's change survived");
    assert!(on_disk.watch.dismissed.iter().any(|d| d.id == "f1"));

    // Concurrent dismissals on one engine: none is lost.
    std::thread::scope(|s| {
        for i in 0..8 {
            let a = &a;
            s.spawn(move || a.dismiss_finding(&format!("c{i}"), Some(1)).unwrap());
        }
    });
    let on_disk = Config::load_from(&paths).config;
    for i in 0..8 {
        assert!(
            on_disk
                .watch
                .dismissed
                .iter()
                .any(|d| d.id == format!("c{i}")),
            "c{i} lost"
        );
    }
    assert!(on_disk.watch.dismissed.iter().any(|d| d.id == "f1"));
}

/// A probe whose listeners and signatures a test switches between runs.
struct SwitchProbe {
    listeners: Mutex<Option<Vec<crate::watch::Listener>>>,
    sig: Mutex<Option<crate::watch::SigStatus>>,
    sig_calls: AtomicU64,
}

impl crate::watch::WatchProbe for SwitchProbe {
    fn listeners(&self) -> Option<Vec<crate::watch::Listener>> {
        self.listeners.lock().unwrap().clone()
    }
    fn signature(&self, _: &Path) -> Option<crate::watch::SigStatus> {
        self.sig_calls.fetch_add(1, Ordering::SeqCst);
        *self.sig.lock().unwrap()
    }
    fn quarantined(&self, _: &Path) -> bool {
        false
    }
}

/// R9 through the engine: an exposed-listener finding survives a run where
/// listeners are unknown; a failed signature check is tried again next run.
#[test]
fn unknown_listeners_keep_findings_and_failed_signatures_are_retried() {
    use crate::watch::{FindingKind, Listener};
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("server");
    std::fs::write(&exe, b"x").unwrap();
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    procs
        .procs
        .push(fact(10, 5, uid, "server", Some(exe.clone())));
    let (_t, e) = engine_with_procs(Config::default(), procs);
    let probe = Arc::new(SwitchProbe {
        listeners: Mutex::new(Some(vec![Listener {
            pid: 10,
            addr: "0.0.0.0".into(),
            port: 4444,
        }])),
        sig: Mutex::new(None),
        sig_calls: AtomicU64::new(0),
    });
    let e = e.with_watch_probe(probe.clone());
    let has =
        |f: &[crate::watch::Finding]| f.iter().any(|f| f.kind == FindingKind::ExposedListener);
    assert!(has(&e.run_watch()));
    let calls = probe.sig_calls.load(Ordering::SeqCst);
    *probe.listeners.lock().unwrap() = None;
    assert!(has(&e.run_watch()), "kept while listeners are unknown");
    assert!(
        probe.sig_calls.load(Ordering::SeqCst) > calls,
        "a failed signature check is not cached"
    );
    *probe.listeners.lock().unwrap() = Some(Vec::new());
    assert!(!has(&e.run_watch()), "gone once known again");
}

/// Counts quarantine reads; every file is quarantined.
struct QuarProbe(AtomicU64);

impl crate::watch::WatchProbe for QuarProbe {
    fn listeners(&self) -> Option<Vec<crate::watch::Listener>> {
        Some(Vec::new())
    }
    fn signature(&self, _: &Path) -> Option<crate::watch::SigStatus> {
        None
    }
    fn quarantined(&self, _: &Path) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        true
    }
}

/// R12: quarantine checks reach every candidate in turn (200 per run),
/// and a flag read once is kept while the file is unchanged.
#[test]
fn quarantine_checks_rotate_through_every_candidate() {
    use crate::watch::FindingKind;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut procs = FakeProcesses::empty();
    let uid = procs.uid;
    for i in 0..250u32 {
        let exe = base.join(format!("tool{i}"));
        std::fs::write(&exe, b"x").unwrap();
        procs.procs.push(fact(100 + i, 5, uid, "tool", Some(exe)));
    }
    let (_t, e) = engine_with_procs(Config::default(), procs);
    let probe = Arc::new(QuarProbe(AtomicU64::new(0)));
    let e = e.with_watch_probe(probe.clone());
    let count = |f: &[crate::watch::Finding]| {
        f.iter()
            .filter(|f| f.kind == FindingKind::QuarantinedExecutable)
            .count()
    };
    let first = count(&e.run_watch());
    assert_eq!(probe.0.load(Ordering::SeqCst), 200);
    let second = count(&e.run_watch());
    assert_eq!(
        probe.0.load(Ordering::SeqCst),
        250,
        "the other 50, not the same 200"
    );
    assert!(second > first, "{first} then {second}");
    e.run_watch();
    assert_eq!(
        probe.0.load(Ordering::SeqCst),
        250,
        "known flags are not read again"
    );
}
