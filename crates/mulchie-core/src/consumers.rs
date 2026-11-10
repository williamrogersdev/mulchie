//! What is using the CPU and memory, grouped the way a person thinks about it,
//! and the same explanation in words for the app, the CLI and agents.
//!
//! Every process belongs to exactly one group:
//! 1. a **workspace** (repo › worktree) when the monitor attributed it;
//! 2. else, for the user's own processes, an **application**: on macOS the
//!    outermost `.app` bundle of its executable (Chrome's helpers are inside
//!    `Google Chrome.app`), or of the nearest ancestor reached without passing
//!    through a shell (a desktop app's child processes); on Linux the nearest
//!    such ancestor's executable name;
//! 3. else the **process** name (its executable's file name);
//! 4. other users' processes form one **system** group.
//!
//! Pure functions over [`ProcSample`]s, so tests run on fake tables. Nothing
//! here acts; group actions are planned in the engine and carried out by the
//! safety gate.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::monitor::ProcKind;

/// How much attention something needs. Ordered: `Info < Low < Medium < High`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

/// One process as the grouping needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub ppid: u32,
    pub own: bool,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// The attributed checkout and its label (`repo › worktree`).
    pub workspace: Option<(PathBuf, String)>,
    pub kind: ProcKind,
    /// Percent of one core (may exceed 100).
    pub cpu_pct: f32,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
    pub start_time: u64,
    /// Stopped (SIGSTOP / `T` state).
    pub stopped: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    Workspace,
    App,
    Process,
    System,
}

/// What can be done to a whole group. Every action is planned first
/// (`POST /v1/process/plan`) and carried out through the safety gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GroupAction {
    /// SIGTERM to the group's main process (an app's helpers exit with it);
    /// for a workspace or a process group, to each top-level process.
    Quit,
    /// SIGKILL to every process in the group (SIGCONT first for stopped ones).
    ForceQuit,
    /// SIGSTOP to every process.
    Pause,
    /// SIGCONT to every process.
    Resume,
    /// Raise the nice value of every process (lower its priority).
    LowerPriority,
    /// SIGTERM to the group's build processes only.
    StopBuilds,
    /// Quit, wait for the main process to exit, then `open -b <bundle id>`.
    /// Only for an application with a bundle id, on macOS.
    Restart,
}

impl GroupAction {
    pub const ALL: [GroupAction; 7] = [
        GroupAction::Quit,
        GroupAction::ForceQuit,
        GroupAction::Pause,
        GroupAction::Resume,
        GroupAction::LowerPriority,
        GroupAction::StopBuilds,
        GroupAction::Restart,
    ];
}

/// One process inside a group, for display.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MemberProc {
    pub pid: u32,
    pub name: String,
    pub kind: ProcKind,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub start_time: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConsumerGroup {
    /// Stable id: `workspace:<path>`, `app:<bundle path>`, `process:<name>`
    /// or `system`. Group actions name it.
    pub key: String,
    pub kind: GroupKind,
    /// What a person calls it: "mulchie › agent-x", "Google Chrome", "node".
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// Summed percent of one core (800 = eight cores' worth).
    pub cpu_pct: f32,
    /// `cpu_pct / 100`.
    pub cpu_cores: f32,
    /// Share of the whole machine: `cpu_pct / cores`.
    pub cpu_share_pct: f32,
    /// Summed resident memory. Shared pages count in every process that maps
    /// them, so this can overstate what the group alone uses.
    pub mem_bytes: u64,
    pub mem_share_pct: f32,
    pub procs: usize,
    /// The group's top-level process: the earliest-started member whose parent
    /// is not in the group (an app's main executable when there is one).
    pub main_pid: u32,
    pub main_start_time: u64,
    /// The three biggest members by CPU, then memory.
    pub top: Vec<MemberProc>,
    pub agents: usize,
    pub builds: usize,
    /// Why the group cannot be acted on, if it cannot (system process,
    /// Mulchie itself, another user's, the engine's own parent app…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protected: Option<String>,
    /// The actions a plan can be asked for. Empty when protected.
    pub actions: Vec<GroupAction>,
}

/// Every group, plus the two capped lists people look at.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Consumers {
    pub cores: usize,
    pub mem_total: u64,
    /// Top groups by CPU, at most [`TOP_GROUPS`].
    pub by_cpu: Vec<ConsumerGroup>,
    /// Top groups by memory, at most [`TOP_GROUPS`].
    pub by_mem: Vec<ConsumerGroup>,
    /// How many groups there are in all.
    pub groups: usize,
    pub memory_note: String,
}

pub const TOP_GROUPS: usize = 10;

pub const MEMORY_NOTE: &str = "Memory per group is resident memory summed over its processes; pages shared between processes are counted in each, so totals can exceed what is really used.";

/// Every member of every group: what group actions are planned from. Not
/// serialized (the lists above are what people read).
pub type Members = HashMap<String, Vec<ProcSample>>;

// ---------------------------------------------------------------- protection

/// Process names that are never acted on as a group: the session, the window
/// server and desktop, the init system, and Mulchie.
pub const PROTECTED_NAMES: &[&str] = &[
    "launchd",
    "kernel_task",
    "WindowServer",
    "loginwindow",
    "Finder",
    "Dock",
    "SystemUIServer",
    "ControlCenter",
    "NotificationCenter",
    "Spotlight",
    "coreaudiod",
    "systemd",
    "init",
    "Xorg",
    "Xwayland",
    "gnome-shell",
    "plasmashell",
    "kwin_wayland",
    "mulchie",
    "Mulchie",
];

/// Executables under these directories belong to the operating system.
/// `/System/Applications/` (Terminal, Music, …) is not among them: those are
/// ordinary apps a person may quit.
pub const PROTECTED_PREFIXES: &[&str] = &[
    "/System/Library/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/sbin/",
    "/usr/lib/systemd/",
    "/lib/systemd/",
];

/// Why one process may not be acted on through a group or signal plan, if it
/// may not. `self_pid` is the engine's own pid.
pub fn protected_reason(pid: u32, name: &str, exe: Option<&Path>, self_pid: u32) -> Option<String> {
    if pid <= 1 {
        return Some("pid 0 and 1 belong to the operating system".into());
    }
    if pid == self_pid {
        return Some("Mulchie's own process".into());
    }
    let base = exe
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().to_string());
    if PROTECTED_NAMES
        .iter()
        .any(|n| *n == name || base.as_deref() == Some(*n))
    {
        return Some(format!("{name} is part of the desktop or system"));
    }
    if let Some(e) = exe {
        let s = e.to_string_lossy();
        if PROTECTED_PREFIXES.iter().any(|p| s.starts_with(p)) {
            return Some(format!("{name} is an operating-system program ({s})"));
        }
    }
    None
}

// ------------------------------------------------------------------ grouping

/// The outermost `X.app` directory in an executable path.
pub fn outer_bundle(exe: &Path) -> Option<PathBuf> {
    let s = exe.to_str()?;
    let i = s.find(".app/")?;
    Some(PathBuf::from(&s[..i + 4]))
}

fn bundle_name(bundle: &Path) -> String {
    bundle
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

const SHELLS: &[&str] = &[
    "zsh", "-zsh", "bash", "-bash", "fish", "-fish", "sh", "-sh", "dash", "tcsh", "csh", "nu",
    "login", "tmux", "screen", "sshd",
];

fn is_shell(p: &ProcSample) -> bool {
    let base = p
        .exe
        .as_deref()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().to_string());
    SHELLS
        .iter()
        .any(|s| p.name == *s || base.as_deref() == Some(*s))
}

/// Executable file name, else the process name.
fn exe_name(p: &ProcSample) -> String {
    p.exe
        .as_deref()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| p.name.clone())
}

/// (key, kind, display name, bundle path) for a process that is not in a
/// workspace and is the user's own.
fn app_or_process(
    p: &ProcSample,
    by_pid: &HashMap<u32, &ProcSample>,
) -> (String, GroupKind, String, Option<PathBuf>) {
    if let Some(b) = p.exe.as_deref().and_then(outer_bundle) {
        return (
            format!("app:{}", b.display()),
            GroupKind::App,
            bundle_name(&b),
            Some(b),
        );
    }
    // A desktop app's children (not through a shell) belong to the app.
    let mut cur = p.ppid;
    for _ in 0..8 {
        let Some(a) = by_pid.get(&cur) else { break };
        if !a.own || a.pid <= 1 || is_shell(a) {
            break;
        }
        if let Some(b) = a.exe.as_deref().and_then(outer_bundle) {
            return (
                format!("app:{}", b.display()),
                GroupKind::App,
                bundle_name(&b),
                Some(b),
            );
        }
        cur = a.ppid;
    }
    let n = exe_name(p);
    (format!("process:{n}"), GroupKind::Process, n, None)
}

/// The group key of every process, and per key its kind, name and bundle.
pub fn assign(procs: &[ProcSample]) -> HashMap<u32, (String, GroupKind, String, Option<PathBuf>)> {
    let by_pid: HashMap<u32, &ProcSample> = procs.iter().map(|p| (p.pid, p)).collect();
    procs
        .iter()
        .map(|p| {
            let g = if let Some((path, label)) = &p.workspace {
                (
                    format!("workspace:{}", path.display()),
                    GroupKind::Workspace,
                    label.clone(),
                    None,
                )
            } else if !p.own {
                (
                    "system".to_string(),
                    GroupKind::System,
                    "System and other users".to_string(),
                    None,
                )
            } else {
                app_or_process(p, &by_pid)
            };
            (p.pid, g)
        })
        .collect()
}

/// Context the grouping needs beyond the process list.
pub struct GroupCtx<'a> {
    pub cores: usize,
    pub mem_total: u64,
    pub self_pid: u32,
    /// The engine's ancestors (the terminal or app that started it): a group
    /// holding one is protected.
    pub ancestors: &'a [u32],
    pub macos: bool,
    /// Bundle ids already known (bundle path → id); see [`bundle_id`].
    pub bundle_ids: &'a HashMap<PathBuf, Option<String>>,
}

/// Group the processes, total them, and keep the members for planning.
pub fn group(procs: &[ProcSample], ctx: &GroupCtx) -> (Consumers, Members) {
    let assigned = assign(procs);
    let mut members: Members = HashMap::new();
    let mut meta: HashMap<String, (GroupKind, String, Option<PathBuf>)> = HashMap::new();
    for p in procs {
        let Some((key, kind, name, bundle)) = assigned.get(&p.pid) else {
            continue;
        };
        members.entry(key.clone()).or_default().push(p.clone());
        meta.entry(key.clone())
            .or_insert_with(|| (*kind, name.clone(), bundle.clone()));
    }
    let cores = ctx.cores.max(1);
    let mut all: Vec<ConsumerGroup> = members
        .iter()
        .map(|(key, ms)| {
            let (kind, name, bundle) =
                meta.get(key)
                    .cloned()
                    .unwrap_or((GroupKind::Process, key.clone(), None));
            summarize(key, kind, name, bundle, ms, ctx, cores)
        })
        .collect();
    let groups = all.len();
    all.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(a.key.cmp(&b.key)));
    let by_cpu: Vec<ConsumerGroup> = all.iter().take(TOP_GROUPS).cloned().collect();
    all.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes).then(a.key.cmp(&b.key)));
    let by_mem: Vec<ConsumerGroup> = all.into_iter().take(TOP_GROUPS).collect();
    (
        Consumers {
            cores,
            mem_total: ctx.mem_total,
            by_cpu,
            by_mem,
            groups,
            memory_note: MEMORY_NOTE.into(),
        },
        members,
    )
}

/// The member whose parent is outside the group, earliest started; for an app
/// prefer the bundle's own `Contents/MacOS` executable.
pub fn main_member<'a>(ms: &'a [ProcSample], bundle: Option<&Path>) -> Option<&'a ProcSample> {
    let pids: std::collections::HashSet<u32> = ms.iter().map(|p| p.pid).collect();
    let roots: Vec<&ProcSample> = ms.iter().filter(|p| !pids.contains(&p.ppid)).collect();
    let pool = if roots.is_empty() {
        ms.iter().collect::<Vec<_>>()
    } else {
        roots
    };
    if let Some(b) = bundle {
        let main_dir = b.join("Contents/MacOS");
        if let Some(p) = pool
            .iter()
            .filter(|p| {
                p.exe
                    .as_deref()
                    .is_some_and(|e| e.parent() == Some(&main_dir))
            })
            .min_by_key(|p| (p.start_time, p.pid))
        {
            return Some(p);
        }
    }
    pool.into_iter().min_by_key(|p| (p.start_time, p.pid))
}

fn summarize(
    key: &str,
    kind: GroupKind,
    name: String,
    bundle: Option<PathBuf>,
    ms: &[ProcSample],
    ctx: &GroupCtx,
    cores: usize,
) -> ConsumerGroup {
    let cpu_pct: f32 = ms.iter().map(|p| p.cpu_pct).sum();
    let mem_bytes: u64 = ms.iter().map(|p| p.mem_bytes).sum();
    let main = main_member(ms, bundle.as_deref());
    let mut top: Vec<&ProcSample> = ms.iter().collect();
    top.sort_by(|a, b| {
        b.cpu_pct
            .total_cmp(&a.cpu_pct)
            .then(b.mem_bytes.cmp(&a.mem_bytes))
            .then(a.pid.cmp(&b.pid))
    });
    let protected = group_protection(kind, ms, ctx);
    let bundle_id = bundle
        .as_ref()
        .and_then(|b| ctx.bundle_ids.get(b).cloned().flatten());
    let actions = if protected.is_some() {
        Vec::new()
    } else {
        offered_actions(kind, ms, bundle_id.is_some() && ctx.macos)
    };
    ConsumerGroup {
        key: key.to_string(),
        kind,
        name,
        bundle_path: bundle,
        bundle_id,
        workspace: ms
            .first()
            .and_then(|p| p.workspace.as_ref().map(|w| w.0.clone()))
            .filter(|_| kind == GroupKind::Workspace),
        cpu_pct,
        cpu_cores: cpu_pct / 100.0,
        cpu_share_pct: cpu_pct / cores as f32,
        mem_bytes,
        mem_share_pct: if ctx.mem_total == 0 {
            0.0
        } else {
            mem_bytes as f32 * 100.0 / ctx.mem_total as f32
        },
        procs: ms.len(),
        main_pid: main.map(|p| p.pid).unwrap_or(0),
        main_start_time: main.map(|p| p.start_time).unwrap_or(0),
        top: top
            .into_iter()
            .take(3)
            .map(|p| MemberProc {
                pid: p.pid,
                name: p.name.clone(),
                kind: p.kind,
                cpu_pct: p.cpu_pct,
                mem_bytes: p.mem_bytes,
                start_time: p.start_time,
            })
            .collect(),
        agents: ms.iter().filter(|p| p.kind == ProcKind::Agent).count(),
        builds: ms.iter().filter(|p| p.kind == ProcKind::Build).count(),
        protected,
        actions,
    }
}

/// Why a whole group may not be acted on, if it may not.
pub fn group_protection(kind: GroupKind, ms: &[ProcSample], ctx: &GroupCtx) -> Option<String> {
    if kind == GroupKind::System {
        return Some("processes of the system and other users".into());
    }
    if let Some(p) = ms.iter().find(|p| ctx.ancestors.contains(&p.pid)) {
        return Some(format!(
            "{} (pid {}) started Mulchie; quitting it would stop Mulchie too",
            p.name, p.pid
        ));
    }
    for p in ms {
        if let Some(r) = protected_reason(p.pid, &p.name, p.exe.as_deref(), ctx.self_pid) {
            return Some(r);
        }
    }
    None
}

fn offered_actions(kind: GroupKind, ms: &[ProcSample], can_restart: bool) -> Vec<GroupAction> {
    let mut v = vec![
        GroupAction::Quit,
        GroupAction::ForceQuit,
        GroupAction::Pause,
        GroupAction::Resume,
        GroupAction::LowerPriority,
    ];
    if ms.iter().any(|p| p.kind == ProcKind::Build) {
        v.push(GroupAction::StopBuilds);
    }
    if kind == GroupKind::App && can_restart {
        v.push(GroupAction::Restart);
    }
    v
}

/// `CFBundleIdentifier` of an app bundle: from an XML `Info.plist` directly,
/// from a binary one with `plutil` (macOS, 1 s limit). Callers cache it.
pub fn bundle_id(bundle: &Path) -> Option<String> {
    let plist = bundle.join("Contents/Info.plist");
    let data = std::fs::read(&plist).ok()?;
    let id = if data.starts_with(b"bplist") {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let mut cmd = std::process::Command::new("/usr/bin/plutil");
        cmd.args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
            .arg(&plist);
        let out = crate::git::run_with_timeout(cmd, std::time::Duration::from_secs(1)).ok()?;
        if !out.success {
            return None;
        }
        String::from_utf8(out.stdout).ok()?.trim().to_string()
    } else {
        let text = String::from_utf8_lossy(&data);
        let after = text.split("<key>CFBundleIdentifier</key>").nth(1)?;
        let start = after.find("<string>")? + "<string>".len();
        let end = after[start..].find("</string>")? + start;
        after[start..end].trim().to_string()
    };
    valid_bundle_id(&id).then_some(id)
}

/// Letters, digits, `.`, `-` and `_` only (what `open -b` is given).
pub fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

// ------------------------------------------------------------------ summaries

/// CPU pressure in words, the same for the app, the CLI and agents.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct CpuSummary {
    /// Whole-machine CPU use, 0–100 (100 = every core busy).
    pub pct: f32,
    pub cores: usize,
    pub load1: f64,
    /// `load1 / cores`: above 1, work is waiting for a core.
    pub runnable_per_core: f64,
    /// About how many runnable tasks are waiting beyond the cores
    /// (`load1 - cores`, at least 0).
    pub waiting: f64,
    pub headline: String,
    pub detail: String,
    pub severity: Option<Severity>,
}

/// Memory in words.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct MemSummary {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// The kernel's own pressure signal: `normal`, `warning`, `critical`, or
    /// `None` when the platform does not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<String>,
    pub headline: String,
    pub detail: String,
    pub severity: Option<Severity>,
}

fn gb(b: u64) -> String {
    let g = b as f64 / 1e9;
    if g >= 10.0 {
        format!("{g:.0} GB")
    } else {
        format!("{g:.1} GB")
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

const CPU_MEANING: &str = "100% means every core is in use; a load above the number of cores means work is queued and things feel slow.";

/// Explain CPU use: `pct` is whole-machine use (0–100), `load1` the 1-minute
/// load average, `top` the groups by CPU.
pub fn cpu_summary(pct: f32, cores: usize, load1: f64, top: &[ConsumerGroup]) -> CpuSummary {
    let cores = cores.max(1);
    let c = cores as f64;
    let waiting = (load1 - c).max(0.0);
    let w = waiting.round() as usize;
    let used_cores = pct as f64 / 100.0 * c;
    let all = if cores == 1 {
        "The only core is busy".to_string()
    } else {
        format!("All {cores} cores are busy")
    };
    let headline = if pct >= 90.0 && w >= 1 {
        format!(
            "{all} and about {} waiting",
            plural(w, "more task is", "more tasks are")
        )
    } else if pct >= 90.0 {
        all
    } else if w >= 1 {
        format!(
            "CPU is at {pct:.0}% but about {} waiting: likely on disk or memory",
            plural(w, "task is", "tasks are")
        )
    } else if pct >= 50.0 {
        format!("CPU is busy: about {used_cores:.0} of {cores} cores in use")
    } else {
        format!("CPU is mostly idle: {pct:.0}% of {cores} cores in use")
    };
    let severity = if load1 > 2.0 * c {
        Severity::High
    } else if load1 > c + 0.5 {
        Severity::Medium
    } else if pct >= 85.0 {
        Severity::Low
    } else {
        Severity::Info
    };
    let tops: Vec<String> = top
        .iter()
        .filter(|g| g.cpu_pct >= 5.0)
        .take(3)
        .map(|g| format!("{} {:.1} cores", g.name, g.cpu_cores))
        .collect();
    let detail = if tops.is_empty() {
        CPU_MEANING.to_string()
    } else {
        format!("Top: {}. {CPU_MEANING}", tops.join(", "))
    };
    CpuSummary {
        pct,
        cores,
        load1,
        runnable_per_core: load1 / c,
        waiting,
        headline,
        detail,
        severity: Some(severity),
    }
}

/// Explain memory: totals from the snapshot, the kernel's pressure level when
/// known, `top` the groups by memory.
pub fn mem_summary(
    total: u64,
    available: u64,
    swap_total: u64,
    swap_used: u64,
    pressure: Option<&str>,
    top: &[ConsumerGroup],
) -> MemSummary {
    let used = total.saturating_sub(available);
    let free_pct = if total == 0 {
        100.0
    } else {
        available as f64 * 100.0 / total as f64
    };
    let severity = if pressure == Some("critical") || free_pct < 5.0 {
        Severity::High
    } else if pressure == Some("warning") || free_pct < 15.0 {
        Severity::Medium
    } else {
        Severity::Info
    };
    let word = match severity {
        Severity::High => "Memory is under pressure",
        Severity::Medium | Severity::Low => "Memory is tight",
        Severity::Info => "Memory is fine",
    };
    let swap = if swap_used == 0 {
        "no swap in use".to_string()
    } else {
        format!("{} swap", gb(swap_used))
    };
    let headline = format!("{word}: {} of {} used, {swap}", gb(used), gb(total));
    let tops: Vec<String> = top
        .iter()
        .take(3)
        .map(|g| format!("{} {}", g.name, gb(g.mem_bytes)))
        .collect();
    let mut detail = String::new();
    if !tops.is_empty() {
        detail.push_str(&format!("Top: {}. ", tops.join(", ")));
    }
    detail.push_str(MEMORY_NOTE);
    MemSummary {
        total,
        used,
        available,
        swap_used,
        swap_total,
        pressure: pressure.map(String::from),
        headline,
        detail,
        severity: Some(severity),
    }
}

/// The kernel's memory pressure level: macOS `kern.memorystatus_vm_pressure_level`
/// (1 normal, 2 warning, 4 critical); Linux PSI `/proc/pressure/memory`.
pub fn memory_pressure() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let name = c"kern.memorystatus_vm_pressure_level";
        let r = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut v as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return None;
        }
        return Some(match v {
            4 => "critical",
            2 => "warning",
            _ => "normal",
        });
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
        return Some(linux_pressure(&text));
    }
    #[allow(unreachable_code)]
    None
}

/// Map PSI text to a level: `full avg10` over 5 → critical; `some avg10` over
/// 10 → warning.
pub fn linux_pressure(text: &str) -> &'static str {
    let avg10 = |kind: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(kind))
            .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg10=")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    };
    if avg10("full") > 5.0 {
        "critical"
    } else if avg10("some") > 10.0 {
        "warning"
    } else {
        "normal"
    }
}

#[cfg(test)]
#[path = "consumers_tests.rs"]
mod tests;
