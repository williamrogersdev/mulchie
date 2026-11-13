//! Runaway chains end to end: sample → decision → limits worker → safety
//! gate. The only real processes signalled are children this test spawns;
//! the process table is a fake that nests them and gives them fixed start
//! times.

use super::*;
use crate::config::{Config, RuleMode};
use crate::engine::tests::engine_with_procs;
use crate::guard::{Decision as GuardDecision, GuardRule};
use crate::limits::Machine;
use crate::process::{FakeProcesses, ProcFacts};
use crate::session::{Session, SessionProc};

const SITE: &str = "/code/site";
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

fn sp(pid: u32, ppid: u32, start: u64, cmd: &str, agent: bool) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: start,
        name: if agent { "claude" } else { "node" }.into(),
        own: true,
        agent: agent.then(|| "claude".to_string()),
        cpu_pct: 0.0,
        mem_bytes: 300 << 20,
        written: None,
        stopped: false,
        workspace: Some(PathBuf::from("/code")),
        cwd: Some(PathBuf::from(SITE)),
        cmd: cmd.into(),
    }
}

fn facts(pid: u32, ppid: u32, start: u64) -> ProcFacts {
    ProcFacts {
        pid,
        ppid,
        uid: Some(unsafe { libc::getuid() }),
        name: "sleep".into(),
        exe: None,
        cmd: vec!["sleep".into()],
        cwd: None,
        start_time: start,
    }
}

/// Six real children presented as six nested `npm run build`, under an agent.
struct Scene {
    agent: Child,
    levels: Vec<Child>,
    procs: Vec<SessionProc>,
    table: Vec<ProcFacts>,
}

fn scene() -> Scene {
    let agent = Child::sleep();
    let levels: Vec<Child> = (0..6).map(|_| Child::sleep()).collect();
    let a = agent.pid();
    let mut procs = vec![sp(a, 1, 500, "claude", true)];
    let mut table = vec![facts(a, 1, 500)];
    let mut parent = a;
    for (i, l) in levels.iter().enumerate() {
        let start = 1_000 + i as u64;
        procs.push(sp(l.pid(), parent, start, "npm run build", false));
        table.push(facts(l.pid(), parent, start));
        parent = l.pid();
    }
    Scene {
        agent,
        levels,
        procs,
        table,
    }
}

fn engine(c: &Config, table: Vec<ProcFacts>) -> (tempfile::TempDir, Engine) {
    let mut f = FakeProcesses::empty();
    f.procs = table;
    engine_with_procs(c.clone(), f)
}

fn snap(procs: &[SessionProc]) -> Snapshot {
    Snapshot {
        at_unix: 2_000,
        cpu_count: 8,
        mem_total: 16 << 30,
        mem_available: 8 << 30,
        session_procs: procs.to_vec(),
        ..Snapshot::default()
    }
}

fn cfg(action: MemoryAction) -> Config {
    let mut c = Config::default();
    c.runaway.action = action;
    c.memory_guard.action = MemoryAction::Off;
    c
}

fn chain_findings(e: &Engine) -> Vec<Finding> {
    e.snapshot_state()
        .findings
        .into_iter()
        .filter(|f| f.kind == FindingKind::RunawayChain)
        .collect()
}

fn check(e: &Engine, command: &str, cwd: &str) -> crate::engine::GuardCheckReply {
    e.guard_check(&crate::engine::GuardCheckRequest {
        command: command.into(),
        cwd: PathBuf::from(cwd),
    })
}

/// With `action: stop` the whole chain is killed, including a level that
/// started after the sample; the agent and unrelated work are spared. The
/// stopping is in state, the journal and the session's record, and the
/// session hears of it once.
#[test]
fn stop_kills_the_whole_chain_and_what_it_started_since_the_sample() {
    let mut s = scene();
    let mut late = Child::sleep();
    let mut other = Child::sleep();
    let a = s.agent.pid();
    // In the process table, not in the sample: started a moment ago.
    let deepest = s.levels.last().unwrap().pid();
    s.table.push(facts(late.pid(), deepest, 1_900));
    s.table.push(facts(other.pid(), a, 900));
    s.procs.push(sp(other.pid(), a, 900, "cargo build", false));
    let c = cfg(MemoryAction::Stop);
    let (_t, e) = engine(&c, s.table.clone());
    e.observe_sessions(2_000, &s.procs, M, &c);
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    assert_eq!(chain_findings(&e).len(), 1);
    e.run_limit_jobs(2_000);
    for l in &mut s.levels {
        assert!(l.exits(), "every level is killed");
    }
    assert!(
        late.exits(),
        "a level started after the sample is killed too"
    );
    assert!(s.agent.running(), "the agent is spared");
    assert!(other.running(), "unrelated work is spared");

    let st = e.limits_status().runaway;
    assert!(st.enforcing);
    assert_eq!(st.stoppings.len(), 1);
    let stop = &st.stoppings[0];
    assert_eq!(stop.state, ActionState::Done, "{stop:?}");
    assert_eq!(stop.command, "npm run build");
    assert_eq!(stop.depth, 6);
    let sid = Session::id_for(a, 500);
    assert_eq!(stop.session_id.as_deref(), Some(sid.as_str()));
    assert!(
        stop.detail.as_deref().unwrap().contains("killed 7 of 7"),
        "{stop:?}"
    );
    assert_eq!(e.snapshot_state().runaway.stoppings.len(), 1);
    assert!(e
        .snapshot_state()
        .actions
        .iter()
        .any(|x| x.source == "runaway"));
    e.flush_record();
    let events = e.session_events(&sid, 0, 1000).unwrap();
    assert!(
        events
            .iter()
            .any(|v| v["type"] == "runaway" && v["outcome"] == "stopped"),
        "{events:?}"
    );
    // The session is told at its next hook call, once.
    let notes = lock(&e.sup).runaway.take_notes(&sid);
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains("Mulchie stopped `npm run build`"),
        "{notes:?}"
    );

    // The chain is gone from the next sample: so is the finding.
    e.observe_runaway(2_003, &snap(&s.procs[..1]), &c);
    assert!(chain_findings(&e).is_empty());
    assert!(e.runaway_status().chains.is_empty());
}

/// The default action only reports: a finding that says how to stop the
/// chain, and no signal.
#[test]
fn the_default_reports_and_signals_nothing() {
    let mut s = scene();
    let c = Config {
        memory_guard: crate::config::MemoryGuardConfig {
            action: MemoryAction::Off,
            ..Default::default()
        },
        ..Config::default()
    };
    assert_eq!(c.runaway.action, MemoryAction::Notify);
    let (_t, e) = engine(&c, s.table.clone());
    e.observe_sessions(2_000, &s.procs, M, &c);
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    e.run_limit_jobs(2_000);
    std::thread::sleep(Duration::from_millis(200));
    for l in &mut s.levels {
        assert!(l.running());
    }
    let st = e.runaway_status();
    assert!(!st.enforcing && st.stoppings.is_empty());
    assert_eq!(st.chains.len(), 1);
    assert_eq!((st.chains[0].depth, st.chains[0].processes), (6, 6));
    let f = chain_findings(&e);
    assert_eq!(f.len(), 1);
    assert!(f[0].title.contains("6 copies of itself"), "{}", f[0].title);
    assert!(f[0].why.contains("runaway.action stop"), "{}", f[0].why);
    let root = s.levels[0].pid();
    assert!(
        f[0].why
            .contains(&format!("mulchie signal {root} kill --tree")),
        "{}",
        f[0].why
    );
    assert_eq!(f[0].evidence.pid, Some(root));
}

/// A command that ran away is remembered: the guard rules warn (or block)
/// before it is run in that directory again, and say nothing elsewhere.
#[test]
fn the_guard_rule_matches_the_command_that_ran_away() {
    let s = scene();
    let mut c = cfg(MemoryAction::Notify);
    let (_t, e) = engine(&c, s.table.clone());
    assert_eq!(
        check(&e, "npm run build", SITE).verdict.decision,
        GuardDecision::Allow
    );
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    let v = check(&e, "npm run build", SITE).verdict;
    assert_eq!(v.decision, GuardDecision::Warn);
    assert_eq!(v.rule, Some(GuardRule::RunawayRepeat));
    assert!(
        v.reason.as_deref().unwrap().contains("npm run build"),
        "{v:?}"
    );
    // Reached through `cd`, and inside `sh -c`.
    let v = check(&e, "cd site && npm run build 2>&1 | tail -5", "/code").verdict;
    assert_eq!(v.rule, Some(GuardRule::RunawayRepeat));
    let v = check(&e, "sh -c 'npm run build'", SITE).verdict;
    assert_eq!(v.rule, Some(GuardRule::RunawayRepeat));
    // Another directory, another script.
    assert_eq!(
        check(&e, "npm run build", "/code").verdict.decision,
        GuardDecision::Allow
    );
    assert_eq!(
        check(&e, "npm run test", SITE).verdict.decision,
        GuardDecision::Allow
    );
    // The rule's mode decides.
    c.guard.rules.runaway_repeat = RuleMode::Block;
    e.set_config(c.clone()).unwrap();
    assert_eq!(
        check(&e, "npm run build", SITE).verdict.decision,
        GuardDecision::Block
    );
    c.guard.rules.runaway_repeat = RuleMode::Off;
    e.set_config(c.clone()).unwrap();
    assert_eq!(
        check(&e, "npm run build", SITE).verdict.decision,
        GuardDecision::Allow
    );
    assert_eq!(e.runaway_status().remembered.len(), 1);
}

/// An engine that does not automate stops nothing and says so.
#[test]
fn an_engine_that_does_not_automate_only_reports() {
    let mut s = scene();
    let c = cfg(MemoryAction::Stop);
    let (_t, e) = engine(&c, s.table.clone());
    e.set_automation_enabled(false);
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    e.run_limit_jobs(2_000);
    std::thread::sleep(Duration::from_millis(200));
    for l in &mut s.levels {
        assert!(l.running());
    }
    assert!(!e.runaway_status().enforcing);
    let f = chain_findings(&e);
    assert!(f[0].why.contains("not the one that acts"), "{}", f[0].why);
}

/// A chain whose outermost pid is another process by now (another start
/// time) is left alone.
#[test]
fn a_reused_pid_is_not_signalled() {
    let mut s = scene();
    let c = cfg(MemoryAction::Stop);
    // The table says the outermost copy started at 5000; the sample saw 1000.
    s.table[1].start_time = 5_000;
    let (_t, e) = engine(&c, s.table.clone());
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    e.run_limit_jobs(2_000);
    std::thread::sleep(Duration::from_millis(200));
    for l in &mut s.levels {
        assert!(l.running());
    }
    let stop = e.runaway_status().stoppings[0].clone();
    assert_eq!(stop.state, ActionState::Skipped, "{stop:?}");
}

/// `action: off` looks at nothing.
#[test]
fn off_reports_nothing() {
    let s = scene();
    let c = cfg(MemoryAction::Off);
    let (_t, e) = engine(&c, s.table.clone());
    e.observe_runaway(2_000, &snap(&s.procs), &c);
    assert!(chain_findings(&e).is_empty());
    assert!(e.runaway_status().chains.is_empty());
    assert_eq!(
        check(&e, "npm run build", SITE).verdict.decision,
        GuardDecision::Allow
    );
}
