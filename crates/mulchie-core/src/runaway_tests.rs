//! One test per rule of runaway detection and its decision.

use super::*;

const SITE: &str = "/code/site";

fn p(pid: u32, ppid: u32, start: u64, cmd: &str, cwd: &str) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: start,
        name: cmd.split(' ').next().unwrap_or(cmd).into(),
        own: true,
        agent: None,
        cpu_pct: 0.0,
        mem_bytes: 100 << 20,
        written: None,
        stopped: false,
        workspace: Some(PathBuf::from("/code")),
        cwd: Some(PathBuf::from(cwd)),
        cmd: cmd.into(),
    }
}

/// `levels` levels of `npm run build` → `sh -c …` → `node react-router build`
/// under a shell (pid 10) the agent (pid 5) started. Pids from 100.
fn build_chain(levels: u32) -> Vec<SessionProc> {
    let mut v = vec![
        p(5, 1, 10, "claude", "/code"),
        p(10, 5, 20, "zsh -c npm run build", SITE),
    ];
    let mut parent = 10;
    for l in 0..levels {
        let (a, b, c) = (100 + l * 3, 101 + l * 3, 102 + l * 3);
        let t = 30 + l as u64;
        v.push(p(a, parent, t, "npm run build", SITE));
        v.push(p(
            b,
            a,
            t,
            "sh -c react-router build && node scripts/postbuild.mjs",
            SITE,
        ));
        v.push(p(
            c,
            b,
            t,
            "node node_modules/.bin/react-router build",
            SITE,
        ));
        parent = c;
    }
    v
}

fn stop_cfg() -> RunawayConfig {
    RunawayConfig {
        action: MemoryAction::Stop,
        ..RunawayConfig::default()
    }
}

#[test]
fn a_command_nested_in_copies_of_itself_is_one_chain_from_its_outermost_copy() {
    let mut procs = build_chain(8);
    // Unrelated work beside it.
    procs.push(p(900, 5, 25, "cargo build", "/code"));
    procs.push(p(901, 900, 26, "rustc --crate-name x", "/code"));
    let chains = detect(&procs, 6);
    assert_eq!(chains.len(), 1, "{chains:?}");
    let c = &chains[0];
    assert_eq!(c.root_pid, 100);
    assert_eq!(c.key, "chain:100:30");
    assert_eq!(c.command, "npm run build");
    assert_eq!(c.cwd.as_deref(), Some(Path::new(SITE)));
    assert_eq!(c.depth, 8);
    // Every level, not the shell that started it, the agent or the cargo build.
    assert_eq!(c.processes, 24);
    assert!(!c.procs.iter().any(|x| [5, 10, 900, 901].contains(&x.0)));
    assert!((c.mem_mb - 2400.0).abs() < 1.0);
    // The commands that nest, as the guard rule sees them.
    assert_eq!(c.phrases[0].words(), "npm run build");
    assert!(c
        .phrases
        .iter()
        .any(|ph| ph.words() == "react-router build"));
}

#[test]
fn fewer_copies_than_depth_is_not_a_chain() {
    assert!(detect(&build_chain(5), 6).is_empty());
    assert_eq!(detect(&build_chain(6), 6).len(), 1);
}

#[test]
fn the_same_command_in_other_directories_is_not_a_chain() {
    // A monorepo: `npm run build` in each package starts the next.
    let mut procs = vec![p(5, 1, 10, "claude", "/code")];
    for l in 0..10u32 {
        procs.push(p(
            100 + l,
            if l == 0 { 5 } else { 99 + l },
            20 + l as u64,
            "npm run build",
            &format!("/code/packages/p{l}"),
        ));
    }
    assert!(detect(&procs, 6).is_empty());
}

#[test]
fn nested_login_shells_are_not_a_chain() {
    let procs: Vec<SessionProc> = (0..10u32)
        .map(|l| {
            p(
                100 + l,
                if l == 0 { 1 } else { 99 + l },
                20 + l as u64,
                "-zsh",
                "/code",
            )
        })
        .collect();
    assert!(detect(&procs, 6).is_empty());
}

#[test]
fn a_parent_that_started_after_its_child_is_a_reused_pid() {
    let mut procs = build_chain(8);
    // The fourth level's `npm` (pid 109) is given a start time after its
    // children: whatever has that pid now is not their parent.
    procs.iter_mut().find(|x| x.pid == 109).unwrap().start_time = 10_000;
    let chains = detect(&procs, 6);
    assert!(
        chains.iter().all(|c| c.depth < 6 || c.root_pid != 100),
        "{chains:?}"
    );
    assert!(
        chains.is_empty(),
        "three levels above and four below: {chains:?}"
    );
}

#[test]
fn a_phrase_is_the_program_and_its_arguments() {
    let w = |s: &str| Phrase::of(s).map(|p| p.words());
    assert_eq!(w("npm run build").as_deref(), Some("npm run build"));
    assert_eq!(
        w("node /opt/homebrew/bin/npm run build").as_deref(),
        Some("npm run build")
    );
    assert_eq!(
        w("node ~/.nvm/lib/node_modules/npm/bin/npm-cli.js run build").as_deref(),
        Some("npm run build")
    );
    // A runtime's own subcommand stays.
    assert_eq!(w("bun run build").as_deref(), Some("bun run build"));
    assert_eq!(w("/usr/bin/make -j8 all").as_deref(), Some("make -j8 all"));
    assert_eq!(w(""), None);
}

#[test]
fn the_default_reports_once_and_stops_nothing() {
    let c = RunawayConfig::default();
    assert_eq!(c.action, MemoryAction::Notify);
    let mut r = Runaway::default();
    let d = r.evaluate(100, detect(&build_chain(8), c.depth), &c, true);
    assert!(d.stop.is_empty());
    assert_eq!(d.fresh.len(), 1);
    let d = r.evaluate(103, detect(&build_chain(9), c.depth), &c, true);
    assert!(d.stop.is_empty() && d.fresh.is_empty());
    assert_eq!(r.chains().len(), 1);
    assert_eq!(r.chains()[0].first_seen_unix, 100);
    assert_eq!(r.chains()[0].depth, 9);
    assert_eq!(r.stoppings().count(), 0);
}

#[test]
fn stop_queues_one_stopping_and_tries_again_only_after_a_while() {
    let c = stop_cfg();
    let mut r = Runaway::default();
    let d = r.evaluate(100, detect(&build_chain(8), c.depth), &c, true);
    assert_eq!(d.stop.len(), 1);
    let s = r.get(&d.stop[0]).unwrap().clone();
    assert_eq!(s.state, ActionState::Pending);
    assert_eq!(s.root, (100, 30));
    assert_eq!((s.depth, s.processes), (8, 24));
    // Still there three seconds later: not queued twice.
    assert!(r
        .evaluate(103, detect(&build_chain(8), c.depth), &c, true)
        .stop
        .is_empty());
    // Still there after the retry time: once more.
    assert_eq!(
        r.evaluate(100 + RETRY_SECS, detect(&build_chain(8), c.depth), &c, true)
            .stop
            .len(),
        1
    );
    assert_eq!(r.stoppings().count(), 2);
}

#[test]
fn an_engine_that_may_not_act_stops_nothing() {
    let c = stop_cfg();
    let mut r = Runaway::default();
    let d = r.evaluate(100, detect(&build_chain(8), c.depth), &c, false);
    assert!(d.stop.is_empty());
    assert_eq!(d.fresh.len(), 1);
}

#[test]
fn off_looks_at_nothing() {
    let c = RunawayConfig {
        action: MemoryAction::Off,
        ..RunawayConfig::default()
    };
    let mut r = Runaway::default();
    let d = r.evaluate(100, detect(&build_chain(8), 6), &c, true);
    assert_eq!(d, Decision::default());
    assert!(r.chains().is_empty() && r.memos().is_empty());
}

#[test]
fn a_chain_is_remembered_for_the_guard_rule_until_it_expires() {
    let c = RunawayConfig {
        remember_secs: 600,
        ..RunawayConfig::default()
    };
    let mut r = Runaway::default();
    r.evaluate(100, detect(&build_chain(8), c.depth), &c, true);
    let site = Path::new(SITE);
    let m = repeat_of(r.memos(), "npm", &["run", "build"], site).expect("remembered");
    assert_eq!(m.command, "npm run build");
    assert_eq!((m.depth, m.at_unix, m.until_unix), (8, 100, 700));
    // Another directory, other arguments, another program: no match.
    assert!(repeat_of(r.memos(), "npm", &["run", "build"], Path::new("/code")).is_none());
    assert!(repeat_of(r.memos(), "npm", &["run", "test"], site).is_none());
    assert!(repeat_of(r.memos(), "npm", &["run"], site).is_none());
    assert!(repeat_of(r.memos(), "pnpm", &["run", "build"], site).is_none());
    // The chain is gone; the memo lasts until its time is up.
    r.evaluate(400, Vec::new(), &c, true);
    assert!(repeat_of(r.memos(), "npm", &["run", "build"], site).is_some());
    r.evaluate(700, Vec::new(), &c, true);
    assert!(r.memos().is_empty());
    // remember_secs 0 remembers nothing.
    let none = RunawayConfig {
        remember_secs: 0,
        ..RunawayConfig::default()
    };
    let mut r = Runaway::default();
    r.evaluate(100, detect(&build_chain(8), none.depth), &none, true);
    assert!(r.memos().is_empty());
}

#[test]
fn a_stopped_chain_is_told_to_its_session_once() {
    let c = stop_cfg();
    let mut r = Runaway::default();
    let mut chains = detect(&build_chain(8), c.depth);
    chains[0].session_id = Some("s-10-5".into());
    let id = r.evaluate(100, chains, &c, true).stop.remove(0);
    assert!(
        r.take_notes("s-10-5").is_empty(),
        "nothing before the gate ran"
    );
    r.set_result(&id, ActionState::Done, "froze 24 and killed 24".into());
    let notes = r.take_notes("s-10-5");
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].contains("npm run build") && notes[0].contains("8 copies"),
        "{notes:?}"
    );
    assert!(r.take_notes("s-10-5").is_empty());
    // A stopping of which nothing was stopped says nothing.
    let mut r = Runaway::default();
    let mut chains = detect(&build_chain(8), c.depth);
    chains[0].session_id = Some("s-10-5".into());
    let id = r.evaluate(100, chains, &c, true).stop.remove(0);
    r.set_result(
        &id,
        ActionState::Skipped,
        "the chain had already ended".into(),
    );
    assert!(r.take_notes("s-10-5").is_empty());
}

fn facts(pid: u32, ppid: u32, start: u64) -> ProcFacts {
    ProcFacts {
        pid,
        ppid,
        uid: Some(501),
        name: "node".into(),
        exe: None,
        cmd: vec!["node".into()],
        cwd: None,
        start_time: start,
    }
}

#[test]
fn the_tree_is_the_root_and_what_is_under_it_now() {
    let list = vec![
        facts(5, 1, 10),
        facts(100, 5, 30),
        facts(101, 100, 31),
        facts(102, 101, 32),
        facts(103, 101, 33),
        // A pid under 101 that started before it: reused, not its child.
        facts(104, 101, 5),
        facts(200, 5, 40),
    ];
    let mut t = tree(&list, (100, 30));
    t.sort();
    assert_eq!(t, vec![(100, 30), (101, 31), (102, 32), (103, 33)]);
    // The root is gone, or its pid is another process now.
    assert!(tree(&list, (999, 30)).is_empty());
    assert!(tree(&list, (100, 29)).is_empty());
}
