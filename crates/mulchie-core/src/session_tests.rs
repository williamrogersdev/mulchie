use super::*;

fn p(pid: u32, ppid: u32, name: &str, agent: Option<&str>) -> SessionProc {
    SessionProc {
        pid,
        ppid,
        start_time: 1000 + pid as u64,
        name: name.into(),
        own: true,
        agent: agent.map(String::from),
        cpu_pct: 50.0,
        mem_bytes: 1_048_576,
        written: Some(1_000),
        stopped: false,
        workspace: Some(PathBuf::from("/code/a")),
        cwd: Some(PathBuf::from("/code/a/src")),
        cmd: name.into(),
    }
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn agents_are_recognised_by_name_exe_script_and_package() {
    let none: Vec<String> = Vec::new();
    assert_eq!(
        agent_of("claude", None, &args(&["claude"]), &none).as_deref(),
        Some("claude")
    );
    assert_eq!(
        agent_of(
            "node",
            Some(Path::new("/opt/homebrew/bin/node")),
            &args(&[
                "node",
                "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
                "exec"
            ]),
            &none
        )
        .as_deref(),
        Some("codex")
    );
    assert_eq!(
        agent_of(
            "node",
            None,
            &args(&["node", "/x/node_modules/@anthropic-ai/claude-code/cli.js"]),
            &none
        )
        .as_deref(),
        Some("claude")
    );
    assert_eq!(
        agent_of("gemini", Some(Path::new("/usr/bin/gemini")), &[], &none).as_deref(),
        Some("gemini")
    );
    // Not agents: the runtime alone, a lookalike, Antigravity's launcher.
    assert_eq!(
        agent_of("node", None, &args(&["node", "server.js"]), &none),
        None
    );
    assert_eq!(
        agent_of("claudette", None, &args(&["claudette"]), &none),
        None
    );
    assert_eq!(agent_of("agy", None, &args(&["agy"]), &none), None);
    // agents.extra: exact names only.
    let extra = vec!["myagent".to_string()];
    assert_eq!(
        agent_of("myagent", None, &args(&["/opt/bin/myagent"]), &extra).as_deref(),
        Some("myagent")
    );
    assert_eq!(agent_of("myagent2", None, &[], &extra), None);
}

#[test]
fn ids_round_trip_and_bad_ids_are_refused() {
    let id = Session::id_for(42, 1700000000);
    assert_eq!(id, "s-1700000000-42");
    assert_eq!(Session::parse_id(&id), Some((42, 1700000000)));
    for bad in [
        "",
        "s-",
        "s-1-",
        "s--1",
        "x-1-2",
        "s-1-2-3",
        "s-a-1",
        "s-{id}",
        "s-1-99999999999",
    ] {
        assert_eq!(Session::parse_id(bad), None, "{bad}");
    }
}

/// Nested agents are their own sessions; nothing is counted twice; the
/// shell and build under an agent belong to it.
#[test]
fn attribution_and_nesting_without_double_counting() {
    let mut t = Tracker::default();
    let procs = vec![
        p(10, 1, "zsh", None),
        p(11, 10, "claude", Some("claude")),
        p(12, 11, "zsh", None),
        p(13, 12, "cargo", None),
        p(14, 13, "rustc", None),
        // claude ran `codex exec`: a nested session with its own child.
        p(20, 11, "codex", Some("codex")),
        p(21, 20, "node", None),
        // Unrelated.
        p(30, 1, "vim", None),
    ];
    let ev = t.observe(2000, &procs, &[13], 86_400);
    let started: Vec<&SessionEvent> = ev
        .iter()
        .filter(|e| matches!(e, SessionEvent::Started { .. }))
        .collect();
    assert_eq!(started.len(), 2, "{ev:?}");
    let claude = t.get(&Session::id_for(11, 1011)).unwrap();
    let codex = t.get(&Session::id_for(20, 1020)).unwrap();
    assert_eq!(claude.usage.processes, 4, "claude, zsh, cargo, rustc");
    assert_eq!(codex.usage.processes, 2);
    assert_eq!(codex.parent.as_deref(), Some(claude.id.as_str()));
    assert_eq!(claude.parent, None);
    assert!((claude.usage.cpu_cores - 2.0).abs() < 1e-9);
    assert!((claude.usage.memory_mb - 4.0).abs() < 1e-9);
    assert_eq!(claude.usage.build_slots, 1);
    assert_eq!(claude.usage.runtime_secs, 2000 - 1011);
    assert_eq!(claude.workspace.as_deref(), Some(Path::new("/code/a")));
    // Every own process in a session is in exactly one.
    let total: u32 = t.sessions().map(|s| s.usage.processes).sum();
    assert_eq!(total, 6);
    assert_eq!(t.session_of(14, 1014), Some(claude.id.as_str()));
    assert_eq!(t.session_of(14, 999), None, "start time must match");
    assert_eq!(t.session_of(30, 1030), None);
    assert_eq!(
        t.session_of_ancestry(21).as_deref(),
        Some(codex.id.as_str())
    );
}

/// A wrapper of the same tool (`node …/codex` starting the native `codex`)
/// is one session; another user's agent is no session.
#[test]
fn same_tool_wrappers_merge_and_foreign_agents_are_ignored() {
    let mut t = Tracker::default();
    let mut foreign = p(40, 1, "claude", Some("claude"));
    foreign.own = false;
    let procs = vec![
        p(10, 1, "node", Some("codex")),
        p(11, 10, "codex", Some("codex")),
        p(12, 11, "bash", None),
        foreign,
    ];
    t.observe(2000, &procs, &[], 86_400);
    let ids: Vec<&str> = t.sessions().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![Session::id_for(10, 1010).as_str()]);
    assert_eq!(t.sessions().next().unwrap().usage.processes, 3);
}

/// Process starts after the first sample are events; peaks are kept; a
/// session ends when its root exits and stays listed for the retention.
#[test]
fn lifecycle_peaks_and_retention() {
    let mut t = Tracker::default();
    let mut procs = vec![p(11, 1, "claude", Some("claude"))];
    t.observe(2000, &procs, &[], 100);
    procs.push(p(12, 11, "cargo", None));
    procs[1].cpu_pct = 400.0;
    let ev = t.observe(2003, &procs, &[], 100);
    assert!(
        ev.iter()
            .any(|e| matches!(e, SessionEvent::ProcessStarted { pid: 12, .. })),
        "{ev:?}"
    );
    procs.pop();
    t.observe(2006, &procs, &[], 100);
    let id = Session::id_for(11, 1011);
    let s = t.get(&id).unwrap();
    assert_eq!(s.usage.processes, 1);
    assert_eq!(s.peak.processes, 2);
    assert!((s.peak.cpu_cores - 4.5).abs() < 1e-9);
    let ev = t.observe(2009, &[], &[], 100);
    assert_eq!(ev, vec![SessionEvent::Ended { id: id.clone() }]);
    let s = t.get(&id).unwrap();
    assert_eq!(s.status, SessionStatus::Ended);
    assert_eq!(s.ended_unix, Some(2009));
    assert_eq!(s.usage.processes, 0);
    t.observe(2100, &[], &[], 100);
    assert!(t.get(&id).is_some(), "kept within the retention");
    t.observe(2200, &[], &[], 100);
    assert!(t.get(&id).is_none(), "forgotten after it");
}

/// Bytes written count members that exited; a platform without the
/// figure reports none, never zero.
#[test]
fn bytes_written_and_rate() {
    let mut t = Tracker::default();
    let mut procs = vec![p(11, 1, "claude", Some("claude")), p(12, 11, "cargo", None)];
    procs[1].written = Some(10 * 1_048_576);
    t.observe(2000, &procs, &[], 100);
    procs[1].written = Some(20 * 1_048_576);
    t.observe(2030, &procs, &[], 100);
    procs[1].written = Some(30 * 1_048_576);
    t.observe(2060, &procs, &[], 100);
    let id = Session::id_for(11, 1011);
    let s = t.get(&id).unwrap();
    assert_eq!(s.usage.bytes_written, Some(1000 + 30 * 1_048_576));
    let r = s.usage.write_mb_per_hour.unwrap();
    assert!((r - 1200.0).abs() < 1.0, "{r}");
    // cargo exits: what it wrote stays counted.
    procs.pop();
    t.observe(2063, &procs, &[], 100);
    assert_eq!(
        t.get(&id).unwrap().usage.bytes_written,
        Some(1000 + 30 * 1_048_576)
    );

    let mut t = Tracker::default();
    let mut q = p(11, 1, "claude", Some("claude"));
    q.written = None;
    t.observe(2000, &[q], &[], 100);
    let s = t.get(&id).unwrap();
    assert_eq!(s.usage.bytes_written, None);
    assert_eq!(s.usage.write_mb_per_hour, None);
}

/// A hook can make a process a root; detected agents keep their source.
#[test]
fn hook_registration() {
    let mut t = Tracker::default();
    let procs = vec![p(50, 1, "tool", None), p(51, 50, "make", None)];
    let id = t.register(
        2000,
        50,
        1050,
        "tool",
        "tool --x".into(),
        Some("claude".into()),
        Some("abc".into()),
        Some(PathBuf::from("/code/a")),
        None,
    );
    assert_eq!(t.get(&id).unwrap().source, RootSource::Hook);
    t.observe(2003, &procs, &[], 100);
    let s = t.get(&id).unwrap();
    assert_eq!(s.usage.processes, 2);
    assert_eq!(s.client_session_id.as_deref(), Some("abc"));
    assert_eq!(s.source, RootSource::Hook);
    // The registered process exits: the registration goes with it.
    t.observe(2006, &[], &[], 100);
    assert_eq!(t.get(&id).unwrap().status, SessionStatus::Ended);
    t.observe(2009, &procs[..1], &[], 100);
    // Same pid and start time, but no longer registered and not an agent.
    assert_eq!(t.get(&id).unwrap().status, SessionStatus::Ended);
}

#[test]
fn listening_ports_follow_membership() {
    let mut t = Tracker::default();
    t.observe(
        2000,
        &[p(11, 1, "claude", Some("claude")), p(12, 11, "node", None)],
        &[],
        100,
    );
    t.set_listening(&[(12, 3000), (12, 3000), (99, 22)]);
    assert_eq!(
        t.get(&Session::id_for(11, 1011)).unwrap().listening,
        vec![3000]
    );
}
