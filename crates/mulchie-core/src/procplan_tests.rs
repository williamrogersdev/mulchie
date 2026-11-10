//! Plan contents for group and pid actions, on fake tables.

use super::*;
use std::path::PathBuf;

const APP: &str = "/Applications/Google Chrome.app";

fn m(pid: u32, ppid: u32, name: &str, exe: &str, kind: ProcKind) -> ProcSample {
    ProcSample {
        pid,
        ppid,
        own: true,
        name: name.into(),
        exe: Some(PathBuf::from(exe)),
        workspace: None,
        kind,
        cpu_pct: 1.0,
        mem_bytes: 1,
        start_time: 500 + pid as u64,
        stopped: false,
    }
}

fn chrome() -> Vec<ProcSample> {
    vec![
        m(
            101,
            100,
            "Google Chrome Helper",
            &format!("{APP}/Contents/Frameworks/H.app/Contents/MacOS/H"),
            ProcKind::Other,
        ),
        m(
            100,
            1,
            "Google Chrome",
            &format!("{APP}/Contents/MacOS/Google Chrome"),
            ProcKind::Other,
        ),
        m(
            102,
            100,
            "Google Chrome Helper (GPU)",
            &format!("{APP}/Contents/Frameworks/G.app/Contents/MacOS/G"),
            ProcKind::Other,
        ),
    ]
}

fn signals(d: &Drafted) -> Vec<(u32, Signal, u64)> {
    d.requests
        .iter()
        .filter_map(|r| match r {
            Request::Signal {
                pid,
                signal,
                expected_start_time,
            } => Some((*pid, *signal, *expected_start_time)),
            _ => None,
        })
        .collect()
}

fn draft(
    kind: GroupKind,
    ms: &[ProcSample],
    a: GroupAction,
    id: Option<&str>,
) -> Result<Drafted, (&'static str, String)> {
    draft_group(
        kind,
        "Google Chrome",
        Some(Path::new(APP)),
        id,
        ms,
        a,
        10,
        true,
    )
}

#[test]
fn quitting_an_app_signals_its_main_process_and_names_the_rest() {
    let d = draft(GroupKind::App, &chrome(), GroupAction::Quit, None).unwrap();
    assert_eq!(signals(&d), vec![(100, Signal::Term, 600)]);
    assert_eq!(d.affected.len(), 3);
    let how: Vec<(u32, &str)> = d.affected.iter().map(|a| (a.pid, a.how.as_str())).collect();
    assert!(how.contains(&(100, "signalled")));
    assert!(how.contains(&(101, "exits_with_app")));
    assert!(
        d.detail
            .contains("its 2 other process(es) should exit with it"),
        "{}",
        d.detail
    );
}

#[test]
fn force_quit_pause_resume_and_lower_priority_cover_every_member() {
    for (a, s) in [
        (GroupAction::ForceQuit, Signal::Kill),
        (GroupAction::Pause, Signal::Stop),
        (GroupAction::Resume, Signal::Cont),
    ] {
        let d = draft(GroupKind::App, &chrome(), a, None).unwrap();
        let got = signals(&d);
        assert_eq!(got.len(), 3, "{a:?}");
        assert!(got
            .iter()
            .all(|(pid, sig, st)| *sig == s && *st == 500 + *pid as u64));
    }
    let d = draft(GroupKind::App, &chrome(), GroupAction::LowerPriority, None).unwrap();
    assert_eq!(d.requests.len(), 3);
    assert!(d.requests.iter().all(|r| matches!(
        r,
        Request::Renice { nice: 10, expected_start_time, pid } if *expected_start_time == 500 + *pid as u64
    )));
}

#[test]
fn restart_is_offered_only_where_it_is_honest() {
    let d = draft(
        GroupKind::App,
        &chrome(),
        GroupAction::Restart,
        Some("com.google.Chrome"),
    )
    .unwrap();
    assert_eq!(signals(&d), vec![(100, Signal::Term, 600)]);
    assert_eq!(
        d.requests.last(),
        Some(&Request::ReopenApp {
            bundle_id: "com.google.Chrome".into(),
            after_pid: 100,
            after_start_time: 600,
        })
    );
    assert!(d
        .restart
        .as_deref()
        .unwrap()
        .contains("open -b com.google.Chrome"));

    let e = draft(GroupKind::App, &chrome(), GroupAction::Restart, None).unwrap_err();
    assert_eq!(e.0, "restart_unavailable");
    let ws: Vec<ProcSample> = chrome()
        .into_iter()
        .map(|mut p| {
            p.workspace = Some((PathBuf::from("/code/r"), "r".into()));
            p
        })
        .collect();
    let e = draft_group(
        GroupKind::Workspace,
        "r",
        None,
        None,
        &ws,
        GroupAction::Restart,
        10,
        true,
    )
    .unwrap_err();
    assert_eq!(e.0, "restart_unavailable");
    assert!(e.1.contains("cannot restart workspace"));
    let e = draft_group(
        GroupKind::App,
        "x",
        Some(Path::new(APP)),
        Some("a.b"),
        &chrome(),
        GroupAction::Restart,
        10,
        false,
    )
    .unwrap_err();
    assert_eq!(e.0, "restart_unavailable");
}

#[test]
fn stop_builds_takes_only_builds_and_workspace_quit_takes_top_levels() {
    let ws = vec![
        m(300, 200, "cargo", "/c/cargo", ProcKind::Build),
        m(301, 300, "rustc", "/c/rustc", ProcKind::Build),
        m(400, 200, "node", "/n/node", ProcKind::Runtime),
    ];
    let d = draft_group(
        GroupKind::Workspace,
        "r › w",
        None,
        None,
        &ws,
        GroupAction::StopBuilds,
        10,
        true,
    )
    .unwrap();
    let pids: Vec<u32> = signals(&d).iter().map(|s| s.0).collect();
    assert_eq!(pids, vec![300, 301]);
    let d = draft_group(
        GroupKind::Workspace,
        "r › w",
        None,
        None,
        &ws,
        GroupAction::Quit,
        10,
        true,
    )
    .unwrap();
    let pids: Vec<u32> = signals(&d).iter().map(|s| s.0).collect();
    assert_eq!(pids, vec![300, 400], "top-level processes only");
    let e = draft(GroupKind::App, &chrome(), GroupAction::StopBuilds, None).unwrap_err();
    assert_eq!(e.0, "no_builds");
    let e = draft(GroupKind::App, &[], GroupAction::Quit, None).unwrap_err();
    assert_eq!(e.0, "unknown_group");
}

fn f(pid: u32, ppid: u32) -> ProcFacts {
    ProcFacts {
        pid,
        ppid,
        uid: Some(501),
        name: format!("p{pid}"),
        exe: None,
        cmd: vec![],
        cwd: None,
        start_time: 900 + pid as u64,
    }
}

#[test]
fn pid_plans_signal_descendants_first() {
    let procs = vec![f(10, 1), f(11, 10), f(12, 11), f(13, 10), f(20, 1)];
    let d = draft_pid(&procs, 10, Signal::Term, true).unwrap();
    let order: Vec<u32> = signals(&d).iter().map(|s| s.0).collect();
    assert_eq!(order, vec![12, 11, 13, 10]);
    assert!(signals(&d).iter().all(|(p, _, st)| *st == 900 + *p as u64));
    assert!(d.detail.contains("and 3 descendant(s)"));
    let d = draft_pid(&procs, 10, Signal::Kill, false).unwrap();
    assert_eq!(signals(&d), vec![(10, Signal::Kill, 910)]);
    assert_eq!(
        draft_pid(&procs, 99, Signal::Term, false).unwrap_err().0,
        "unknown_pid"
    );
}
