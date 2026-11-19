use super::*;

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

fn run(at: u64, secs: u64, code: Option<i32>, head: char) -> Timing {
    Timing {
        at_unix: at,
        duration_ms: secs * 1000,
        exit_code: code,
        command: "cargo test --workspace".into(),
        key: crate::issues::normalize_command("cargo test --workspace"),
        label: "repo-x: cargo test".into(),
        cwd: Some("/w/repo-x".into()),
        workspace: Some("/w/repo-x".into()),
        branch: Some("main".into()),
        head: Some(sha(head)),
        session_id: None,
        agent: None,
    }
}

fn moved(at: u64, prev: char, head: char, sid: &str, agent: &str) -> CommitEntry {
    CommitEntry {
        at_unix: at,
        workspace: "/w/repo-x".into(),
        branch: Some("main".into()),
        head: sha(head),
        previous_head: Some(sha(prev)),
        session_id: Some(sid.into()),
        agent: Some(agent.into()),
        via: "hook".into(),
    }
}

#[test]
fn a_slow_streak_becomes_one_finding_with_the_commits_in_between() {
    let mut v: Vec<Timing> = (0..10).map(|i| run(i, 120, Some(0), 'a')).collect();
    // A failed run is not counted, however long it took.
    v.push(run(10, 900, Some(101), 'a'));
    v.extend((11..14).map(|i| run(i, 250, Some(0), 'c')));
    let log = vec![
        moved(5, 'a', 'b', "s-1-1", "claude"),
        moved(9, 'b', 'c', "s-2-2", "codex"),
    ];
    let f = findings(&v, &log, &PerfConfig::default(), 100);
    assert_eq!(f.len(), 1, "{f:#?}");
    let f = &f[0];
    assert_eq!(f.kind, FindingKind::BuildSlower);
    assert_eq!(f.category, FindingCategory::Performance);
    assert_eq!(
        f.title,
        "`cargo test --workspace` in repo-x is 2.1× slower than usual (4m10s vs 2m)"
    );
    assert_eq!(f.severity, Severity::Medium);
    let p = f.evidence.perf.as_ref().unwrap();
    assert_eq!(p.baseline_ms, 120_000);
    assert_eq!(p.recent_ms, vec![250_000; 3]);
    assert_eq!(p.from_head.as_deref(), Some(sha('a').as_str()));
    assert_eq!(p.to_head.as_deref(), Some(sha('c').as_str()));
    assert_eq!(
        p.commits
            .iter()
            .map(|c| c.agent.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["claude", "codex"]
    );
    assert!(f.why.contains("claude, codex"), "{}", f.why);
    assert_eq!(f.evidence.cmd.as_deref(), Some("cargo test --workspace"));
    assert_eq!(f.first_seen_unix, 11);
    // The id is stable and the issue fingerprint groups it.
    assert_eq!(
        f.id,
        finding_id(Some(Path::new("/w/repo-x")), "cargo test --workspace")
    );
    assert!(crate::issues::fingerprint(f).is_some());
    let route = &f.suggested[0].api_request.as_ref().unwrap().route;
    assert_eq!(
        route,
        "/v1/timings?workspace=/w/repo-x&command=cargo%20test%20--workspace"
    );
}

#[test]
fn same_head_says_the_cause_is_outside_the_code() {
    let mut v: Vec<Timing> = (0..5).map(|i| run(i, 60, Some(0), 'a')).collect();
    v.extend((5..8).map(|i| run(i, 200, Some(0), 'a')));
    let f = findings(&v, &[], &PerfConfig::default(), 100);
    assert_eq!(f.len(), 1);
    assert!(f[0].why.contains("did not move"), "{}", f[0].why);
    assert!(f[0].evidence.perf.as_ref().unwrap().commits.is_empty());
}

#[test]
fn steady_runs_runs_without_exit_codes_and_other_commands_raise_nothing() {
    let mut v: Vec<Timing> = (0..10).map(|i| run(i, 120, Some(0), 'a')).collect();
    // No exit code: not counted as successful.
    v.extend((10..13).map(|i| run(i, 400, None, 'a')));
    assert!(findings(&v, &[], &PerfConfig::default(), 100).is_empty());
    // Another command's slow runs do not mix in.
    let mut other: Vec<Timing> = (13..16).map(|i| run(i, 400, Some(0), 'a')).collect();
    for o in &mut other {
        o.key = "npm run build".into();
    }
    v.extend(other);
    assert!(findings(&v, &[], &PerfConfig::default(), 100).is_empty());
}

#[test]
fn thresholds_come_from_the_config() {
    let mut v: Vec<Timing> = (0..10).map(|i| run(i, 100, Some(0), 'a')).collect();
    v.extend((10..12).map(|i| run(i, 140, Some(0), 'a')));
    assert!(findings(&v, &[], &PerfConfig::default(), 100).is_empty());
    let cfg = PerfConfig {
        slow_runs: 2,
        slower_ratio: 1.3,
        min_slowdown_secs: 20,
        ..PerfConfig::default()
    };
    let f = findings(&v, &[], &cfg, 100);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Low);
}

#[test]
fn reports_are_checked() {
    let ok = TimingReport {
        duration_ms: Some(10),
        exit_code: Some(0),
        command: Some("make".into()),
        cwd: Some("/w".into()),
    };
    assert!(ok.check().is_ok());
    assert!(TimingReport::default().check().is_err());
    for bad in [
        TimingReport {
            duration_ms: Some(MAX_DURATION_MS + 1),
            ..ok.clone()
        },
        TimingReport {
            cwd: Some("relative/dir".into()),
            ..ok.clone()
        },
        TimingReport {
            command: Some("x".repeat(70_000)),
            ..ok.clone()
        },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
}

#[test]
fn commands_are_redacted_and_cut() {
    let c = clean_command("deploy --token ghp_0123456789abcdefghijABCDEFGHIJ012345\n--x");
    assert!(!c.contains("ghp_0123456789"), "{c}");
    assert!(!c.contains('\n'));
    let long = clean_command(&"a ".repeat(5000));
    assert_eq!(long.chars().count(), MAX_COMMAND_CHARS);
}
