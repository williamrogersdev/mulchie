//! Limit actions end to end: sample → breach → limits worker → safety gate.
//! The only real processes signalled are `sleep` children this test spawns;
//! the process table is a fake that gives them fixed start times.

use super::*;
use crate::config::{Budget, Cap, LimitsConfig};
use crate::consumers::ProcSample;
use crate::engine::tests::engine_with_procs;
use crate::process::FakeProcesses;

const WS: &str = "/code/a";
const M: Machine = Machine {
    cores: 8,
    mem_mb: 16_000.0,
    strained: false,
};

/// A child process killed when the test ends, pass or fail.
struct Child(std::process::Child);

impl Child {
    fn sleep() -> Self {
        Self(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        )
    }

    /// A shell that ignores SIGTERM.
    fn stubborn() -> Self {
        let c = Self(
            std::process::Command::new("/bin/sh")
                .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));
        c
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// `ps` state letter: `T` stopped, `S`/`R` running, empty when gone.
    fn state(&mut self) -> String {
        if self.0.try_wait().ok().flatten().is_some() {
            return String::new();
        }
        let o = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &self.pid().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .chars()
            .take(1)
            .collect()
    }

    fn stopped(&mut self) -> bool {
        self.state() == "T"
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn proc(pid: u32, ppid: u32, start: u64, agent: Option<&str>, cpu: f32) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: start,
        name: agent.unwrap_or("sleep").into(),
        own: true,
        agent: agent.map(String::from),
        cpu_pct: cpu,
        mem_bytes: 1_048_576,
        written: None,
        stopped: false,
        workspace: Some(PathBuf::from(WS)),
        cwd: Some(PathBuf::from(WS)),
        cmd: agent.unwrap_or("sleep 60").into(),
    }
}

fn cfg(action: LimitAction, include_agent: bool) -> Config {
    Config {
        limits: LimitsConfig {
            default: Some(Budget {
                cpu_cores: Some(Cap {
                    max: 1.0,
                    sustain_secs: 0,
                    action,
                }),
                resume_after_secs: 30,
                include_agent,
                stop_grace_secs: 2,
                ..Budget::default()
            }),
            ..LimitsConfig::default()
        },
        ..Config::default()
    }
}

/// An engine whose fake process table lists `procs` as (pid, start time).
fn engine(c: &Config, procs: &[(u32, u64)]) -> (tempfile::TempDir, Engine) {
    let mut f = FakeProcesses::empty();
    for (pid, st) in procs {
        f = f.with(*pid, "sleep", None, *st);
    }
    engine_with_procs(c.clone(), f)
}

fn only_breach(e: &Engine) -> Breach {
    let sup = lock(&e.sup);
    let v: Vec<&Breach> = sup.limits.breaches().collect();
    assert_eq!(v.len(), 1, "{v:?}");
    v[0].clone()
}

/// A pause stops the session's processes but not the agent, records them in
/// paused.json, and resumes them resume_after_secs later.
#[test]
fn pause_spares_the_agent_and_resumes_after_resume_after_secs() {
    let mut agent = Child::sleep();
    let mut work = Child::sleep();
    let (a, w) = (agent.pid(), work.pid());
    let c = cfg(LimitAction::Pause, false);
    let (_t, e) = engine(&c, &[(a, 500), (w, 1000)]);
    let busy = [
        proc(a, 1, 500, Some("claude"), 0.0),
        proc(w, a, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    assert_eq!(only_breach(&e).action_state, ActionState::Pending);
    e.run_limit_jobs(1_000);
    let b = only_breach(&e);
    assert_eq!(b.action_state, ActionState::Done, "{b:?}");
    assert!(work.stopped(), "the work process is paused");
    assert!(!agent.stopped(), "the agent is spared");
    let saved = load_paused(e.paths()).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!((saved[0].pid, saved[0].start_time), (w, 1000));
    assert_eq!(e.snapshot_state().sessions.paused_by_mulchie.len(), 1);

    let idle = [
        proc(a, 1, 500, Some("claude"), 0.0),
        proc(w, a, 1000, None, 0.0),
    ];
    e.observe_sessions(1_029, &idle, M, &c);
    e.run_limit_jobs(1_029);
    assert!(work.stopped(), "not before resume_after_secs");
    e.observe_sessions(1_030, &idle, M, &c);
    e.run_limit_jobs(1_030);
    assert!(!work.stopped(), "resumed");
    assert!(only_breach(&e).restored);
    assert!(load_paused(e.paths()).unwrap().is_empty());
    assert!(e.snapshot_state().sessions.paused_by_mulchie.is_empty());
}

/// include_agent lets a pause reach the agent's own process.
#[test]
fn include_agent_pauses_the_agent_too() {
    let mut agent = Child::sleep();
    let mut work = Child::sleep();
    let (a, w) = (agent.pid(), work.pid());
    let c = cfg(LimitAction::Pause, true);
    let (_t, e) = engine(&c, &[(a, 500), (w, 1000)]);
    let busy = [
        proc(a, 1, 500, Some("claude"), 0.0),
        proc(w, a, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_limit_jobs(1_000);
    assert!(work.stopped() && agent.stopped());
    e.shutdown(Duration::from_millis(10));
    assert!(!work.stopped() && !agent.stopped(), "shutdown resumes");
    assert!(load_paused(e.paths()).unwrap().is_empty());
}

/// The pid seen in the sample now belongs to a different process: nothing is
/// signalled and the breach says why.
#[test]
fn a_pid_reused_between_breach_and_action_is_not_signalled() {
    let mut work = Child::sleep();
    let w = work.pid();
    let c = cfg(LimitAction::Pause, false);
    // The sample saw start time 1000; the table now says 2000.
    let (_t, e) = engine(&c, &[(w, 2000)]);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(w, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_limit_jobs(1_000);
    let b = only_breach(&e);
    assert_eq!(b.action_state, ActionState::Skipped);
    assert!(
        b.action_detail
            .as_deref()
            .is_some_and(|d| d.contains("start_time_mismatch")),
        "{b:?}"
    );
    assert!(!work.stopped());
    assert!(load_paused(e.paths()).unwrap().is_empty());
}

/// Stop: SIGTERM, then SIGKILL to what still runs after stop_grace_secs.
#[test]
fn stop_terms_then_kills_after_the_grace_period() {
    let mut work = Child::stubborn();
    let w = work.pid();
    let c = cfg(LimitAction::Stop, false);
    let (_t, e) = engine(&c, &[(w, 1000)]);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(w, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_limit_jobs(1_000);
    assert_eq!(only_breach(&e).action_state, ActionState::Done);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!work.state().is_empty(), "SIGTERM is ignored");
    e.run_limit_jobs(1_001);
    assert!(!work.state().is_empty(), "not before the grace period");
    e.run_limit_jobs(1_002);
    let st = work.0.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), Some(libc::SIGKILL));
    let d = only_breach(&e).action_detail.unwrap_or_default();
    assert!(d.contains("SIGKILL to 1 of 1"), "{d}");
}

/// Deprioritize lowers the priority through the gate; clearing restores it
/// (or says it could not: raising priority needs privilege).
#[test]
fn deprioritize_lowers_priority_and_restores_it_after_clearing() {
    let work = Child::sleep();
    let w = work.pid();
    let c = cfg(LimitAction::Deprioritize, false);
    let (_t, e) = engine(&c, &[(w, 1000)]);
    let before = nice_of(w).unwrap();
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(w, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_limit_jobs(1_000);
    assert_eq!(only_breach(&e).action_state, ActionState::Done);
    assert_eq!(nice_of(w), Some(c.build_nice.max(10)));
    let idle = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(w, 77_777, 1000, None, 0.0),
    ];
    e.observe_sessions(1_003, &idle, M, &c);
    e.run_limit_jobs(1_003);
    let b = only_breach(&e);
    assert!(!b.active());
    let d = b.action_detail.unwrap_or_default();
    if nice_of(w) == Some(before) {
        assert!(b.restored, "{d}");
    } else {
        assert!(!b.restored && d.contains("needs privilege"), "{d}");
    }
}

/// paused.json left by an engine that crashed: the next engine that
/// automates resumes what is still the same process; one that does not
/// automate only reports it.
#[test]
fn processes_paused_before_a_restart_are_resumed_or_reported() {
    let mut work = Child::sleep();
    let w = work.pid();
    unsafe { libc::kill(w as i32, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(100));
    assert!(work.stopped());
    let c = Config::default();
    let entry = |st: u64| PausedProc {
        pid: w,
        start_time: st,
        session_id: "s-500-77777".into(),
        breach_id: "b-1".into(),
        since_unix: 900,
    };

    // Not automating: reported, left alone.
    let (t, e) = engine(&c, &[(w, 1000)]);
    save_paused(e.paths(), &[entry(1000)]).unwrap();
    e.set_automation_enabled(false);
    e.recover_paused();
    assert!(work.stopped());
    assert_eq!(e.snapshot_state().sessions.paused_left, vec![entry(1000)]);
    // A resume plan (by request) forgets it.
    let r = e.execute(
        &[Request::Signal {
            pid: w,
            signal: Signal::Cont,
            expected_start_time: 1000,
        }],
        "api",
    );
    assert!(r[0].is_done());
    e.run_limit_jobs(now_unix());
    assert!(e.snapshot_state().sessions.paused_left.is_empty());
    assert!(load_paused(e.paths()).unwrap().is_empty());
    drop(t);

    // Automating: resumed; an entry whose pid was reused is dropped unsignalled.
    unsafe { libc::kill(w as i32, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(100));
    let (_t, e) = engine(&c, &[(w, 1000)]);
    save_paused(e.paths(), &[entry(1000), entry(999)]).unwrap();
    e.recover_paused();
    assert!(!work.stopped(), "resumed on start");
    assert!(load_paused(e.paths()).unwrap().is_empty());
    assert!(e.snapshot_state().sessions.paused_left.is_empty());
}

/// A session a hook registered is never acted on: its breach is noted only.
#[test]
fn hook_registered_sessions_are_not_enforced() {
    let mut work = Child::sleep();
    let w = work.pid();
    let c = cfg(LimitAction::Pause, false);
    let (_t, e) = engine(&c, &[(w, 1000)]);
    lock(&e.sup).tracker.register(
        999,
        77_777,
        500,
        "codex",
        "codex".into(),
        Some("codex".into()),
        None,
        None,
        Some(PathBuf::from(WS)),
    );
    let busy = [
        proc(77_777, 1, 500, None, 0.0),
        proc(w, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_limit_jobs(1_000);
    let b = only_breach(&e);
    assert_eq!(b.action_state, ActionState::NotEnforced);
    assert!(!work.stopped());
}

/// notify: a finding (category limit) and a journal entry, nothing signalled.
/// Without budgets, a hog is reported while the machine is strained.
#[test]
fn notify_and_hogs_raise_limit_findings() {
    let c = cfg(LimitAction::Notify, false);
    let (_t, e) = engine(&c, &[]);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(77_778, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    assert_eq!(only_breach(&e).action_state, ActionState::NotifyOnly);
    assert!(lock(&e.sup).jobs.is_empty());
    let st = e.snapshot_state();
    let f: Vec<&Finding> = st
        .findings
        .iter()
        .filter(|f| f.kind == FindingKind::LimitExceeded)
        .collect();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].category, FindingCategory::Limit);
    assert_eq!(st.sessions.breaches_active, 1);
    assert_eq!(st.sessions.breaches_undelivered, 1);
    assert!(st.actions.iter().any(|a| a.kind == "limit"));
    assert_eq!(e.health_summary().breaches_active, 1);
    assert_eq!(e.health_summary().sessions_running, 1);

    // Report only: no budgets, strained machine, a session holding 3 of 4 cores.
    let c = Config::default();
    let (_t, e) = engine(&c, &[]);
    let strained = Machine {
        cores: 4,
        mem_mb: 16_000.0,
        strained: true,
    };
    let hog = c.limits.hog_sustain_secs;
    e.observe_sessions(1_000, &busy, strained, &c);
    e.observe_sessions(1_000 + hog, &busy, strained, &c);
    let st = e.snapshot_state();
    assert!(st.sessions.limits_report_only);
    assert!(st
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::SessionHog));
    assert!(st.sessions.breaches.is_empty());
    // Health recovers: the hog finding goes.
    e.observe_sessions(1_001 + hog, &busy, M, &c);
    assert!(!e
        .snapshot_state()
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::SessionHog));
}

/// A watch run keeps limit findings it did not produce.
#[test]
fn a_watch_run_keeps_limit_findings() {
    let c = cfg(LimitAction::Notify, false);
    let (_t, e) = engine(&c, &[]);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(77_778, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    e.run_watch();
    assert!(e
        .snapshot_state()
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::LimitExceeded));
}

/// The session target of a process plan: its processes from the last sample,
/// the agent only with include_agent.
#[test]
fn session_plans_cover_the_session_without_the_agent() {
    let c = Config::default();
    let (_t, e) = engine(&c, &[(77_777, 500), (77_778, 1000)]);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(77_778, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_000, &busy, M, &c);
    let sample = |pid: u32, ppid: u32, st: u64| ProcSample {
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
    };
    e.state_mut().snapshot = Arc::new(Snapshot {
        members: Arc::new(HashMap::from([(
            "workspace:a".to_string(),
            vec![sample(77_777, 1, 500), sample(77_778, 77_777, 1000)],
        )])),
        ..Snapshot::default()
    });
    let id = Session::id_for(77_777, 500);
    let req = |include_agent: bool| ProcessPlanRequest {
        session: Some(id.clone()),
        action: Some(crate::consumers::GroupAction::Pause),
        include_agent,
        ..ProcessPlanRequest::default()
    };
    let pids = |p: &ProcessPlan| -> Vec<u32> {
        let mut v: Vec<u32> = p
            .items
            .iter()
            .filter_map(|i| match i.request {
                Request::Signal { pid, .. } => Some(pid),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v
    };
    let p = e.process_plan(&req(false));
    assert_eq!(pids(&p), vec![77_778], "{p:?}");
    assert_eq!(p.target, id);
    let p = e.process_plan(&req(true));
    assert_eq!(pids(&p), vec![77_777, 77_778]);
    let p = e.process_plan(&ProcessPlanRequest {
        session: Some("s-1-2".into()),
        action: Some(crate::consumers::GroupAction::Pause),
        ..ProcessPlanRequest::default()
    });
    assert_eq!(p.refused.as_deref(), Some("unknown_session"));
}

/// Activity records: session start, breach and process events reach the
/// session's file, through the writer thread.
#[test]
fn sessions_and_breaches_are_recorded() {
    let c = cfg(LimitAction::Notify, false);
    let (_t, e) = engine(&c, &[]);
    let first = [proc(77_777, 1, 500, Some("claude"), 0.0)];
    e.observe_sessions(1_000, &first, M, &c);
    let busy = [
        proc(77_777, 1, 500, Some("claude"), 0.0),
        proc(77_778, 77_777, 1000, None, 300.0),
    ];
    e.observe_sessions(1_003, &busy, M, &c);
    e.observe_sessions(1_006, &[], M, &c);
    let id = Session::id_for(77_777, 500);
    let ev = e.session_events(&id, 0, 100).unwrap();
    let types: Vec<&str> = ev.iter().filter_map(|v| v["type"].as_str()).collect();
    assert!(types.contains(&"session_start"), "{types:?}");
    assert!(types.contains(&"limit_breach"), "{types:?}");
    assert!(types.contains(&"session_end"), "{types:?}");
    assert!(e.snapshot_state().sessions.record_error.is_none());
    // Not the lock holder: nothing is written.
    let (_t2, e2) = engine(&c, &[]);
    e2.set_automation_enabled(false);
    e2.observe_sessions(1_000, &first, M, &c);
    e2.flush_record();
    assert!(e2.session_events(&id, 0, 100).unwrap().is_empty());
}
