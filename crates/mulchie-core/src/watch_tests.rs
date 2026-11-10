//! Detectors against fake process tables and temp dirs. No real processes are
//! signalled, and nothing outside a tempdir is read for a decision.

use super::*;
use crate::config::Dismissal;

const UID: u32 = 501;
const NOW: u64 = 1_000_000;

fn facts(pid: u32, ppid: u32, name: &str, exe: &Path, cwd: Option<&Path>, start: u64) -> ProcFacts {
    ProcFacts {
        pid,
        ppid,
        uid: Some(UID),
        name: name.into(),
        exe: Some(exe.to_path_buf()),
        cmd: vec![
            exe.display().to_string(),
            "--token".into(),
            "sk-live-abcdefghijklmnopqrstuvwxyz0123".into(),
        ],
        cwd: cwd.map(Path::to_path_buf),
        start_time: start,
    }
}

fn sample(p: &ProcFacts, kind: ProcKind, ws: Option<&Path>) -> ProcSample {
    ProcSample {
        pid: p.pid,
        ppid: p.ppid,
        own: true,
        name: p.name.clone(),
        exe: p.exe.clone(),
        workspace: ws.map(|w| (w.to_path_buf(), "repo".to_string())),
        kind,
        cpu_pct: 0.0,
        mem_bytes: 1,
        start_time: p.start_time,
        stopped: false,
    }
}

struct World {
    tmp: tempfile::TempDir,
    root: PathBuf,
    temp: PathBuf,
    cfg: WatchConfig,
    procs: Vec<ProcFacts>,
    samples: HashMap<u32, ProcSample>,
    tracker: Tracker,
    listeners: Option<Vec<Listener>>,
    sigs: HashMap<PathBuf, SigStatus>,
    quar: HashSet<PathBuf>,
    posture: Posture,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("code");
        let temp = base.join("tmpdir");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        Self {
            tmp,
            root,
            temp,
            cfg: WatchConfig::default(),
            procs: Vec::new(),
            samples: HashMap::new(),
            tracker: Tracker::default(),
            listeners: None,
            sigs: HashMap::new(),
            quar: HashSet::new(),
            posture: Posture::default(),
        }
    }

    /// An executable file that exists and is older than every start time.
    fn exe(&self, rel: &str) -> PathBuf {
        let p = self.tmp.path().canonicalize().unwrap().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1000))
            .unwrap();
        p
    }

    fn add(&mut self, p: ProcFacts, kind: ProcKind, ws: Option<&Path>) {
        self.samples.insert(p.pid, sample(&p, kind, ws));
        self.procs.push(p);
    }

    fn run(&self) -> Vec<Finding> {
        let home = self.tmp.path().join("home");
        let roots = vec![self.root.clone()];
        let temps = vec![self.temp.clone()];
        let i = WatchInput {
            now: NOW,
            cfg: &self.cfg,
            procs: &self.procs,
            samples: &self.samples,
            tracker: &self.tracker,
            listeners: self.listeners.as_deref(),
            signatures: &self.sigs,
            quarantined: &self.quar,
            uid: UID,
            self_pid: 4242,
            home: &home,
            roots: &roots,
            temp_dirs: &temps,
            posture: &self.posture,
        };
        detect(&i)
    }
}

fn kinds(f: &[Finding]) -> Vec<FindingKind> {
    let mut k: Vec<FindingKind> = f.iter().map(|f| f.kind).collect();
    k.sort();
    k.dedup();
    k
}

fn one(f: &[Finding], k: FindingKind) -> &Finding {
    f.iter()
        .find(|f| f.kind == k)
        .unwrap_or_else(|| panic!("no {k:?} in {f:#?}"))
}

#[test]
fn an_ordinary_process_produces_nothing() {
    let mut w = World::new();
    let exe = w.exe("bin/node");
    let ws = w.root.join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    w.add(
        facts(10, 5, "node", &exe, Some(&ws), NOW - 600),
        ProcKind::Runtime,
        Some(&ws),
    );
    assert!(w.run().is_empty(), "{:#?}", w.run());
}

#[test]
fn orphans_in_a_workspace_are_reported_with_quit_and_pause() {
    let mut w = World::new();
    let exe = w.exe("bin/node");
    let ws = w.root.join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    w.add(
        facts(10, 1, "node", &exe, Some(&ws), NOW - 600),
        ProcKind::Runtime,
        Some(&ws),
    );
    // An orphan outside any workspace is a normal daemon.
    let d = w.exe("bin/daemon");
    w.add(
        facts(11, 1, "daemon", &d, Some(Path::new("/")), NOW - 600),
        ProcKind::Runtime,
        None,
    );
    let f = w.run();
    assert_eq!(kinds(&f), vec![FindingKind::OrphanedProcess]);
    let o = one(&f, FindingKind::OrphanedProcess);
    assert_eq!(o.id, format!("orphaned_process:10:{}", NOW - 600));
    assert_eq!(o.severity, Severity::Low);
    assert_eq!(o.evidence.workspace.as_deref(), Some(ws.as_path()));
    assert!(o.suggested.iter().any(|s| s.reversible
        && s.request
            == Some(Request::Signal {
                pid: 10,
                signal: Signal::Stop,
                expected_start_time: NOW - 600
            })));
    // Old orphans are Medium.
    w.procs[0].start_time = NOW - 100 * 3600;
    w.samples.get_mut(&10).unwrap().start_time = NOW - 100 * 3600;
    assert_eq!(
        one(&w.run(), FindingKind::OrphanedProcess).severity,
        Severity::Medium
    );
}

#[test]
fn command_lines_in_evidence_are_redacted() {
    let mut w = World::new();
    let exe = w.exe("bin/node");
    let ws = w.root.join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    w.add(
        facts(10, 1, "node", &exe, Some(&ws), NOW - 600),
        ProcKind::Runtime,
        Some(&ws),
    );
    let f = w.run();
    let cmd = one(&f, FindingKind::OrphanedProcess)
        .evidence
        .cmd
        .clone()
        .unwrap();
    assert!(!cmd.contains("sk-live"), "{cmd}");
    assert!(cmd.contains(crate::redact::REDACTED), "{cmd}");
}

#[test]
fn a_deleted_working_directory_or_executable_is_reported() {
    let mut w = World::new();
    let exe = w.exe("bin/cargo");
    let gone = w.root.join("removed-worktree");
    w.add(
        facts(10, 5, "cargo", &exe, Some(&gone), NOW - 600),
        ProcKind::Build,
        None,
    );
    let gone_exe = w.root.join("nope/tool");
    w.add(
        facts(11, 5, "tool", &gone_exe, None, NOW - 600),
        ProcKind::Other,
        None,
    );
    // Linux marks a deleted exe with a suffix.
    let marked = PathBuf::from(format!("{} (deleted)", exe.display()));
    w.add(
        facts(12, 5, "old", &marked, None, NOW - 600),
        ProcKind::Other,
        None,
    );
    let f = w.run();
    assert_eq!(
        one(&f, FindingKind::MissingWorkingDirectory).evidence.pid,
        Some(10)
    );
    let ex: Vec<u32> = f
        .iter()
        .filter(|f| f.kind == FindingKind::ExecutableChanged)
        .filter_map(|f| f.evidence.pid)
        .collect();
    assert_eq!(ex, vec![11, 12]);
}

#[test]
fn a_replaced_executable_is_reported() {
    let mut w = World::new();
    let exe = w.exe("bin/server");
    let fh = std::fs::File::options().write(true).open(&exe).unwrap();
    fh.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(NOW - 60))
        .unwrap();
    w.add(
        facts(10, 5, "server", &exe, None, NOW - 3600),
        ProcKind::Other,
        None,
    );
    let f = w.run();
    assert!(one(&f, FindingKind::ExecutableChanged)
        .title
        .contains("was replaced"));
}

fn feed(t: &mut Tracker, p: &ProcSample, from: u64, to: u64, f: impl Fn(u64, &mut ProcSample)) {
    let mut m = from;
    while m <= to {
        let mut s = p.clone();
        f(m, &mut s);
        t.observe(m, std::slice::from_ref(&s));
        m += 60;
    }
}

#[test]
fn stale_idle_agents_are_reported_but_busy_or_young_ones_are_not() {
    let mut w = World::new();
    let exe = w.exe("bin/claude");
    let old = NOW - 100 * 3600;
    w.add(
        facts(10, 5, "claude", &exe, None, old),
        ProcKind::Agent,
        None,
    );
    w.add(
        facts(11, 5, "claude", &exe, None, old),
        ProcKind::Agent,
        None,
    );
    w.add(
        facts(12, 5, "claude", &exe, None, NOW - 3600),
        ProcKind::Agent,
        None,
    );
    let (s10, s11, s12) = (
        w.samples[&10].clone(),
        w.samples[&11].clone(),
        w.samples[&12].clone(),
    );
    feed(&mut w.tracker, &s10, NOW - 3600, NOW, |_, s| {
        s.cpu_pct = 0.1
    });
    let mut t2 = Tracker::default();
    feed(&mut t2, &s11, NOW - 3600, NOW, |_, s| s.cpu_pct = 30.0);
    feed(&mut t2, &s12, NOW - 3600, NOW, |_, s| s.cpu_pct = 0.0);
    let f = w.run();
    assert_eq!(one(&f, FindingKind::StaleProcess).evidence.pid, Some(10));
    w.tracker = t2;
    assert!(!kinds(&w.run()).contains(&FindingKind::StaleProcess));
}

#[test]
fn a_long_stopped_process_offers_resume() {
    let mut w = World::new();
    let exe = w.exe("bin/vite");
    w.add(
        facts(10, 5, "vite", &exe, None, NOW - 30 * 3600),
        ProcKind::Runtime,
        None,
    );
    let mut s = w.samples[&10].clone();
    s.stopped = true;
    w.tracker.observe(NOW, &[s]);
    assert!(
        !kinds(&w.run()).contains(&FindingKind::StoppedProcess),
        "just stopped"
    );
    w.tracker
        .set_stopped_since(10, NOW - 30 * 3600, NOW - 7 * 3600);
    let f = w.run();
    let st = one(&f, FindingKind::StoppedProcess);
    assert_eq!(
        st.suggested[0].request,
        Some(Request::Signal {
            pid: 10,
            signal: Signal::Cont,
            expected_start_time: NOW - 30 * 3600
        })
    );
}

#[test]
fn runaway_cpu_and_steady_memory_growth_need_the_whole_window() {
    let mut w = World::new();
    let exe = w.exe("bin/node");
    w.add(
        facts(10, 5, "node", &exe, None, NOW - 7200),
        ProcKind::Runtime,
        None,
    );
    let s = w.samples[&10].clone();
    let start = NOW - 29 * 60;
    feed(&mut w.tracker, &s, start, NOW, |m, s| {
        s.cpu_pct = 99.0;
        s.mem_bytes = (m - start) / 60 * 50 * 1024 * 1024;
    });
    let f = w.run();
    assert_eq!(one(&f, FindingKind::RunawayCpu).severity, Severity::Medium);
    assert!(one(&f, FindingKind::RunawayCpu)
        .suggested
        .iter()
        .any(|s| matches!(s.request, Some(Request::Renice { nice: 10, .. }))));
    one(&f, FindingKind::GrowingMemory);

    // Ten minutes is too short a window to call either.
    let mut w2 = World::new();
    let exe = w2.exe("bin/node");
    w2.add(
        facts(10, 5, "node", &exe, None, NOW - 7200),
        ProcKind::Runtime,
        None,
    );
    let s = w2.samples[&10].clone();
    feed(&mut w2.tracker, &s, NOW - 600, NOW, |m, s| {
        s.cpu_pct = 99.0;
        s.mem_bytes = m * 1024 * 1024 * 1024;
    });
    assert!(w2.run().is_empty());

    // Memory that goes up and down is not a leak.
    let mut w3 = World::new();
    let exe = w3.exe("bin/node");
    w3.add(
        facts(10, 5, "node", &exe, None, NOW - 7200),
        ProcKind::Runtime,
        None,
    );
    let s = w3.samples[&10].clone();
    feed(&mut w3.tracker, &s, start, NOW, |m, s| {
        let k = (m - start) / 60;
        s.mem_bytes = if k.is_multiple_of(2) {
            (k * 100) << 20
        } else {
            1
        };
    });
    assert!(!kinds(&w3.run()).contains(&FindingKind::GrowingMemory));
}

#[test]
fn exposed_listeners_are_graded_by_where_they_run() {
    let mut w = World::new();
    let node = w.exe("bin/node");
    let ws = w.root.join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    w.add(
        facts(10, 5, "node", &node, Some(&ws), NOW - 60),
        ProcKind::Runtime,
        Some(&ws),
    );
    let app = w.exe("Apps/Spotify.app/Contents/MacOS/Spotify");
    w.add(
        facts(11, 5, "Spotify", &app, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    let tool = w.exe("bin/tool");
    w.add(
        facts(12, 5, "tool", &tool, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.add(
        facts(13, 5, "loop", &tool, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.listeners = Some(parse_lsof(
        "p10\nn*:3000\np11\nn*:57621\np12\nn0.0.0.0:8080\nn127.0.0.1:9\np13\nn127.0.0.1:5173\nn[::1]:5173\n",
    ));
    let f = w.run();
    let by: HashMap<u32, Severity> = f
        .iter()
        .filter(|f| f.kind == FindingKind::ExposedListener)
        .map(|f| (f.evidence.pid.unwrap(), f.severity))
        .collect();
    assert_eq!(by.get(&10), Some(&Severity::Medium));
    assert_eq!(by.get(&11), Some(&Severity::Info));
    assert_eq!(by.get(&12), Some(&Severity::Low));
    assert_eq!(by.get(&13), None, "loopback only");
    let e = &one(&f, FindingKind::ExposedListener).evidence;
    assert!(e.listen.is_some());
}

#[test]
fn temp_executables_are_graded_and_build_runners_are_lowered() {
    let mut w = World::new();
    let x = w.exe("tmpdir/payload");
    w.add(
        facts(10, 5, "payload", &x, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    let f = w.run();
    assert_eq!(
        one(&f, FindingKind::TempExecutable).severity,
        Severity::Medium
    );
    // Started by `go run`.
    let go = w.exe("bin/go");
    w.add(
        facts(5, 2, "go", &go, None, NOW - 100),
        ProcKind::Build,
        None,
    );
    assert_eq!(
        one(&w.run(), FindingKind::TempExecutable).severity,
        Severity::Low
    );
    // Listening beyond loopback from a temp dir is High even then.
    w.listeners = Some(vec![Listener {
        pid: 10,
        addr: "*".into(),
        port: 4444,
    }]);
    assert_eq!(
        one(&w.run(), FindingKind::TempExecutable).severity,
        Severity::High
    );
    // Allow-listed by path: gone.
    w.cfg.allow_exes = vec![x.clone()];
    assert!(!kinds(&w.run()).contains(&FindingKind::TempExecutable));
}

#[test]
fn hidden_folders_in_code_are_reported_except_ordinary_tool_folders() {
    let mut w = World::new();
    let ok = w.exe("code/repo/.venv/bin/python");
    let odd = w.exe("code/repo/.x/helper");
    w.add(
        facts(10, 5, "python", &ok, None, NOW - 60),
        ProcKind::Runtime,
        None,
    );
    w.add(
        facts(11, 5, "helper", &odd, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    let f = w.run();
    let h: Vec<u32> = f
        .iter()
        .filter(|f| f.kind == FindingKind::HiddenDirExecutable)
        .filter_map(|f| f.evidence.pid)
        .collect();
    assert_eq!(h, vec![11]);
}

#[test]
fn a_system_name_running_from_elsewhere_is_high() {
    let mut w = World::new();
    let fake = w.exe("Library/kernel_task");
    w.add(
        facts(10, 5, "kernel_task", &fake, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    let real = PathBuf::from("/usr/sbin/cron");
    w.add(
        facts(11, 5, "cron", &real, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    let f = w.run();
    let s = one(&f, FindingKind::SystemNameMismatch);
    assert_eq!(s.severity, Severity::High);
    assert_eq!(s.evidence.pid, Some(10));
    assert_eq!(
        f.iter()
            .filter(|f| f.kind == FindingKind::SystemNameMismatch)
            .count(),
        1
    );
}

#[test]
fn unsigned_apps_are_reported_but_workspace_builds_are_not() {
    let mut w = World::new();
    let app = w.exe("Apps/Odd.app/Contents/MacOS/Odd");
    let built = w.exe("code/repo/target/Mine.app/Contents/MacOS/Mine");
    let signed = w.exe("Apps/Good.app/Contents/MacOS/Good");
    w.add(
        facts(10, 5, "Odd", &app, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.add(
        facts(11, 5, "Mine", &built, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.add(
        facts(12, 5, "Good", &signed, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.sigs.insert(app.clone(), SigStatus::AdHoc);
    w.sigs.insert(built.clone(), SigStatus::Unsigned);
    w.sigs.insert(signed.clone(), SigStatus::DeveloperId);
    let f = w.run();
    let u: Vec<u32> = f
        .iter()
        .filter(|f| f.kind == FindingKind::UnsignedExecutable)
        .filter_map(|f| f.evidence.pid)
        .collect();
    assert_eq!(u, vec![10]);
    assert_eq!(
        one(&f, FindingKind::UnsignedExecutable).evidence.signature,
        Some(SigStatus::AdHoc)
    );
}

#[test]
fn quarantined_loose_binaries_are_reported() {
    let mut w = World::new();
    let x = w.exe("bin/fetched");
    let app = w.exe("Apps/A.app/Contents/MacOS/A");
    w.add(
        facts(10, 5, "fetched", &x, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.add(
        facts(11, 5, "A", &app, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.quar.insert(x.clone());
    w.quar.insert(app.clone());
    let f = w.run();
    let q: Vec<u32> = f
        .iter()
        .filter(|f| f.kind == FindingKind::QuarantinedExecutable)
        .filter_map(|f| f.evidence.pid)
        .collect();
    assert_eq!(q, vec![10]);
    assert_eq!(
        one(&f, FindingKind::QuarantinedExecutable).severity,
        Severity::Low
    );
    w.sigs.insert(x, SigStatus::Unsigned);
    assert_eq!(
        one(&w.run(), FindingKind::QuarantinedExecutable).severity,
        Severity::Medium
    );
}

#[test]
fn other_users_pid1_and_mulchie_itself_are_never_examined() {
    let mut w = World::new();
    let fake = w.exe("Library/launchd");
    let mut p = facts(10, 5, "launchd", &fake, None, NOW - 60);
    p.uid = Some(0);
    w.add(p, ProcKind::Other, None);
    w.add(
        facts(4242, 5, "launchd", &fake, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    w.add(
        facts(1, 0, "launchd", &fake, None, NOW - 60),
        ProcKind::Other,
        None,
    );
    assert!(w.run().is_empty());
}

#[test]
fn posture_findings() {
    let mut w = World::new();
    w.posture = Posture {
        config_dir: "/c".into(),
        config_dir_mode: Some(0o40755),
        token_modes: vec![
            ("/c/token".into(), 0o100600),
            ("/c/token.read".into(), 0o100644),
        ],
        api_port: Some(7450),
        lock_held_by_other: Some(77),
        config_error: Some("bad".into()),
    };
    w.listeners = Some(vec![
        Listener {
            pid: 9,
            addr: "127.0.0.1".into(),
            port: 7450,
        },
        Listener {
            pid: 9,
            addr: "0.0.0.0".into(),
            port: 7450,
        },
    ]);
    let f = w.run();
    assert_eq!(
        kinds(&f),
        vec![
            FindingKind::InsecurePermissions,
            FindingKind::ApiExposed,
            FindingKind::EngineLockNotHeld,
            FindingKind::ConfigNotLoaded
        ]
    );
    let perms: Vec<&str> = f
        .iter()
        .filter(|f| f.kind == FindingKind::InsecurePermissions)
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(
        perms,
        vec![
            "insecure_permissions:/c",
            "insecure_permissions:/c/token.read"
        ]
    );
    assert_eq!(
        one(&f, FindingKind::ApiExposed).evidence.listen,
        Some(vec!["0.0.0.0:7450".into()])
    );
    w.posture = Posture {
        config_dir_mode: Some(0o40700),
        ..Default::default()
    };
    assert!(w.run().is_empty());
}

fn finding(id: &str, sev: Severity, exe: Option<&str>) -> Finding {
    Finding {
        id: id.into(),
        kind: FindingKind::OrphanedProcess,
        category: FindingCategory::Hygiene,
        severity: sev,
        title: "t".into(),
        why: "w".into(),
        evidence: Evidence {
            exe: exe.map(PathBuf::from),
            ..Default::default()
        },
        suggested: vec![],
        first_seen_unix: NOW,
        last_seen_unix: NOW,
        dismissed: false,
        dismissed_until: None,
    }
}

#[test]
fn finalize_applies_allow_lists_dismissals_and_reports_changes() {
    let mut cfg = WatchConfig {
        allow_ids: vec!["a".into()],
        allow_exes: vec![PathBuf::from("/ok/bin")],
        ..WatchConfig::default()
    };
    cfg.dismissed = vec![
        Dismissal {
            id: "d".into(),
            until_unix: Some(NOW + 10),
        },
        Dismissal {
            id: "e".into(),
            until_unix: Some(NOW - 10),
        },
        Dismissal {
            id: "f".into(),
            until_unix: None,
        },
    ];
    let raw = vec![
        finding("a", Severity::High, None),
        finding("b", Severity::Low, Some("/ok/bin")),
        finding("c", Severity::Low, None),
        finding("d", Severity::Medium, None),
        finding("e", Severity::High, None),
        finding("f", Severity::Info, None),
    ];
    let mut prev_c = finding("c", Severity::Low, None);
    prev_c.first_seen_unix = 5;
    let prev = vec![prev_c, finding("gone", Severity::Low, None)];
    let (out, ch) = finalize(raw, &prev, &cfg, NOW);
    let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["e", "d", "c", "f"],
        "allow-listed dropped, sorted by severity"
    );
    let get = |id: &str| out.iter().find(|f| f.id == id).unwrap();
    assert!(get("d").dismissed && get("d").dismissed_until == Some(NOW + 10));
    assert!(
        !get("e").dismissed,
        "an expired dismissal no longer applies"
    );
    assert!(get("f").dismissed && get("f").dismissed_until.is_none());
    assert_eq!(get("c").first_seen_unix, 5);
    assert!(get("c").suggested.iter().any(|s| s
        .api
        .as_deref()
        .is_some_and(|a| a.contains("/v1/findings/dismiss"))));
    let app: Vec<&str> = ch.appeared.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(app, vec!["e", "d", "f"]);
    assert_eq!(ch.gone.len(), 1);
    assert_eq!(ch.gone[0].id, "gone");

    let c = FindingCounts::of(&out);
    assert_eq!(
        (c.high, c.medium, c.low, c.info, c.dismissed),
        (1, 0, 1, 0, 2)
    );
    assert_eq!(filter(&out, Some(Severity::Medium), None, false).len(), 1);
    assert_eq!(filter(&out, Some(Severity::Medium), None, true).len(), 2);
    assert_eq!(
        filter(&out, None, Some(FindingKind::ExposedListener), true).len(),
        0
    );

    assert!(prune_dismissed(&mut cfg, NOW));
    assert_eq!(cfg.dismissed.len(), 2);
}

#[test]
fn parsers() {
    let l = parse_lsof("p1\nfx\np20\nn*:22\nn[::]:22\nn127.0.0.1:631\np30\nnlocalhost:9\n");
    assert_eq!(l.len(), 4);
    assert!(!l.iter().find(|x| x.addr == "*").unwrap().loopback());
    assert!(!l.iter().find(|x| x.addr == "::").unwrap().loopback());
    assert_eq!(
        l.iter().find(|x| x.addr == "::").unwrap().shown(),
        "[::]:22"
    );
    assert!(l.iter().find(|x| x.port == 631).unwrap().loopback());

    assert_eq!(
        parse_codesign("x: code object is not signed at all"),
        SigStatus::Unsigned
    );
    assert_eq!(parse_codesign("Signature=adhoc\n"), SigStatus::AdHoc);
    assert_eq!(
        parse_codesign("Authority=Software Signing\nAuthority=Apple Root CA\n"),
        SigStatus::Apple
    );
    assert_eq!(
        parse_codesign("Authority=Developer ID Application: X (T)\n"),
        SigStatus::DeveloperId
    );
    assert_eq!(
        parse_codesign("Authority=Apple Development: me\n"),
        SigStatus::Development
    );
    assert_eq!(parse_codesign("Authority=Acme\n"), SigStatus::OtherSigned);
    assert_eq!(parse_codesign("weird"), SigStatus::Unknown);

    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1\n   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 222 1\n   2: 0100007F:1F91 0100007F:9999 01 00000000:00000000 00:00000000 00000000  1000        0 333 1\n";
    assert_eq!(
        parse_proc_net_tcp(tcp, false),
        vec![
            ("127.0.0.1".into(), 8080, 12345),
            ("0.0.0.0".into(), 22, 222)
        ]
    );
    let tcp6 = "h\n 0: 00000000000000000000000001000000:0016 00000000000000000000000000000000:0000 0A 0:0 0:0 0 0 0 77 1\n 1: 00000000000000000000000000000000:0050 00000000000000000000000000000000:0000 0A 0:0 0:0 0 0 0 78 1\n";
    assert_eq!(
        parse_proc_net_tcp(tcp6, true),
        vec![("::1".into(), 22, 77), ("::".into(), 80, 78)]
    );
}

#[test]
fn signature_candidates_rank_risky_places_and_skip_known_and_system() {
    let temp = vec![PathBuf::from("/tmp")];
    let mk = |pid, exe: &str| ProcFacts {
        pid,
        ppid: 1,
        uid: Some(UID),
        name: "x".into(),
        exe: Some(exe.into()),
        cmd: vec![],
        cwd: None,
        start_time: 1,
    };
    let procs = vec![
        mk(1, "/Applications/A.app/Contents/MacOS/A"),
        mk(2, "/tmp/x"),
        mk(3, "/usr/bin/ls"),
        mk(4, "/Users/u/code/.h/bin"),
        mk(5, "/opt/homebrew/bin/rg"),
        mk(6, "/Applications/K.app/Contents/MacOS/K"),
    ];
    let known = |p: &Path| p.ends_with("K");
    let c = signature_candidates(&procs, UID, &known, &temp, 10);
    assert_eq!(
        c,
        vec![
            PathBuf::from("/tmp/x"),
            PathBuf::from("/Users/u/code/.h/bin"),
            PathBuf::from("/Applications/A.app/Contents/MacOS/A")
        ]
    );
    assert_eq!(signature_candidates(&procs, UID, &known, &temp, 1).len(), 1);
}

#[test]
fn sig_cache_forgets_a_changed_file() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("bin");
    std::fs::write(&p, b"a").unwrap();
    let mut c = SigCache::default();
    c.put(&p, SigStatus::DeveloperId);
    assert_eq!(c.get(&p), Some(SigStatus::DeveloperId));
    std::fs::write(&p, b"ab").unwrap();
    assert_eq!(c.get(&p), None);
}

#[test]
fn finding_json_shape() {
    let f = Finding {
        id: "exposed_listener:10:5".into(),
        kind: FindingKind::ExposedListener,
        category: FindingCategory::Security,
        severity: Severity::Medium,
        title: "t".into(),
        why: "w".into(),
        evidence: Evidence {
            pid: Some(10),
            listen: Some(vec!["*:3000".into()]),
            ..Default::default()
        },
        suggested: vec![Suggestion {
            label: "q".into(),
            request: Some(Request::Signal {
                pid: 10,
                signal: Signal::Term,
                expected_start_time: 5,
            }),
            api: None,
            reversible: false,
        }],
        first_seen_unix: 1,
        last_seen_unix: 2,
        dismissed: false,
        dismissed_until: None,
    };
    let v = serde_json::to_value(&f).unwrap();
    assert_eq!(v["kind"], "exposed_listener");
    assert_eq!(v["category"], "security");
    assert_eq!(v["severity"], "medium");
    assert_eq!(
        v["evidence"],
        serde_json::json!({"pid": 10, "listen": ["*:3000"]})
    );
    assert!(v.get("dismissed_until").is_none());
    let back: Finding = serde_json::from_value(v).unwrap();
    assert_eq!(back, f);
    println!("{}", serde_json::to_string_pretty(&f).unwrap());
}

#[test]
fn every_kind_has_a_category_and_old_json_still_parses() {
    assert_eq!(
        FindingKind::LimitExceeded.category(),
        FindingCategory::Limit
    );
    assert_eq!(FindingKind::Advisory.category(), FindingCategory::Update);
    assert_eq!(FindingKind::ApiExposed.category(), FindingCategory::Posture);
    assert_eq!(FindingKind::RunawayCpu.category(), FindingCategory::Hygiene);
    // A finding serialized before `category` existed still loads.
    let mut v = serde_json::to_value(finding("x", Severity::Low, None)).unwrap();
    v.as_object_mut().unwrap().remove("category");
    let f: Finding = serde_json::from_value(v).unwrap();
    // finalize sets it from the kind.
    let (out, _) = finalize(vec![f], &[], &WatchConfig::default(), NOW);
    assert_eq!(out[0].category, FindingCategory::Hygiene);
}
