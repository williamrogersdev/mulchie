use super::*;

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

fn listening(pid: u32, start: u64) -> Listening {
    Listening {
        pid,
        start_time: start,
        ports: vec![3000],
        workspace: "/w/repo-x".into(),
        command: "node /w/repo-x/node_modules/.bin/next dev -p 3000".into(),
    }
}

fn sp(pid: u32, ppid: u32, start: u64, mb: u64, cpu: f32) -> SampleProc {
    SampleProc {
        pid,
        ppid,
        start_time: start,
        mem_bytes: mb * 1_048_576,
        cpu_pct: cpu,
    }
}

// ----------------------------------------------------- time to listen

#[test]
fn time_to_listen_is_measured_only_when_the_start_was_seen() {
    let mut t = Tracker::default();
    // The first watch run: the server already listens, its start was missed.
    t.observe_watch(1_000, 60, &[(10, 500)], Some(&[listening(10, 500)]));
    // A second server, seen not listening at the next run, listening at the
    // one after: observed, with the gap as its uncertainty.
    t.observe_watch(
        1_060,
        60,
        &[(10, 500), (11, 1_050)],
        Some(&[listening(10, 500)]),
    );
    t.observe_watch(
        1_120,
        60,
        &[(10, 500), (11, 1_050)],
        Some(&[listening(10, 500), listening(11, 1_050)]),
    );
    // A third started after the previous run and listens already.
    t.observe_watch(
        1_180,
        60,
        &[(10, 500), (11, 1_050), (12, 1_130)],
        Some(&[
            listening(10, 500),
            listening(11, 1_050),
            listening(12, 1_130),
        ]),
    );
    let live = t.live(1_200);
    let by = |pid: u32| live.iter().find(|r| r.pid == pid).unwrap();
    assert_eq!(by(10).listen_ms, None, "met already listening");
    assert_eq!(by(10).listen_uncertainty_ms, None);
    assert_eq!(by(11).listen_ms, Some(70_000));
    assert_eq!(by(11).listen_uncertainty_ms, Some(61_000));
    assert_eq!(by(12).listen_ms, Some(50_000));
    assert_eq!(
        by(12).listen_uncertainty_ms,
        Some(51_000),
        "no more than it ran"
    );
    assert_eq!(by(11).watch_interval_secs, 60);
    assert_eq!(by(11).key, "node next dev -p #");
}

#[test]
fn unknown_listeners_change_nothing_and_do_not_move_the_baseline() {
    let mut t = Tracker::default();
    t.observe_watch(1_000, 60, &[(11, 990)], Some(&[]));
    // Listeners could not be read: the previous run stays the reference.
    t.observe_watch(1_060, 60, &[(11, 990)], None);
    t.observe_watch(1_120, 60, &[(11, 990)], Some(&[listening(11, 990)]));
    let r = &t.live(1_120)[0];
    assert_eq!(r.listen_ms, Some(130_000));
    assert_eq!(r.listen_uncertainty_ms, Some(121_000), "two intervals wide");
}

#[test]
fn ports_are_added_and_capped() {
    let mut t = Tracker::default();
    t.observe_watch(1_000, 60, &[], Some(&[]));
    t.observe_watch(1_060, 60, &[], Some(&[listening(10, 1_001)]));
    let mut more = listening(10, 1_001);
    more.ports = (4000..4020).collect();
    t.observe_watch(1_120, 60, &[], Some(&[more]));
    let p = &t.live(1_120)[0].ports;
    assert_eq!(p.len(), MAX_PORTS);
    assert_eq!(p[0], 3000);
}

// ------------------------------------------------ memory, CPU, ending

/// Samples every 30 seconds from `from` to `to` of a server (pid 10, child
/// 11) whose tree memory is `mem(elapsed)` MB.
fn feed(t: &mut Tracker, start: u64, from: u64, to: u64, mem: impl Fn(u64) -> u64, cpu: f32) {
    let mut now = from;
    while now <= to {
        let m = mem(now - start);
        t.observe_sample(
            now,
            &[
                sp(1, 0, 1, 50, 1.0),
                sp(10, 1, start, m / 2, cpu),
                sp(11, 10, start + 1, m - m / 2, cpu),
                sp(12, 1, start + 2, 9_000, 400.0), // not in the tree
            ],
        );
        now += 30;
    }
}

fn started(start: u64) -> Tracker {
    let mut t = Tracker::default();
    t.observe_watch(start, 60, &[(10, start)], Some(&[]));
    t.observe_watch(
        start + 60,
        60,
        &[(10, start)],
        Some(&[listening(10, start)]),
    );
    t
}

#[test]
fn memory_settles_over_minutes_two_to_five_and_the_slope_follows() {
    let start = 10_000;
    let mut t = started(start);
    // 200 MB while starting, then 600 MB plus 60 MB per hour (1 MB/min).
    feed(
        &mut t,
        start,
        start + 60,
        start + 3_600,
        |e| {
            if e < 120 {
                200
            } else {
                600 + (e - 120) / 60
            }
        },
        50.0,
    );
    let r = &t.live(start + 3_600)[0];
    // Samples at 120, 150, ..., 300 seconds: 600..605 MB.
    let settled = r.settled_mem_bytes.unwrap() / 1_048_576;
    assert!((601..=603).contains(&settled), "{settled}");
    let slope = r.mem_slope_mb_per_hour.unwrap();
    assert!((slope - 60.0).abs() < 2.0, "{slope}");
    // The tree is the process and its child; the stranger is left out.
    assert_eq!(r.processes, 2);
    assert_eq!(r.peak_cpu_pct, 100.0);
    assert!(r.peak_mem_bytes / 1_048_576 >= 658);
    assert_eq!(r.end, End::Running);
    assert_eq!(r.run_secs, 3_600);
}

#[test]
fn a_short_watch_gives_no_slope_and_a_late_one_no_settled_memory() {
    let start = 10_000;
    let mut t = started(start);
    feed(&mut t, start, start + 60, start + 400, |_| 300, 10.0);
    let r = &t.live(start + 400)[0];
    assert!(r.settled_mem_bytes.is_some());
    assert_eq!(r.mem_slope_mb_per_hour, None, "under 10 minutes watched");
    // Met after the settle window: nothing to take a median of.
    let mut t = Tracker::default();
    t.observe_watch(start, 60, &[(10, start - 1_000)], Some(&[]));
    t.observe_watch(start + 60, 60, &[], Some(&[listening(10, start - 1_000)]));
    feed(
        &mut t,
        start - 1_000,
        start + 60,
        start + 400,
        |_| 300,
        10.0,
    );
    assert_eq!(t.live(start + 400)[0].settled_mem_bytes, None);
}

#[test]
fn a_run_that_ends_is_recorded_once_as_exited() {
    let start = 10_000;
    let mut t = started(start);
    feed(&mut t, start, start + 60, start + 600, |_| 300, 10.0);
    assert!(t.due(start + 600).is_empty(), "not due while it lives");
    // An empty sample (the table could not be read) ends nothing.
    t.observe_sample(start + 630, &[]);
    assert_eq!(t.live_count(), 1);
    // The process is gone from the next real sample.
    t.observe_sample(start + 660, &[sp(1, 0, 1, 50, 1.0)]);
    assert_eq!(t.live_count(), 0);
    let due = t.due(start + 660);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].end, End::Exited);
    assert_eq!(due[0].run_secs, 600, "to the last sample that saw it");
    assert!(t.due(start + 700).is_empty());
}

#[test]
fn a_reused_pid_is_another_run() {
    let start = 10_000;
    let mut t = started(start);
    feed(&mut t, start, start + 60, start + 150, |_| 300, 10.0);
    t.observe_sample(start + 180, &[sp(10, 1, start + 170, 50, 1.0)]);
    assert_eq!(t.live_count(), 0);
}

#[test]
fn a_server_that_lasted_under_a_minute_is_not_recorded() {
    let start = 10_000;
    let mut t = started(start);
    t.observe_sample(start + 90, &[sp(10, 1, start, 300, 1.0)]);
    t.observe_sample(start + 100, &[sp(1, 0, 1, 50, 1.0)]);
    assert!(t.due(start + 100).is_empty());
}

#[test]
fn a_record_is_written_every_half_hour_while_it_lasts() {
    let start = 10_000;
    let mut t = started(start);
    feed(&mut t, start, start + 60, start + 1_700, |_| 300, 10.0);
    assert!(t.due(start + 1_700).is_empty());
    let first = t.due(start + 60 + SNAPSHOT_SECS);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].end, End::Running);
    assert!(t.due(start + 60 + SNAPSHOT_SECS + 10).is_empty());
    assert_eq!(t.due(start + 60 + 2 * SNAPSHOT_SECS).len(), 1);
}

#[test]
fn attribution_is_set_once_for_live_and_finished_runs() {
    let start = 10_000;
    let mut t = started(start);
    assert_eq!(t.unattributed(), vec![("/w/repo-x".into(), 10, start)]);
    t.attribute(
        10,
        start,
        Some("main".into()),
        Some(sha('a')),
        Some(("s-1-1".into(), "claude".into())),
    );
    assert!(t.unattributed().is_empty());
    feed(&mut t, start, start + 60, start + 200, |_| 300, 10.0);
    t.observe_sample(start + 230, &[sp(1, 0, 1, 50, 1.0)]);
    let r = &t.due(start + 230)[0];
    assert_eq!(r.head.as_deref(), Some(sha('a').as_str()));
    assert_eq!(r.agent.as_deref(), Some("claude"));
    assert_eq!(r.branch.as_deref(), Some("main"));
}

#[test]
fn at_most_max_runs_are_tracked() {
    let mut t = Tracker::default();
    t.observe_watch(1_000, 60, &[], Some(&[]));
    let all: Vec<Listening> = (0..(MAX_RUNS as u32 + 10))
        .map(|i| listening(100 + i, 1_001))
        .collect();
    t.observe_watch(1_060, 60, &[], Some(&all));
    assert_eq!(t.live_count(), MAX_RUNS);
}

// ----------------------------------------------------------- findings

fn rec(start: u64, pid: u32, listen_s: Option<u64>, mem_mb: Option<u64>, head: char) -> DevServer {
    DevServer {
        at_unix: start + 4_000,
        pid,
        start_unix: start,
        workspace: "/w/repo-x".into(),
        branch: Some("main".into()),
        head: Some(sha(head)),
        session_id: None,
        agent: None,
        command: "next dev".into(),
        key: "next dev".into(),
        ports: vec![3000],
        listen_ms: listen_s.map(|s| s * 1000),
        listen_uncertainty_ms: listen_s.map(|_| 61_000),
        watch_interval_secs: 60,
        settled_mem_bytes: mem_mb.map(|m| m * 1_048_576),
        mem_slope_mb_per_hour: None,
        peak_cpu_pct: 100.0,
        peak_mem_bytes: 0,
        processes: 2,
        run_secs: 4_000,
        end: End::Exited,
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

fn series(base: (Option<u64>, Option<u64>), now: (Option<u64>, Option<u64>)) -> Vec<DevServer> {
    let mut v: Vec<DevServer> = (0..10)
        .map(|i| rec(1_000 + i * 10_000, 100 + i as u32, base.0, base.1, 'a'))
        .collect();
    v.extend((10..13).map(|i| rec(1_000 + i * 10_000, 100 + i as u32, now.0, now.1, 'c')));
    v
}

#[test]
fn a_slower_start_is_a_finding_with_the_commits_in_between() {
    let v = series((Some(120), Some(500)), (Some(400), Some(500)));
    let log = vec![moved(5, 'a', 'c', "s-1-1", "claude")];
    let f = findings(&v, &log, &PerfConfig::default(), 999_999);
    assert_eq!(f.len(), 1, "{f:#?}");
    let f = &f[0];
    assert_eq!(f.kind, FindingKind::DevServerSlowStart);
    assert_eq!(f.category, FindingCategory::Performance);
    assert_eq!(f.severity, Severity::Medium);
    assert_eq!(
        f.title,
        "`next dev` in repo-x takes 6m40s to start listening, 3.3× its usual 2m"
    );
    let e = f.evidence.devserver.as_ref().unwrap();
    assert_eq!(e.measure, "time_to_listen");
    assert_eq!((e.baseline, e.baseline_runs), (120_000, 10));
    assert_eq!(e.recent, vec![400_000; 3]);
    assert_eq!(e.uncertainty_ms, Some(61_000));
    assert_eq!(e.commits.len(), 1);
    assert!(
        f.why.contains("claude") && f.why.contains("watch interval"),
        "{}",
        f.why
    );
    assert_eq!(f.evidence.cmd.as_deref(), Some("next dev"));
    assert_eq!(
        f.id,
        finding_id(
            FindingKind::DevServerSlowStart,
            Path::new("/w/repo-x"),
            "next dev"
        )
    );
    assert!(crate::issues::fingerprint(f).is_some());
    assert_eq!(
        f.suggested[0].api_request.as_ref().unwrap().route,
        "/v1/devservers?workspace=/w/repo-x"
    );
}

#[test]
fn a_start_below_what_the_watch_interval_can_resolve_is_not_judged() {
    // Usual 20 s, now 90 s: both within the 61 s the watch interval blurs.
    let v = series((Some(20), None), (Some(90), None));
    assert!(findings(&v, &[], &PerfConfig::default(), 1).is_empty());
    // A slowdown smaller than the uncertainty is not a slowdown either.
    let v = series((Some(120), None), (Some(150), None));
    let cfg = PerfConfig {
        slower_ratio: 1.05,
        ..PerfConfig::default()
    };
    assert!(findings(&v, &[], &cfg, 1).is_empty());
    // ...while one clear of it is.
    let v = series((Some(120), None), (Some(190), None));
    assert_eq!(findings(&v, &[], &cfg, 1).len(), 1);
}

#[test]
fn heavier_settled_memory_is_a_finding_and_a_small_rise_is_not() {
    let make = |after: u64| series((None, Some(500)), (None, Some(after)));
    let f = findings(&make(1_200), &[], &PerfConfig::default(), 5);
    assert_eq!(f.len(), 1, "{f:#?}");
    assert_eq!(f[0].kind, FindingKind::DevServerHeavier);
    assert_eq!(
        f[0].evidence.devserver.as_ref().unwrap().measure,
        "settled_memory"
    );
    assert!(
        f[0].title.contains("settles at") && f[0].title.contains("2.4×"),
        "{}",
        f[0].title
    );
    assert!(f[0].why.contains("HEAD moved from"), "{}", f[0].why);
    // Over the ratio but under 256 MB more.
    assert!(findings(&make(700), &[], &PerfConfig::default(), 5).is_empty());
    // Not enough runs for a baseline.
    assert!(findings(&make(1_200)[8..], &[], &PerfConfig::default(), 5).is_empty());
}

#[test]
fn a_run_counts_once_by_its_newest_record_and_groups_stay_apart() {
    let mut v: Vec<DevServer> = (0..10)
        .map(|i| rec(1_000 + i * 10_000, 100 + i as u32, None, Some(500), 'a'))
        .collect();
    // Each heavy run was recorded while running, then again when it ended.
    for i in 10..13u64 {
        let mut early = rec(1_000 + i * 10_000, 100 + i as u32, None, Some(1_500), 'b');
        early.at_unix -= 1_000;
        early.end = End::Running;
        v.push(early);
        v.push(rec(
            1_000 + i * 10_000,
            100 + i as u32,
            None,
            Some(1_500),
            'b',
        ));
    }
    assert_eq!(runs(&v).len(), 13);
    assert_eq!(findings(&v, &[], &PerfConfig::default(), 5).len(), 1);
    // The same runs in another workspace are another group, too short alone.
    let other: Vec<DevServer> = v[..4]
        .iter()
        .cloned()
        .map(|mut r| {
            r.workspace = "/w/repo-y".into();
            r
        })
        .collect();
    let mut all = v.clone();
    all.extend(other);
    assert_eq!(findings(&all, &[], &PerfConfig::default(), 5).len(), 1);
}

#[test]
fn the_record_file_is_private_and_round_trips() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let r = rec(1_000, 10, Some(5), Some(300), 'a');
    file(&paths).append(&r).unwrap();
    let back: Vec<DevServer> = file(&paths).read_all();
    assert_eq!(back, vec![r]);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(file(&paths).path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(file(&paths).path().ends_with("devservers.jsonl"));
}
