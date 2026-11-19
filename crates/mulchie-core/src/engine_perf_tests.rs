//! Commit attribution and timings through the engine, against real
//! temporary git repositories. Nothing here signals or deletes.

use super::*;
use crate::engine::tests::engine_with;
use crate::watch::{FindingCategory, FindingKind};

/// Builds off, budgets off, memory guard, runaway chains and update checks
/// off: an engine that holds the lock here cannot act on real processes.
fn safe() -> Config {
    let mut c = Config {
        throttle_builds: false,
        ..Config::default()
    };
    c.memory_guard.action = crate::config::MemoryAction::Off;
    c.runaway.action = crate::config::MemoryAction::Off;
    c.updates.check = crate::config::UpdateCheck::Off;
    c
}

struct Repo {
    dir: PathBuf,
    home: PathBuf,
}

impl Repo {
    fn new(home: &Path, name: &str) -> Self {
        let dir = home.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let r = Self {
            dir: std::fs::canonicalize(&dir).unwrap(),
            home: home.to_path_buf(),
        };
        r.git(&["init", "-q", "-b", "main"]);
        r.commit("first");
        r
    }

    fn git(&self, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("HOME", &self.home)
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {o:?}");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn commit(&self, msg: &str) -> String {
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

/// A running session of `agent` working in `ws`.
fn session(e: &Engine, ws: &Path, agent: &str, now: u64) -> String {
    let mut sup = lock(&e.sup);
    let id = sup.tracker.register(
        now,
        4242,
        now,
        agent,
        agent.into(),
        Some(agent.into()),
        None,
        Some(ws.to_path_buf()),
        Some(ws.to_path_buf()),
    );
    sup.tracker.touch(&id, now);
    id
}

fn report(ws: &Path, secs: u64) -> TimingReport {
    TimingReport {
        duration_ms: Some(secs * 1000),
        exit_code: Some(0),
        command: Some("cargo test --workspace".into()),
        cwd: Some(ws.to_path_buf()),
    }
}

#[test]
fn a_git_commit_reported_by_a_hook_is_attributed_to_its_session() {
    let (t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let repo = Repo::new(&e.paths.home, "repo-x");
    let first = repo.head();
    let now = 1_000;
    let sid = session(&e, &repo.dir, "claude", now);
    // The first look at a workspace is recorded without a session.
    e.perf_tick(now);
    let c = e.commits(Some(&repo.dir), None, 10);
    assert_eq!(c.total, 1);
    assert_eq!(c.commits[0].head, first);
    assert!(c.commits[0].previous_head.is_none() && c.commits[0].session_id.is_none());

    let second = repo.commit("second");
    // A command that does not move HEAD queues nothing.
    e.note_shell_command(&repo.dir, "git status", Some(&sid));
    assert!(lock(&e.perf.state).jobs.is_empty());
    e.note_shell_command(&repo.dir, "cargo fmt && git commit -am x", Some(&sid));
    e.perf_tick(now + 1);
    let c = e.commits(Some(&repo.dir), None, 10);
    assert_eq!(c.total, 2);
    let m = &c.commits[0];
    assert_eq!(
        (m.head.as_str(), m.previous_head.as_deref()),
        (second.as_str(), Some(first.as_str()))
    );
    assert_eq!(m.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(m.agent.as_deref(), Some("claude"));
    assert_eq!(
        (m.via.as_str(), m.branch.as_deref()),
        ("hook", Some("main"))
    );

    // The public attribution API.
    let a = e.attribute(&repo.dir, &second[..12]).unwrap();
    assert_eq!(a.session_id.as_deref(), Some(sid.as_str()));
    let between = e.commits_between(&repo.dir, &first, &second);
    assert_eq!(between.len(), 1);

    // Stored privately, one line per move.
    use std::os::unix::fs::PermissionsExt;
    let f = e.paths.config_dir.join("commits.jsonl");
    assert_eq!(std::fs::read_to_string(&f).unwrap().lines().count(), 2);
    let mode = std::fs::metadata(&f).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // A restart reads the last HEAD back: an unchanged HEAD is no move.
    let e2 = Engine::with_processes(
        (*e.paths).clone(),
        LoadedConfig {
            config: safe(),
            error: None,
        },
        Arc::new(crate::process::FakeProcesses::empty()),
    );
    session(&e2, &repo.dir, "claude", now + 100);
    e2.perf_tick(now + 100);
    assert_eq!(e2.commits(Some(&repo.dir), None, 10).total, 2);
    drop(t);
}

#[test]
fn the_periodic_check_finds_a_move_and_names_the_session_working_there() {
    let (_t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let repo = Repo::new(&e.paths.home, "repo-y");
    let now = 5_000;
    let sid = session(&e, &repo.dir, "codex", now);
    e.perf_tick(now);
    let next = repo.commit("two");
    // Not due yet: nothing read.
    e.perf_tick(now + 5);
    assert_eq!(e.commits(Some(&repo.dir), None, 10).total, 1);
    e.perf_tick(now + 61);
    let c = e.commits(Some(&repo.dir), None, 10);
    assert_eq!(c.total, 2);
    assert_eq!(c.commits[0].head, next);
    assert_eq!(c.commits[0].via, "poll");
    assert_eq!(c.commits[0].session_id.as_deref(), Some(sid.as_str()));
}

#[test]
fn slower_builds_become_a_performance_finding_and_an_issue_that_the_watch_keeps() {
    let (_t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let repo = Repo::new(&e.paths.home, "repo-x");
    let holder = || ("repo-x: cargo test".to_string(), None);
    for _ in 0..10 {
        e.queue_timing(report(&repo.dir, 120), holder()).unwrap();
    }
    e.perf_tick(10_000);
    let before = repo.head();
    let sid = session(&e, &repo.dir, "claude", 10_001);
    let after = repo.commit("make it slow");
    e.note_shell_command(&repo.dir, "git commit -m slow", Some(&sid));
    for _ in 0..3 {
        e.queue_timing(report(&repo.dir, 250), holder()).unwrap();
    }
    e.perf_tick(10_002);

    let tl = e.timings(Some(&repo.dir), Some("cargo test"), 100);
    assert_eq!(tl.total, 13);
    assert_eq!(tl.timings[0].head.as_deref(), Some(after.as_str()));
    assert_eq!(tl.timings[0].workspace.as_deref(), Some(repo.dir.as_path()));
    assert_eq!(tl.timings[0].label, "repo-x: cargo test");
    assert_eq!(tl.timings[12].head.as_deref(), Some(before.as_str()));
    assert!(e.paths.config_dir.join("timings.jsonl").is_file());

    let perf = |e: &Engine| -> Vec<Finding> {
        e.snapshot_state()
            .findings
            .into_iter()
            .filter(|f| f.category == FindingCategory::Performance)
            .collect()
    };
    let f = perf(&e);
    assert_eq!(f.len(), 1, "{f:#?}");
    assert_eq!(f[0].kind, FindingKind::BuildSlower);
    let p = f[0].evidence.perf.as_ref().unwrap();
    assert_eq!(p.commits.len(), 1);
    assert_eq!(p.commits[0].session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(
        e.issues(None, false, Some(FindingKind::BuildSlower))
            .issues
            .len(),
        1
    );
    // A watch run replaces only the categories it owns.
    e.run_watch();
    assert_eq!(perf(&e).len(), 1);

    // Faster again: the finding goes.
    for _ in 0..3 {
        e.queue_timing(report(&repo.dir, 118), holder()).unwrap();
    }
    e.perf_tick(10_003);
    assert!(perf(&e).is_empty());
}

#[test]
fn a_bad_timing_is_refused_before_it_is_queued() {
    let (_t, e) = engine_with(safe());
    let r = TimingReport {
        cwd: Some("relative".into()),
        ..report(Path::new("/w"), 1)
    };
    assert!(e.queue_timing(r, ("x".into(), None)).is_err());
    assert!(lock(&e.perf.state).jobs.is_empty());
}

#[test]
fn an_engine_without_the_lock_writes_no_files() {
    let (_t, e) = engine_with(safe());
    let repo = Repo::new(&e.paths.home, "repo-z");
    e.queue_timing(report(&repo.dir, 10), ("x".into(), None))
        .unwrap();
    e.perf_tick(1);
    assert_eq!(e.timings(None, None, 10).total, 1);
    assert!(!e.paths.config_dir.join("timings.jsonl").exists());
    assert!(!e.paths.config_dir.join("commits.jsonl").exists());
}

// ------------------------------------------------------- dev servers

use crate::devservers::End;

fn dev_sample(pid: u32, ppid: u32, start: u64, ws: &Path, mb: u64) -> crate::consumers::ProcSample {
    crate::consumers::ProcSample {
        pid,
        ppid,
        own: false,
        name: "node".into(),
        exe: None,
        workspace: Some((ws.to_path_buf(), "repo-x".into())),
        kind: crate::monitor::ProcKind::Other,
        cpu_pct: 20.0,
        mem_bytes: mb * 1_048_576,
        swap_bytes: 0,
        start_time: start,
        stopped: false,
    }
}

fn dev_members(v: Vec<crate::consumers::ProcSample>) -> crate::consumers::Members {
    HashMap::from([("workspace:x".to_string(), v)])
}

fn dev_facts(pid: u32, ppid: u32, uid: u32, start: u64, ws: &Path) -> crate::process::ProcFacts {
    crate::process::ProcFacts {
        pid,
        ppid,
        uid: Some(uid),
        name: "node".into(),
        exe: None,
        cmd: vec![
            "node".into(),
            "next".into(),
            "dev".into(),
            "--token=abcdefghijklmnop1234567890".into(),
        ],
        cwd: Some(ws.to_path_buf()),
        start_time: start,
    }
}

fn listener(pid: u32, port: u16) -> crate::watch::Listener {
    crate::watch::Listener {
        pid,
        addr: "127.0.0.1".into(),
        port,
    }
}

fn dev_record(start: u64, pid: u32, ws: &Path, mem_mb: u64, head: &str) -> DevServer {
    DevServer {
        at_unix: start + 4_000,
        pid,
        start_unix: start,
        workspace: ws.to_path_buf(),
        branch: Some("main".into()),
        head: Some(head.into()),
        session_id: None,
        agent: None,
        command: "next dev".into(),
        key: "next dev".into(),
        ports: vec![3000],
        listen_ms: None,
        listen_uncertainty_ms: None,
        watch_interval_secs: 60,
        settled_mem_bytes: Some(mem_mb * 1_048_576),
        mem_slope_mb_per_hour: None,
        peak_cpu_pct: 10.0,
        peak_mem_bytes: 0,
        processes: 1,
        run_secs: 4_000,
        end: End::Exited,
    }
}

#[test]
fn a_dev_server_is_seen_from_the_watch_and_the_samples_and_recorded_when_it_ends() {
    let (_t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let repo = Repo::new(&e.paths.home, "repo-x");
    let uid = e.procs.current_uid();
    let now = crate::monitor::now_unix();
    let start = now - 400;
    let procs = vec![
        dev_facts(10, 1, uid, start, &repo.dir),
        dev_facts(11, 10, uid, start + 1, &repo.dir),
    ];
    let samples: HashMap<u32, crate::consumers::ProcSample> = procs
        .iter()
        .map(|p| {
            (
                p.pid,
                dev_sample(p.pid, p.ppid, p.start_time, &repo.dir, 300),
            )
        })
        .collect();
    // The watch before it listened, and the one after.
    e.observe_dev_listeners(now - 330, 60, uid, &procs, &samples, Some(&[]));
    e.observe_dev_listeners(
        now - 270,
        60,
        uid,
        &procs,
        &samples,
        Some(&[listener(10, 3000)]),
    );
    // A sample reads the tree's memory and CPU.
    let members = dev_members(
        procs
            .iter()
            .map(|p| dev_sample(p.pid, p.ppid, p.start_time, &repo.dir, 300))
            .collect(),
    );
    e.observe_dev_sample(now - 100, &members);
    let l = e.dev_servers(Some(&repo.dir), 10);
    assert_eq!(l.total, 1, "{l:#?}");
    let r = &l.dev_servers[0];
    assert_eq!(r.end, End::Running);
    assert_eq!(r.listen_ms, Some((now - 270 - start) * 1000));
    assert_eq!(r.listen_uncertainty_ms, Some(61_000));
    assert_eq!(r.ports, vec![3000]);
    assert_eq!(r.processes, 2);
    assert_eq!(r.peak_cpu_pct, 40.0);
    assert!(r.command.contains("next dev"), "{}", r.command);
    assert!(!r.command.contains("abcdefghijklmnop"), "{}", r.command);
    assert!(r.key.starts_with("node next dev"), "{}", r.key);

    // The worker attributes it: branch and HEAD of the workspace.
    e.perf_tick(now);
    let r = &e.dev_servers(None, 10).dev_servers[0];
    assert_eq!(r.head.as_deref(), Some(repo.head().as_str()));
    assert_eq!(r.branch.as_deref(), Some("main"));
    assert!(
        !e.paths.config_dir.join("devservers.jsonl").exists(),
        "nothing is written while it runs and is young"
    );

    // It exits (a sample without it), and the worker writes the record.
    e.observe_dev_sample(now, &dev_members(vec![dev_sample(1, 0, 1, &repo.dir, 10)]));
    e.perf_tick(now + 1);
    let l = e.dev_servers(None, 10);
    assert_eq!(l.total, 1);
    assert_eq!(l.dev_servers[0].end, End::Exited);
    let on_disk: Vec<DevServer> = devservers::file(&e.paths).read_all();
    assert_eq!(on_disk.len(), 1);
    assert_eq!(on_disk[0].pid, 10);

    // An engine that starts later reads the run from the file.
    let later = Engine::with_processes(
        (*e.paths).clone(),
        crate::config::LoadedConfig {
            config: safe(),
            error: None,
        },
        Arc::new(crate::process::FakeProcesses::empty()),
    );
    let l = later.dev_servers(Some(&repo.dir), 10);
    assert_eq!(l.total, 1);
    assert_eq!(l.dev_servers[0].pid, 10);
    assert_eq!(
        later.dev_servers(Some(Path::new("/elsewhere")), 10).total,
        0
    );
}

#[test]
fn heavier_dev_server_runs_become_a_performance_finding_with_the_commits() {
    let (_t, e) = engine_with(safe());
    let _lock = e.acquire_lock().unwrap();
    let repo = Repo::new(&e.paths.home, "repo-x");
    let first = repo.head();
    // History on disk: ten normal runs, then three heavy ones.
    let file = devservers::file(&e.paths);
    for i in 0..10u64 {
        let r = dev_record(1_000 + i * 10_000, 100 + i as u32, &repo.dir, 500, &first);
        file.append(&r).unwrap();
    }
    let sid = session(&e, &repo.dir, "claude", 5_000);
    e.perf_tick(5_000);
    let second = repo.commit("heavier");
    e.note_shell_command(&repo.dir, "git commit -am heavier", Some(&sid));
    e.perf_tick(5_002);
    // Three heavy runs end (records come in through the worker's list).
    e.with_perf(|p| {
        let v = p.devservers.get_or_insert_with(Vec::new);
        for i in 10..13u64 {
            v.push(dev_record(
                1_000 + i * 10_000,
                100 + i as u32,
                &repo.dir,
                1_400,
                &second,
            ));
        }
        p.dirty = true;
    });
    e.perf_tick(5_003);
    let perf = |e: &Engine| -> Vec<Finding> {
        e.snapshot_state()
            .findings
            .into_iter()
            .filter(|f| f.category == FindingCategory::Performance)
            .collect()
    };
    let f = perf(&e);
    assert_eq!(f.len(), 1, "{f:#?}");
    assert_eq!(f[0].kind, FindingKind::DevServerHeavier);
    let d = f[0].evidence.devserver.as_ref().unwrap();
    assert_eq!(d.recent.len(), 3);
    assert_eq!(d.from_head.as_deref(), Some(first.as_str()));
    assert_eq!(d.to_head.as_deref(), Some(second.as_str()));
    assert_eq!(d.commits.len(), 1);
    assert_eq!(d.commits[0].session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(
        e.issues(None, false, Some(FindingKind::DevServerHeavier))
            .issues
            .len(),
        1
    );
    // A watch run replaces only the categories it owns.
    e.run_watch();
    assert_eq!(perf(&e).len(), 1);
}

#[test]
fn an_engine_without_the_lock_writes_no_dev_server_file() {
    let (_t, e) = engine_with(safe());
    let repo = Repo::new(&e.paths.home, "repo-z");
    let uid = e.procs.current_uid();
    let now = crate::monitor::now_unix();
    let start = now - 900;
    let p = dev_facts(10, 1, uid, start, &repo.dir);
    let samples = HashMap::from([(10, dev_sample(10, 1, start, &repo.dir, 100))]);
    e.observe_dev_listeners(
        now - 800,
        60,
        uid,
        std::slice::from_ref(&p),
        &samples,
        Some(&[]),
    );
    e.observe_dev_listeners(
        now - 700,
        60,
        uid,
        &[p],
        &samples,
        Some(&[listener(10, 5173)]),
    );
    e.observe_dev_sample(
        now - 630,
        &dev_members(vec![dev_sample(10, 1, start, &repo.dir, 100)]),
    );
    e.observe_dev_sample(
        now - 600,
        &dev_members(vec![dev_sample(1, 0, 1, &repo.dir, 10)]),
    );
    e.perf_tick(now);
    assert_eq!(e.dev_servers(None, 10).total, 1);
    assert!(!e.paths.config_dir.join("devservers.jsonl").exists());
}

#[test]
fn only_the_users_own_processes_in_a_workspace_count_and_never_the_engine() {
    let (_t, e) = engine_with(safe());
    let repo = Repo::new(&e.paths.home, "repo-z");
    let uid = e.procs.current_uid();
    let now = crate::monitor::now_unix();
    let start = now - 900;
    let mut other = dev_facts(20, 1, uid + 1, start, &repo.dir);
    other.uid = Some(uid + 1);
    let procs = vec![dev_facts(10, 1, uid, start, &repo.dir), other];
    // Process 10 sits outside any workspace; 20 is someone else's.
    let mut s10 = dev_sample(10, 1, start, &repo.dir, 100);
    s10.workspace = None;
    let samples = HashMap::from([(10, s10), (20, dev_sample(20, 1, start, &repo.dir, 100))]);
    let ls = [listener(10, 3000), listener(20, 3001)];
    e.observe_dev_listeners(now - 800, 60, uid, &procs, &samples, Some(&[]));
    e.observe_dev_listeners(now - 700, 60, uid, &procs, &samples, Some(&ls));
    e.observe_dev_sample(
        now - 600,
        &dev_members(vec![dev_sample(10, 1, start, &repo.dir, 1)]),
    );
    assert_eq!(e.dev_servers(None, 10).total, 0);
}
