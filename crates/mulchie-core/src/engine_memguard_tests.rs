//! The memory guard end to end: sample → decision → limits worker → safety
//! gate. The only real processes signalled are children this test spawns;
//! the process table is a fake that gives them fixed start times.

use super::*;
use crate::config::Config;
use crate::consumers::Members;
use crate::engine::tests::engine_with_procs;
use crate::limits::Machine;
use crate::monitor::ProcKind;
use crate::process::FakeProcesses;
use crate::session::{Session, SessionProc};

const WS: &str = "/code/a";
const GB: u64 = 1 << 30;
const M: Machine = Machine {
    cores: 8,
    mem_mb: 16_384.0,
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

    /// Whether it exits within two seconds.
    fn exits(&mut self) -> bool {
        for _ in 0..40 {
            if self.0.try_wait().ok().flatten().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().ok().flatten().is_none()
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sp(pid: u32, ppid: u32, start: u64, agent: Option<&str>, mem: u64) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: start,
        name: agent.unwrap_or("sleep").into(),
        own: true,
        agent: agent.map(String::from),
        cpu_pct: 0.0,
        mem_bytes: mem,
        written: None,
        stopped: false,
        workspace: Some(PathBuf::from(WS)),
        cwd: Some(PathBuf::from(WS)),
        cmd: agent.unwrap_or("sleep 60").into(),
    }
}

fn ps(pid: u32, ppid: u32, start: u64, name: &str, mem: u64) -> ProcSample {
    ProcSample {
        pid,
        ppid,
        own: true,
        name: name.into(),
        exe: None,
        workspace: None,
        kind: ProcKind::Other,
        cpu_pct: 0.0,
        mem_bytes: mem,
        start_time: start,
        stopped: false,
    }
}

/// A sample with `free_pct` percent of 16 GB available.
fn snap(free_pct: u64, procs: &[SessionProc], groups: &[(&str, Vec<ProcSample>)]) -> Snapshot {
    let mut members = Members::new();
    for (k, v) in groups {
        members.insert(k.to_string(), v.clone());
    }
    Snapshot {
        at_unix: 1_000,
        cpu_count: 8,
        mem_total: 16 * GB,
        mem_available: 16 * GB * free_pct / 100,
        session_procs: procs.to_vec(),
        members: Arc::new(members),
        ..Snapshot::default()
    }
}

fn cfg(action: MemoryAction, scope: MemoryScope) -> Config {
    Config {
        memory_guard: MemoryGuardConfig {
            action,
            scope,
            sustain_secs: 0,
            stop_grace_secs: 2,
            ..MemoryGuardConfig::default()
        },
        ..Config::default()
    }
}

fn engine(c: &Config, procs: &[(u32, u64)]) -> (tempfile::TempDir, Engine) {
    let mut f = FakeProcesses::empty();
    for (pid, st) in procs {
        f = f.with(*pid, "sleep", None, *st);
    }
    engine_with_procs(c.clone(), f)
}

fn memory_findings(e: &Engine) -> Vec<Finding> {
    e.snapshot_state()
        .findings
        .into_iter()
        .filter(|f| f.kind == FindingKind::MemoryLow)
        .collect()
}

/// With `action: stop`, low memory closes what the biggest session started
/// and spares the agent; the closing is in state, the journal and the
/// session's record.
#[test]
fn low_memory_closes_what_the_session_started_and_spares_the_agent() {
    let mut agent = Child::sleep();
    let mut work = Child::sleep();
    let (a, w) = (agent.pid(), work.pid());
    let c = cfg(MemoryAction::Stop, MemoryScope::Sessions);
    let (_t, e) = engine(&c, &[(a, 500), (w, 1000)]);
    let procs = [
        sp(a, 1, 500, Some("claude"), GB),
        sp(w, a, 1000, None, 3 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    // Plenty of memory: nothing happens.
    e.observe_memory(1_000, &snap(40, &procs, &[]), &c);
    e.run_limit_jobs(1_000);
    assert!(work.running());
    assert!(memory_findings(&e).is_empty());

    e.observe_memory(1_003, &snap(3, &procs, &[]), &c);
    e.run_limit_jobs(1_003);
    assert!(work.exits(), "the work process is closed");
    assert!(agent.running(), "the agent is spared");
    let st = e.snapshot_state().memory_guard;
    assert!(st.enforcing && st.low);
    assert_eq!(st.closings.len(), 1);
    let cl = &st.closings[0];
    assert_eq!(cl.state, ActionState::Done, "{cl:?}");
    assert_eq!(cl.processes, 1);
    assert_eq!(
        cl.session_id.as_deref(),
        Some(Session::id_for(a, 500).as_str())
    );
    assert_eq!(cl.trigger, Trigger::MemoryLow);
    assert_eq!(e.limits_status().memory_guard.closings.len(), 1);
    assert!(e
        .snapshot_state()
        .actions
        .iter()
        .any(|x| x.source == "memory_guard"));
    e.flush_record();
    let events = e.session_events(&Session::id_for(a, 500), 0, 1000).unwrap();
    assert!(
        events
            .iter()
            .any(|v| v["type"] == "memory_guard" && v["outcome"] == "stopped"),
        "{events:?}"
    );
}

/// The default action only reports: a finding that names what would be
/// closed, and no signal.
#[test]
fn the_default_reports_and_signals_nothing() {
    let mut agent = Child::sleep();
    let mut work = Child::sleep();
    let (a, w) = (agent.pid(), work.pid());
    let c = Config {
        memory_guard: MemoryGuardConfig {
            sustain_secs: 0,
            ..MemoryGuardConfig::default()
        },
        ..Config::default()
    };
    let (_t, e) = engine(&c, &[(a, 500), (w, 1000)]);
    let procs = [
        sp(a, 1, 500, Some("claude"), GB),
        sp(w, a, 1000, None, 3 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    e.observe_memory(1_000, &snap(2, &procs, &[]), &c);
    e.run_limit_jobs(1_000);
    std::thread::sleep(Duration::from_millis(200));
    assert!(work.running() && agent.running());
    let st = e.snapshot_state().memory_guard;
    assert!(!st.enforcing && st.low);
    assert!(st.closings.is_empty());
    assert_eq!(st.would_close.as_ref().unwrap().processes, 1);
    let f = memory_findings(&e);
    assert_eq!(f.len(), 1);
    assert!(f[0].title.contains("would close"), "{}", f[0].title);
    assert!(f[0].why.contains("memory_guard.action"), "{}", f[0].why);
    // Memory back: the finding goes.
    e.observe_memory(1_003, &snap(40, &procs, &[]), &c);
    assert!(memory_findings(&e).is_empty());
}

/// A process that ignores SIGTERM is killed after `stop_grace_secs`.
#[test]
fn a_stubborn_process_is_killed_after_the_grace_period() {
    let agent = Child::sleep();
    let mut work = Child::stubborn();
    let (a, w) = (agent.pid(), work.pid());
    let c = cfg(MemoryAction::Stop, MemoryScope::Sessions);
    let (_t, e) = engine(&c, &[(a, 500), (w, 1000)]);
    let procs = [
        sp(a, 1, 500, Some("claude"), GB),
        sp(w, a, 1000, None, 3 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    e.observe_memory(1_000, &snap(3, &procs, &[]), &c);
    e.run_limit_jobs(1_000);
    e.run_limit_jobs(1_001);
    std::thread::sleep(Duration::from_millis(200));
    assert!(work.running(), "not before the grace period");
    e.run_limit_jobs(1_002);
    assert!(work.exits(), "killed");
    let cl = e.memory_guard_status().closings[0].clone();
    assert!(cl.detail.unwrap().contains("SIGKILL to 1 of 1"));
}

/// An engine that does not automate closes nothing and says so.
#[test]
fn an_engine_that_does_not_automate_only_reports() {
    let mut work = Child::sleep();
    let (a, w) = (77_001, work.pid());
    let c = cfg(MemoryAction::Stop, MemoryScope::Sessions);
    let (_t, e) = engine(&c, &[(w, 1000)]);
    e.set_automation_enabled(false);
    let procs = [
        sp(a, 1, 500, Some("claude"), GB),
        sp(w, a, 1000, None, 3 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    e.observe_memory(1_000, &snap(3, &procs, &[]), &c);
    e.run_limit_jobs(1_000);
    std::thread::sleep(Duration::from_millis(200));
    assert!(work.running());
    assert!(!e.memory_guard_status().enforcing);
    let f = memory_findings(&e);
    assert!(f[0].why.contains("not the one that acts"), "{}", f[0].why);
}

/// A pid reused since the sample (another start time) is not signalled.
#[test]
fn a_reused_pid_is_not_signalled() {
    let mut work = Child::sleep();
    let (a, w) = (77_002, work.pid());
    let c = cfg(MemoryAction::Stop, MemoryScope::Sessions);
    // The table says pid `w` started at 2000; the sample saw 1000.
    let (_t, e) = engine(&c, &[(w, 2000)]);
    let procs = [
        sp(a, 1, 500, Some("claude"), GB),
        sp(w, a, 1000, None, 3 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    e.observe_memory(1_000, &snap(3, &procs, &[]), &c);
    e.run_limit_jobs(1_000);
    std::thread::sleep(Duration::from_millis(200));
    assert!(work.running());
    let cl = e.memory_guard_status().closings[0].clone();
    assert_eq!(cl.state, ActionState::Skipped, "{cl:?}");
}

/// `scope: all` also closes a program outside every session.
#[test]
fn scope_all_closes_the_biggest_program_of_the_user() {
    let mut hog = Child::sleep();
    let h = hog.pid();
    let c = cfg(MemoryAction::Stop, MemoryScope::All);
    let (_t, e) = engine(&c, &[(h, 1000)]);
    let groups = [("process:sleep", vec![ps(h, 1, 1000, "sleep", 6 * GB)])];
    e.observe_memory(1_000, &snap(3, &[], &groups), &c);
    e.run_limit_jobs(1_000);
    assert!(hog.exits());
    let cl = e.memory_guard_status().closings[0].clone();
    assert_eq!(
        (cl.key.as_str(), cl.kind),
        ("process:sleep", UnitKind::Process)
    );
    // With the default scope the same sample closes nothing.
    let mut hog = Child::sleep();
    let h = hog.pid();
    let c = cfg(MemoryAction::Stop, MemoryScope::Sessions);
    let (_t, e) = engine(&c, &[(h, 1000)]);
    let groups = [("process:sleep", vec![ps(h, 1, 1000, "sleep", 6 * GB)])];
    e.observe_memory(1_000, &snap(3, &[], &groups), &c);
    e.run_limit_jobs(1_000);
    std::thread::sleep(Duration::from_millis(200));
    assert!(hog.running());
    assert!(memory_findings(&e)[0]
        .title
        .contains("nothing the memory guard may close"));
}

/// Which groups `scope: all` may pick: none that is protected, foreign,
/// named in `never`, hosts an agent session or started this engine; and a
/// session's processes only through the session's unit.
#[test]
fn units_leave_out_protected_groups_hosts_and_session_members() {
    let c = cfg(MemoryAction::Stop, MemoryScope::All);
    let (_t, e) = engine(&c, &[]);
    // Terminal (900) hosts the agent (901), which started a build (902).
    let procs = [
        sp(901, 900, 500, Some("claude"), GB),
        sp(902, 901, 600, None, 2 * GB),
    ];
    e.observe_sessions(1_000, &procs, M, &c);
    let mut foreign = ps(910, 1, 1, "postgres", 4 * GB);
    foreign.own = false;
    let mut system = ps(920, 1, 1, "mds", 4 * GB);
    system.exe = Some(PathBuf::from("/usr/libexec/mds"));
    let groups = [
        (
            "app:/Applications/Terminal.app",
            vec![ps(900, 1, 1, "Terminal", GB)],
        ),
        (
            "workspace:/code/a",
            vec![
                ps(901, 900, 500, "claude", GB),
                ps(902, 901, 600, "cargo", 2 * GB),
                ps(903, 1, 700, "node", GB),
            ],
        ),
        ("process:postgres", vec![foreign]),
        ("process:mds", vec![system]),
        ("process:keepme", vec![ps(930, 1, 1, "keepme", 4 * GB)]),
        (
            "app:/Applications/Browser.app",
            vec![ps(940, 1, 1, "Browser", 5 * GB)],
        ),
        ("system", vec![ps(950, 1, 1, "kernel_task", 9 * GB)]),
        // The engine's own parent (the fake engine has pid u32::MAX).
        ("process:launcher", vec![ps(960, 1, 1, "launcher", 4 * GB)]),
        ("process:mulchie", vec![ps(u32::MAX, 960, 1, "mulchie", GB)]),
    ];
    let s = snap(3, &procs, &groups);
    let mut g = c.memory_guard.clone();
    g.never = vec!["keepme".into()];
    let sup = lock(&e.sup);
    let units = build_units(&sup.tracker, &s, &g, true, u32::MAX);
    let mut keys: Vec<(&str, Vec<u32>)> = units
        .iter()
        .map(|u| (u.key.as_str(), u.procs.iter().map(|p| p.0).collect()))
        .collect();
    keys.sort();
    let session = format!("session:{}", Session::id_for(901, 500));
    assert_eq!(
        keys,
        vec![
            ("app:/Applications/Browser.app", vec![940]),
            (session.as_str(), vec![902]),
            // Only the leftover outside the session.
            ("workspace:/code/a", vec![903]),
        ]
    );
    // `include_agent` adds the agent to its session's unit.
    g.include_agent = true;
    let units = build_units(&sup.tracker, &s, &g, true, u32::MAX);
    let su = units.iter().find(|u| u.key == session).unwrap();
    assert_eq!(su.procs.len(), 2);
    assert_eq!(su.mem_mb, 3.0 * 1024.0);
}
