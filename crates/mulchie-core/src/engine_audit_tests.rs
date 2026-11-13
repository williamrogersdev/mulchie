//! Tests for rules an audit found untested: who a session plan may signal,
//! that hiding a finding leaves a trace, and what the engine remembers about
//! actions that did not succeed. Paths live under a tempdir and the process
//! table is a fake; nothing here is signalled.

use super::tests::engine_with_procs;
use super::*;
use crate::consumers::{GroupAction, ProcSample};
use crate::git::Git;
use crate::limits::Machine;
use crate::monitor::ProcKind;
use crate::process::FakeProcesses;
use crate::safety::SkipReason;
use crate::session::{Session, SessionProc};

const M: Machine = Machine {
    cores: 8,
    mem_mb: 16_000.0,
    strained: false,
};

fn sproc(pid: u32, ppid: u32, start: u64, agent: Option<&str>) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: start,
        name: agent.unwrap_or("sleep").into(),
        own: true,
        agent: agent.map(String::from),
        cpu_pct: 0.0,
        mem_bytes: 1_048_576,
        written: None,
        stopped: false,
        workspace: Some(PathBuf::from("/code/a")),
        cwd: Some(PathBuf::from("/code/a")),
        cmd: agent.unwrap_or("sleep 60").into(),
    }
}

fn sample(pid: u32, ppid: u32, st: u64) -> ProcSample {
    ProcSample {
        pid,
        ppid,
        own: true,
        name: "sleep".into(),
        exe: None,
        workspace: None,
        kind: ProcKind::Other,
        cpu_pct: 0.0,
        mem_bytes: 1,
        start_time: st,
        stopped: false,
    }
}

/// An engine that has seen one session: `(pid, ppid, start)` rows, the first
/// being the agent.
fn engine_with_session(rows: &[(u32, u32, u64)]) -> (tempfile::TempDir, Engine, String) {
    let mut f = FakeProcesses::empty();
    for (pid, ppid, st) in rows {
        f = f.with(*pid, "sleep", None, *st);
        f.procs.last_mut().unwrap().ppid = *ppid;
    }
    let c = Config::default();
    let (t, e) = engine_with_procs(c.clone(), f);
    let procs: Vec<SessionProc> = rows
        .iter()
        .enumerate()
        .map(|(i, (pid, ppid, st))| sproc(*pid, *ppid, *st, (i == 0).then_some("claude")))
        .collect();
    e.observe_sessions(1_000, &procs, M, &c);
    e.state_mut().snapshot = Arc::new(Snapshot {
        members: Arc::new(HashMap::from([(
            "workspace:a".to_string(),
            rows.iter()
                .map(|(p, pp, st)| sample(*p, *pp, *st))
                .collect(),
        )])),
        ..Snapshot::default()
    });
    let id = Session::id_for(rows[0].0, rows[0].2);
    (t, e, id)
}

fn planned_pids(p: &ProcessPlan) -> Vec<u32> {
    let mut v: Vec<u32> = p
        .items
        .iter()
        .filter_map(|i| match i.request {
            Request::Signal { pid, .. } | Request::Renice { pid, .. } => Some(pid),
            _ => None,
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// An agent that plans an action on its own session (even with
/// include_agent, even a force quit) never gets its own process, its shell
/// or the agent app into the plan: only the rest of the session.
#[test]
fn a_session_plan_never_holds_the_caller_or_its_ancestors() {
    // agent 77_701 -> shell 77_702 -> caller 77_703 (`mulchie mcp`);
    // 77_704 is a build the agent started.
    let rows = [
        (77_701, 1, 500),
        (77_702, 77_701, 600),
        (77_703, 77_702, 700),
        (77_704, 77_701, 800),
    ];
    let (_t, e, id) = engine_with_session(&rows);
    for action in [
        GroupAction::Pause,
        GroupAction::Quit,
        GroupAction::ForceQuit,
    ] {
        for include_agent in [false, true] {
            let p = e.process_plan(&ProcessPlanRequest {
                session: Some(id.clone()),
                action: Some(action),
                include_agent,
                caller_pid: Some(77_703),
                ..ProcessPlanRequest::default()
            });
            assert_eq!(
                planned_pids(&p),
                vec![77_704],
                "{action:?} include_agent={include_agent}: {p:?}"
            );
            assert!(p.detail.contains("left out"), "{}", p.detail);
            assert!(p.affected.iter().all(|a| a.pid == 77_704), "{p:?}");
        }
    }
    // Without a caller (the user, from the app) the whole session is there.
    let p = e.process_plan(&ProcessPlanRequest {
        session: Some(id.clone()),
        action: Some(GroupAction::Pause),
        include_agent: true,
        ..ProcessPlanRequest::default()
    });
    assert_eq!(planned_pids(&p), vec![77_701, 77_702, 77_703, 77_704]);
}

/// A session that is nothing but the caller and what started it is refused
/// as protected, not planned empty and not widened.
#[test]
fn a_session_plan_of_only_the_caller_and_its_ancestors_is_refused() {
    let rows = [
        (77_711, 1, 500),
        (77_712, 77_711, 600),
        (77_713, 77_712, 700),
    ];
    let (_t, e, id) = engine_with_session(&rows);
    let p = e.process_plan(&ProcessPlanRequest {
        session: Some(id),
        action: Some(GroupAction::ForceQuit),
        include_agent: true,
        caller_pid: Some(77_713),
        ..ProcessPlanRequest::default()
    });
    assert!(!p.ready && p.items.is_empty(), "{p:?}");
    assert_eq!(p.refused.as_deref(), Some("protected"));
    assert!(p.detail.contains("never signalled"), "{}", p.detail);
}

/// Hiding a finding is itself in the journal, also when the id names no
/// current finding, and so is showing it again.
#[test]
fn dismissing_a_finding_is_journaled() {
    let (_t, e) = engine_with_procs(Config::default(), FakeProcesses::empty());
    e.dismiss_finding("runaway:123", Some(3650)).unwrap();
    let notes = |e: &Engine| -> Vec<String> {
        e.snapshot_state()
            .actions
            .iter()
            .filter(|a| a.kind == "finding")
            .map(|a| a.detail.clone())
            .collect()
    };
    let n = notes(&e);
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(n[0].contains("Dismissed finding runaway:123 for 3650 day(s)"));
    assert!(n[0].contains("not a current finding"), "{}", n[0]);
    assert!(e.undismiss_finding("runaway:123").unwrap());
    assert!(notes(&e).iter().any(|d| d.contains("again")));
    // Removing a dismissal that is not there writes nothing.
    assert!(!e.undismiss_finding("runaway:123").unwrap());
    assert_eq!(notes(&e).len(), 2);
    // On disk too, not only in memory.
    let on_disk = crate::journal::Journal::new(e.paths()).tail(10);
    assert_eq!(on_disk.iter().filter(|a| a.kind == "finding").count(), 2);
}

struct Repo {
    t: tempfile::TempDir,
    paths: Paths,
    root: PathBuf,
    repo: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let root = t.path().join("code");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let r = Self {
            t,
            paths,
            root,
            repo,
        };
        r.git(&r.repo, &["init", "-q", "-b", "main"]);
        r.git(&r.repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        r
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        Git::new(&self.paths, dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .text()
            .unwrap()
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let p = self.repo.join(".claude/worktrees").join(name);
        self.git(
            &self.repo,
            &["worktree", "add", "-q", "-b", name, p.to_str().unwrap()],
        );
        p
    }

    fn engine(&self) -> Engine {
        Engine::with_processes(
            self.paths.clone(),
            LoadedConfig {
                config: Config {
                    scan_roots: vec![self.root.clone()],
                    ..Config::default()
                },
                error: None,
            },
            Arc::new(FakeProcesses::empty()),
        )
        .with_watch_probe(Arc::new(crate::watch::NoProbe))
    }
}

fn stale_removal(wt: &Path) -> Request {
    Request::RemoveWorktree {
        path: wt.to_path_buf(),
        // Not the worktree's HEAD: skipped as head_moved every time.
        expected_head: "0".repeat(40),
        allow_dirty: false,
        require_idle: false,
    }
}

/// Skips are counted per target, shown from the second in a row, told to
/// whoever plans the same target again, and survive a restart (they are
/// rebuilt from the journal). A success clears them.
#[test]
fn repeated_skips_are_remembered_shown_and_cleared_by_a_success() {
    let r = Repo::new();
    let wt = r.worktree("agent-a");
    let e = r.engine();
    let target = wt.display().to_string();

    let out = e.execute(&[stale_removal(&wt)], "test");
    assert_eq!(out[0].reason(), Some(SkipReason::HeadMoved));
    assert!(
        e.snapshot_state().failures.is_empty(),
        "one skip is not a pattern"
    );
    assert!(e.worktree_plan(std::slice::from_ref(&wt), false).items[0]
        .past_failures
        .is_none());

    e.execute(&[stale_removal(&wt)], "test");
    let f = e.snapshot_state().failures;
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(
        (f[0].kind.as_str(), f[0].target.as_str(), f[0].count),
        ("worktree", target.as_str(), 2)
    );
    assert_eq!(f[0].reason, Some(SkipReason::HeadMoved));
    assert!(f[0].retry_after_unix.is_none(), "no wait before the third");
    assert!(!f[0].advice.is_empty());

    e.execute(&[stale_removal(&wt)], "test");
    let f = e.snapshot_state().failures;
    assert_eq!(f[0].count, 3);
    assert!(f[0].retry_after_unix.unwrap() >= now_unix() + crate::failures::BACKOFF_BASE_SECS - 5);
    // The journal says once that automation now leaves it alone.
    let notes: Vec<Action> = e
        .snapshot_state()
        .actions
        .iter()
        .filter(|a| a.kind == "failure")
        .cloned()
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].detail.contains(&target) && notes[0].detail.contains("3 times"));

    // A plan for the same target is still ready (a person may have fixed
    // the cause) and says what happened before.
    let plan = e.worktree_plan(std::slice::from_ref(&wt), false);
    assert!(plan.items[0].ready, "{plan:?}");
    let past = plan.items[0].past_failures.clone().expect("past failures");
    assert_eq!((past.count, past.reason), (3, Some(SkipReason::HeadMoved)));
    let v = serde_json::to_value(&plan).unwrap();
    assert_eq!(v["items"][0]["past_failures"]["count"], 3);

    // A new engine on the same directory knows it too.
    drop(e);
    let e = r.engine();
    let f = e.snapshot_state().failures;
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].count, 3);

    // The planned removal goes through, and the record is gone.
    let out = e.execute(
        &e.worktree_plan(std::slice::from_ref(&wt), false).approved(),
        "test",
    );
    assert!(out[0].is_done(), "{out:?}");
    assert!(e.snapshot_state().failures.is_empty());
    assert!(lock(&e.failures)
        .get("worktree", &target, now_unix())
        .is_none());
    let _ = &r.t;
}

/// After three skips in a row the engine's own passes leave the target alone
/// for a while, without running it and without another journal line; an
/// explicit execute still runs.
#[test]
fn automatic_passes_hold_back_a_target_that_keeps_failing() {
    let r = Repo::new();
    let wt = r.worktree("agent-a");
    let other = r.worktree("agent-b");
    let e = r.engine();
    for _ in 0..2 {
        e.execute(&[stale_removal(&wt)], "test");
    }
    // Two in a row: still tried.
    let (run, held) = e.hold_back(vec![stale_removal(&wt)]);
    assert_eq!((run.len(), held.len()), (1, 0));
    e.execute(&[stale_removal(&wt)], "test");

    let journaled = e.snapshot_state().actions.len();
    let good = |p: &Path| Request::RemoveWorktree {
        path: p.to_path_buf(),
        expected_head: r.git(p, &["rev-parse", "HEAD"]).trim().to_string(),
        allow_dirty: false,
        require_idle: false,
    };
    // Even a request that would now succeed is not run for the held target;
    // the other one is.
    let out = e.execute_auto(vec![good(&wt), good(&other)], "optimizer");
    assert_eq!(out.len(), 2);
    let held = out
        .iter()
        .find(|o| o.target == wt.display().to_string())
        .unwrap();
    assert_eq!(held.reason(), Some(SkipReason::BackingOff), "{held:?}");
    assert!(wt.exists(), "held back, so untouched");
    assert!(!other.exists(), "the other request ran");
    assert_eq!(
        e.snapshot_state().actions.len(),
        journaled + 1,
        "only the removal that ran is journaled"
    );
    assert_eq!(
        e.snapshot_state().failures[0].count,
        3,
        "a held item is not a new failure"
    );

    // Once the wait is over it is tried again.
    {
        let mut a = e
            .snapshot_state()
            .actions
            .iter()
            .rev()
            .find(|a| a.kind == "worktree" && a.status.as_deref() == Some("skipped"))
            .cloned()
            .unwrap();
        let mut mem = lock(&e.failures);
        *mem = crate::failures::Memory::default();
        a.at_unix = now_unix() - 8 * 86_400;
        for _ in 0..3 {
            mem.note(&a);
        }
    }
    let (run, held) = e.hold_back(vec![good(&wt)]);
    assert_eq!((run.len(), held.len()), (1, 0));
    // An explicit execute was never held back.
    let out = e.execute(&[good(&wt)], "test");
    assert!(out[0].is_done(), "{out:?}");
}

/// A record whose target is gone from disk (removed by hand since) is not
/// reported as a problem any more.
#[test]
fn failures_of_targets_that_are_gone_are_not_reported() {
    let r = Repo::new();
    let e = r.engine();
    let gone = r.root.join("proj/target");
    for _ in 0..2 {
        let out = e.execute(
            &[Request::DeleteCache {
                path: gone.clone(),
                require_stale: false,
            }],
            "test",
        );
        assert!(out[0].reason().is_some());
    }
    assert!(lock(&e.failures)
        .get("reclaim", &gone.display().to_string(), now_unix())
        .is_some());
    assert!(e.snapshot_state().failures.is_empty());
}

/// Process actions are not remembered: a pid names a different process each
/// time.
#[test]
fn process_skips_are_not_remembered() {
    let r = Repo::new();
    let e = r.engine();
    for _ in 0..3 {
        let out = e.execute(
            &[Request::Signal {
                pid: 4_000_100,
                signal: Signal::Term,
                expected_start_time: 1,
            }],
            "test",
        );
        assert_eq!(out[0].reason(), Some(SkipReason::ProcessGone));
    }
    assert!(e.snapshot_state().failures.is_empty());
    assert!(lock(&e.failures).list(now_unix()).is_empty());
}
