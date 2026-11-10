//! Grouping, sums, headlines and the protected list, on fake process tables.

use super::*;

const CHROME: &str = "/Applications/Google Chrome.app";

fn p(pid: u32, ppid: u32, name: &str, exe: Option<&str>) -> ProcSample {
    ProcSample {
        pid,
        ppid,
        own: true,
        name: name.into(),
        exe: exe.map(PathBuf::from),
        workspace: None,
        kind: ProcKind::Other,
        cpu_pct: 0.0,
        mem_bytes: 0,
        start_time: 1000 + pid as u64,
        stopped: false,
    }
}

fn cpu(mut s: ProcSample, c: f32, m: u64) -> ProcSample {
    s.cpu_pct = c;
    s.mem_bytes = m;
    s
}

fn ctx<'a>(ancestors: &'a [u32], ids: &'a HashMap<PathBuf, Option<String>>) -> GroupCtx<'a> {
    GroupCtx {
        cores: 16,
        mem_total: 64_000_000_000,
        self_pid: 9999,
        ancestors,
        macos: true,
        bundle_ids: ids,
    }
}

fn table() -> Vec<ProcSample> {
    let ws = (
        PathBuf::from("/code/repo/.claude/worktrees/agent-x"),
        "repo › agent-x".to_string(),
    );
    let mut cargo = cpu(
        p(300, 200, "cargo", Some("/Users/u/.cargo/bin/cargo")),
        400.0,
        1_000_000_000,
    );
    cargo.kind = ProcKind::Build;
    cargo.workspace = Some(ws.clone());
    let mut rustc = cpu(
        p(301, 300, "rustc", Some("/Users/u/.rustup/rustc")),
        450.0,
        2_000_000_000,
    );
    rustc.kind = ProcKind::Build;
    rustc.workspace = Some(ws.clone());
    // An app helper run from inside a workspace: the workspace wins.
    let mut ws_helper = cpu(
        p(302, 300, "Code Helper", Some("/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper")),
        10.0,
        100,
    );
    ws_helper.workspace = Some(ws);
    let mut root = p(
        50,
        1,
        "WindowServer",
        Some("/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer"),
    );
    root.own = false;
    vec![
        cpu(p(100, 1, "Google Chrome", Some(&format!("{CHROME}/Contents/MacOS/Google Chrome"))), 20.0, 500_000_000),
        cpu(
            p(101, 100, "Google Chrome Helper (Renderer)", Some(&format!("{CHROME}/Contents/Frameworks/Google Chrome Framework.framework/Versions/1/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)"))),
            150.0,
            1_500_000_000,
        ),
        cpu(
            p(102, 100, "Google Chrome Helper (GPU)", Some(&format!("{CHROME}/Contents/Frameworks/Google Chrome Framework.framework/Versions/1/Helpers/Google Chrome Helper (GPU).app/Contents/MacOS/Google Chrome Helper (GPU)"))),
            30.0,
            200_000_000,
        ),
        // Claude desktop and a child it started directly (an MCP server).
        cpu(p(400, 1, "Claude", Some("/Applications/Claude.app/Contents/MacOS/Claude")), 5.0, 300_000_000),
        cpu(p(401, 400, "node", Some("/opt/homebrew/bin/node")), 60.0, 100_000_000),
        // A terminal, its shell, and a command run from the shell: not Terminal's.
        p(500, 1, "Terminal", Some("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal")),
        p(501, 500, "zsh", Some("/bin/zsh")),
        cpu(p(502, 501, "node", Some("/opt/homebrew/bin/node")), 25.0, 50_000_000),
        p(200, 501, "zsh", Some("/bin/zsh")),
        cargo,
        rustc,
        ws_helper,
        root,
    ]
}

#[test]
fn helpers_join_their_bundle_and_workspace_beats_app() {
    let ids = HashMap::new();
    let (c, members) = group(&table(), &ctx(&[], &ids));
    let a = assign(&table());
    let key = |pid: u32| a[&pid].0.clone();
    assert_eq!(key(101), format!("app:{CHROME}"));
    assert_eq!(key(102), format!("app:{CHROME}"));
    assert_eq!(
        key(401),
        "app:/Applications/Claude.app",
        "a desktop app's child"
    );
    assert_eq!(key(502), "process:node", "run from a shell: its own name");
    assert_eq!(key(501), "app:/System/Applications/Utilities/Terminal.app");
    assert_eq!(
        key(302),
        "workspace:/code/repo/.claude/worktrees/agent-x",
        "the workspace wins over the app bundle"
    );
    assert_eq!(key(50), "system");
    // Every process is in exactly one group.
    let n: usize = members.values().map(|v| v.len()).sum();
    assert_eq!(n, table().len());
    assert_eq!(c.groups, members.len());

    let chrome = c.by_cpu.iter().find(|g| g.name == "Google Chrome").unwrap();
    assert_eq!(chrome.kind, GroupKind::App);
    assert_eq!(chrome.procs, 3);
    assert_eq!(chrome.main_pid, 100);
    assert_eq!(chrome.main_start_time, 1100);
    assert!((chrome.cpu_pct - 200.0).abs() < 0.01);
    assert!((chrome.cpu_cores - 2.0).abs() < 0.01);
    assert!((chrome.cpu_share_pct - 12.5).abs() < 0.01);
    assert_eq!(chrome.mem_bytes, 2_200_000_000);
    assert_eq!(chrome.top[0].pid, 101);
    assert_eq!(chrome.bundle_path.as_deref(), Some(Path::new(CHROME)));

    let ws = &c.by_cpu[0];
    assert_eq!(ws.kind, GroupKind::Workspace);
    assert_eq!(ws.name, "repo › agent-x");
    assert_eq!(ws.builds, 2);
    assert_eq!(ws.main_pid, 300, "the parent is outside the group");
    assert!(ws.actions.contains(&GroupAction::StopBuilds));
    assert!(
        !ws.actions.contains(&GroupAction::Restart),
        "a workspace is never restarted"
    );
    assert_eq!(c.by_mem[0].key, ws.key, "3.0 GB beats 2.2 GB");
}

#[test]
fn lists_are_capped_and_sorted() {
    let procs: Vec<ProcSample> = (0..30)
        .map(|i| {
            cpu(
                p(1000 + i, 1, &format!("tool{i}"), None),
                i as f32,
                (30 - i) as u64,
            )
        })
        .collect();
    let ids = HashMap::new();
    let (c, _) = group(&procs, &ctx(&[], &ids));
    assert_eq!(c.by_cpu.len(), TOP_GROUPS);
    assert_eq!(c.by_mem.len(), TOP_GROUPS);
    assert_eq!(c.groups, 30);
    assert_eq!(c.by_cpu[0].name, "tool29");
    assert_eq!(c.by_mem[0].name, "tool0");
    assert!(c.by_cpu.windows(2).all(|w| w[0].cpu_pct >= w[1].cpu_pct));
}

#[test]
fn protected_groups_offer_no_actions() {
    let ids: HashMap<PathBuf, Option<String>> =
        [(PathBuf::from(CHROME), Some("com.google.Chrome".to_string()))].into();
    let mut t = table();
    t.push(p(
        600,
        1,
        "Finder",
        Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"),
    ));
    t.push(p(
        601,
        1,
        "Dock",
        Some("/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"),
    ));
    t.push(p(9999, 1, "mulchie", Some("/usr/local/bin/mulchie")));
    // Mulchie was started from Terminal (pid 500).
    let c = ctx(&[501, 500], &ids);
    let (cons, _) = group(&t, &c);
    assert!(cons.groups <= TOP_GROUPS, "every group is listed");
    let find = |key: &str| {
        cons.by_cpu
            .iter()
            .find(|g| g.key == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key}"))
    };
    for key in [
        "app:/System/Library/CoreServices/Finder.app",
        "app:/System/Library/CoreServices/Dock.app",
        "process:mulchie",
        "app:/System/Applications/Utilities/Terminal.app",
        "system",
    ] {
        let g = find(key);
        assert!(g.protected.is_some(), "{key} must be protected");
        assert!(g.actions.is_empty(), "{key}");
    }
    let chrome = find(&format!("app:{CHROME}"));
    assert!(chrome.protected.is_none());
    assert_eq!(chrome.bundle_id.as_deref(), Some("com.google.Chrome"));
    assert!(chrome.actions.contains(&GroupAction::Restart));
}

#[test]
fn the_protected_list_is_explicit() {
    let me = 4242;
    let cases: &[(u32, &str, Option<&str>, bool)] = &[
        (1, "launchd", Some("/sbin/launchd"), true),
        (0, "kernel_task", None, true),
        (me, "anything", None, true),
        (
            77,
            "WindowServer",
            Some("/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer"),
            true,
        ),
        (
            78,
            "loginwindow",
            Some("/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow"),
            true,
        ),
        (79, "Finder", None, true),
        (80, "Dock", None, true),
        (81, "SystemUIServer", None, true),
        (82, "systemd", Some("/usr/lib/systemd/systemd"), true),
        (83, "gnome-shell", Some("/usr/bin/gnome-shell"), true),
        (84, "mds", Some("/usr/libexec/mds"), true),
        (
            85,
            "Mulchie",
            Some("/Applications/Mulchie.app/Contents/MacOS/Mulchie"),
            true,
        ),
        (
            90,
            "Terminal",
            Some("/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal"),
            false,
        ),
        (
            91,
            "Google Chrome",
            Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            false,
        ),
        (92, "node", Some("/opt/homebrew/bin/node"), false),
        (93, "cargo", Some("/Users/u/.cargo/bin/cargo"), false),
    ];
    for (pid, name, exe, want) in cases {
        let got = protected_reason(*pid, name, exe.map(Path::new), me);
        assert_eq!(got.is_some(), *want, "{name} ({pid}): {got:?}");
    }
}

#[test]
fn outer_bundle_and_bundle_ids() {
    assert_eq!(
        outer_bundle(Path::new(
            "/A/X.app/Contents/Frameworks/Y.app/Contents/MacOS/Y"
        )),
        Some(PathBuf::from("/A/X.app"))
    );
    assert_eq!(outer_bundle(Path::new("/usr/bin/x")), None);
    let t = tempfile::tempdir().unwrap();
    let b = t.path().join("Thing.app");
    std::fs::create_dir_all(b.join("Contents")).unwrap();
    std::fs::write(
        b.join("Contents/Info.plist"),
        "<?xml version=\"1.0\"?><plist><dict><key>CFBundleName</key><string>Thing</string>\n<key>CFBundleIdentifier</key>\n<string>com.example.thing</string></dict></plist>",
    )
    .unwrap();
    assert_eq!(bundle_id(&b).as_deref(), Some("com.example.thing"));
    std::fs::write(
        b.join("Contents/Info.plist"),
        "<key>CFBundleIdentifier</key><string>-rf; evil</string>",
    )
    .unwrap();
    assert_eq!(bundle_id(&b), None, "only safe characters");
    assert!(valid_bundle_id("com.google.Chrome"));
    assert!(!valid_bundle_id("-a"));
    assert!(!valid_bundle_id("a b"));
}

fn g(name: &str, cpu_pct: f32, mem: u64) -> ConsumerGroup {
    let ids = HashMap::new();
    let c = ctx(&[], &ids);
    let mut s = summarize(
        "k",
        GroupKind::Process,
        name.into(),
        None,
        &[cpu(p(10, 1, name, None), cpu_pct, mem)],
        &c,
        16,
    );
    s.name = name.into();
    s
}

#[test]
fn cpu_headlines_follow_thresholds() {
    let top = [
        g("game-music", 860.0, 0),
        g("onboarding-flow-review", 190.0, 0),
    ];
    let s = cpu_summary(100.0, 16, 47.0, &top);
    assert_eq!(
        s.headline,
        "All 16 cores are busy and about 31 more tasks are waiting"
    );
    assert_eq!(s.severity, Some(Severity::High));
    assert!(
        s.detail
            .starts_with("Top: game-music 8.6 cores, onboarding-flow-review 1.9 cores."),
        "{}",
        s.detail
    );
    assert!(s.detail.contains("100% means every core is in use"));
    assert!((s.runnable_per_core - 47.0 / 16.0).abs() < 1e-9);
    assert!((s.waiting - 31.0).abs() < 1e-9);

    let s = cpu_summary(95.0, 16, 15.0, &top);
    assert_eq!(s.headline, "All 16 cores are busy");
    assert_eq!(s.severity, Some(Severity::Low));
    let s = cpu_summary(95.0, 16, 20.0, &top);
    assert_eq!(s.severity, Some(Severity::Medium));
    let s = cpu_summary(60.0, 16, 4.0, &[]);
    assert_eq!(s.headline, "CPU is busy: about 10 of 16 cores in use");
    assert_eq!(s.severity, Some(Severity::Info));
    let s = cpu_summary(12.0, 16, 2.0, &[]);
    assert_eq!(s.headline, "CPU is mostly idle: 12% of 16 cores in use");
    let s = cpu_summary(30.0, 8, 12.0, &[]);
    assert_eq!(
        s.headline,
        "CPU is at 30% but about 4 tasks are waiting: likely on disk or memory"
    );
    let s = cpu_summary(99.0, 8, 9.0, &[]);
    assert_eq!(
        s.headline,
        "All 8 cores are busy and about 1 more task is waiting"
    );
}

#[test]
fn memory_headlines_follow_thresholds() {
    let top = [g("Google Chrome", 0.0, 4_100_000_000)];
    let s = mem_summary(
        69_000_000_000,
        34_000_000_000,
        8_000_000_000,
        7_500_000_000,
        Some("normal"),
        &top,
    );
    assert_eq!(
        s.headline,
        "Memory is fine: 35 GB of 69 GB used, 7.5 GB swap"
    );
    assert_eq!(s.severity, Some(Severity::Info));
    assert!(s.detail.starts_with("Top: Google Chrome 4.1 GB. "));
    let s = mem_summary(16_000_000_000, 2_000_000_000, 0, 0, None, &[]);
    assert_eq!(s.severity, Some(Severity::Medium));
    assert!(
        s.headline
            .starts_with("Memory is tight: 14 GB of 16 GB used, no swap in use"),
        "{}",
        s.headline
    );
    let s = mem_summary(16_000_000_000, 8_000_000_000, 0, 0, Some("critical"), &[]);
    assert_eq!(s.severity, Some(Severity::High));
    assert!(s.headline.starts_with("Memory is under pressure"));
    assert_eq!(
        linux_pressure(
            "some avg10=12.00 avg60=1 avg300=0 total=1\nfull avg10=0.00 avg60=0 avg300=0 total=0"
        ),
        "warning"
    );
    assert_eq!(
        linux_pressure("some avg10=50.00\nfull avg10=9.00"),
        "critical"
    );
    assert_eq!(linux_pressure("some avg10=0.10\nfull avg10=0.00"), "normal");
}
