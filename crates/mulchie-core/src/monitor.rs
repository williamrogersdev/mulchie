use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessStatus,
    ProcessesToUpdate, System, UpdateKind,
};

use crate::paths::home_dir;
use crate::process::ProcFacts;

/// Per-process git root lookup: `(root, in_worktree)`.
type DirectEntry = Option<(PathBuf, bool)>;
/// Working directory → git root, remembered between samples.
pub type RootCache = HashMap<PathBuf, Option<(PathBuf, bool)>>;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProcKind {
    Agent,
    Build,
    Runtime,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub kind: ProcKind,
    pub group: String,
    pub workspace: Option<PathBuf>,
    pub own: bool,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub read_bps: u64,
    pub write_bps: u64,
    pub nice: i32,
    pub stopped: bool,
    pub run_secs: u64,
    /// Seconds since the epoch; a signal request carries it so a reused pid is
    /// not hit.
    pub start_time: u64,
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub path: PathBuf,
    pub label: String,
    pub repo: String,
    pub name: String,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub exists: bool,
    pub paused: bool,
    pub throttled: usize,
    pub agents: usize,
    pub builds: usize,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub read_bps: u64,
    pub write_bps: u64,
    pub orphan: Option<String>,
    pub procs: Vec<ProcInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupUsage {
    pub group: String,
    pub is_workspace: bool,
    pub procs: usize,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub read_bps: u64,
    pub write_bps: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    pub at_unix: u64,
    pub cpu_pct: f32,
    pub cpu_count: usize,
    pub load: [f64; 3],
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disk_mount: PathBuf,
    pub disk_total: u64,
    pub disk_free: u64,
    pub read_bps: u64,
    pub write_bps: u64,
    pub proc_count: usize,
    pub agent_count: usize,
    pub build_count: usize,
    pub workspaces: Vec<Workspace>,
    pub groups: Vec<GroupUsage>,
    pub top: Vec<ProcInfo>,
}

impl Snapshot {
    pub fn mem_free_pct(&self) -> f32 {
        if self.mem_total == 0 {
            return 100.0;
        }
        self.mem_available as f32 * 100.0 / self.mem_total as f32
    }

    pub fn mem_used(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_available)
    }

    pub fn disk_free_gb(&self) -> f64 {
        self.disk_free as f64 / 1e9
    }

    pub fn busy_paths(&self) -> Vec<PathBuf> {
        self.workspaces.iter().map(|w| w.path.clone()).collect()
    }

    pub fn find_proc(&self, pid: u32) -> Option<&ProcInfo> {
        self.top
            .iter()
            .chain(self.workspaces.iter().flat_map(|w| w.procs.iter()))
            .find(|p| p.pid == pid)
    }
}

const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "droid",
    "kimi",
    "aider",
    "goose",
    "opencode",
    "amp",
    "qwen",
    "cursor-agent",
    "crush",
    "cline",
    "copilot",
];
const BUILDS: &[&str] = &[
    "cargo",
    "rustc",
    "clang",
    "clang++",
    "cc",
    "c++",
    "ld",
    "ld64",
    "swiftc",
    "swift-frontend",
    "xcodebuild",
    "go",
    "gcc",
    "g++",
    "make",
    "ninja",
    "cmake",
    "tsc",
    "esbuild",
    "webpack",
    "vite",
    "rollup",
    "gradle",
    "javac",
    "zig",
    "lld",
    "sccache",
    "build-script-build",
];
const RUNTIMES: &[&str] = &["node", "bun", "deno", "python", "python3", "ruby", "java"];

pub fn classify(name: &str) -> ProcKind {
    let n = name.to_ascii_lowercase();
    let base = n.split(['.', ' ']).next().unwrap_or(&n);
    if AGENTS.contains(&base) {
        ProcKind::Agent
    } else if BUILDS.contains(&base) {
        ProcKind::Build
    } else if RUNTIMES.contains(&base) || base.starts_with("python3") {
        ProcKind::Runtime
    } else {
        ProcKind::Other
    }
}

pub struct Monitor {
    sys: System,
    home: PathBuf,
    root_cache: RootCache,
    branch_cache: HashMap<PathBuf, (Instant, Option<String>)>,
    last: Instant,
    me: u32,
    samples: u64,
}

impl Monitor {
    /// A monitor for the home directory in `$HOME`. Entry points only.
    pub fn new() -> Self {
        Self::with_home(home_dir())
    }

    pub fn with_home(home: PathBuf) -> Self {
        let mut sys = System::new().expect("system info unavailable");
        sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_usage());
        Self {
            sys,
            home,
            root_cache: HashMap::new(),
            branch_cache: HashMap::new(),
            last: Instant::now(),
            me: std::process::id(),
            samples: 0,
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        self.sys
            .refresh_cpu_specifics(CpuRefreshKind::nothing().with_usage());
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram().with_swap());
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                // A shell that `cd`s into a worktree must be seen there.
                .with_cwd(UpdateKind::Always)
                .with_user(UpdateKind::OnlyIfNotSet)
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        let secs = self.last.elapsed().as_secs_f64().max(0.5);
        self.last = Instant::now();
        let io_valid = self.samples > 0;
        self.samples += 1;

        let my_uid = unsafe { libc::getuid() };
        let mut groups: HashMap<PathBuf, Workspace> = HashMap::new();
        let mut usage: HashMap<String, GroupUsage> = HashMap::new();
        let mut all: Vec<ProcInfo> = Vec::with_capacity(self.sys.processes().len());
        let (mut agent_count, mut build_count) = (0, 0);
        let (mut read_total, mut write_total) = (0u64, 0u64);

        let facts: Vec<ProcFacts> = self
            .sys
            .processes()
            .iter()
            .filter(|(_, p)| p.thread_kind().is_none())
            .map(|(pid, p)| ProcFacts {
                pid: pid.as_u32(),
                ppid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                uid: p.user_id().map(|u| **u),
                name: String::new(),
                exe: None,
                cmd: Vec::new(),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                start_time: p.start_time(),
            })
            .collect();
        let roots = attribute(&facts, my_uid, &self.home, &mut self.root_cache);
        let bcache = &mut self.branch_cache;
        let home = self.home.clone();
        for (pid, p) in self.sys.processes() {
            if pid.as_u32() == self.me || p.thread_kind().is_some() {
                continue;
            }
            let own = p.user_id().map(|u| **u == my_uid).unwrap_or(false);
            let name = p.name().to_string_lossy().to_string();
            let kind = classify(&name);
            let du = p.disk_usage();
            let (read_bps, write_bps) = if io_valid {
                (
                    (du.read_bytes as f64 / secs) as u64,
                    (du.written_bytes as f64 / secs) as u64,
                )
            } else {
                (0, 0)
            };
            read_total += read_bps;
            write_total += write_bps;

            let ws_root = roots.get(&pid.as_u32()).cloned();
            let group = match &ws_root {
                Some((root, wt)) => label_for(root, *wt),
                None => app_group(p.exe(), &name),
            };
            let cmd = shorten_cmd(
                &p.cmd()
                    .iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                &home,
            );
            let info = ProcInfo {
                pid: pid.as_u32(),
                ppid: p.parent().map(|x| x.as_u32()).unwrap_or(0),
                name,
                kind,
                group: group.clone(),
                workspace: ws_root.as_ref().map(|r| r.0.clone()),
                own,
                cpu_pct: p.cpu_usage(),
                mem_bytes: p.memory(),
                read_bps,
                write_bps,
                nice: if own { priority_of(pid.as_u32()) } else { 0 },
                stopped: matches!(p.status(), ProcessStatus::Stop),
                run_secs: p.run_time(),
                start_time: p.start_time(),
                cmd,
            };

            let g = usage.entry(group.clone()).or_insert_with(|| GroupUsage {
                group: group.clone(),
                is_workspace: ws_root.is_some(),
                procs: 0,
                cpu_pct: 0.0,
                mem_bytes: 0,
                read_bps: 0,
                write_bps: 0,
            });
            g.procs += 1;
            g.cpu_pct += info.cpu_pct;
            g.mem_bytes += info.mem_bytes;
            g.read_bps += read_bps;
            g.write_bps += write_bps;

            if let Some((root, is_wt)) = ws_root {
                match kind {
                    ProcKind::Agent => agent_count += 1,
                    ProcKind::Build => build_count += 1,
                    _ => {}
                }
                let ws = groups.entry(root.clone()).or_insert_with(|| Workspace {
                    label: group.clone(),
                    repo: repo_name(&root, is_wt),
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    branch: cached_branch(bcache, &root),
                    exists: root.exists(),
                    path: root,
                    is_worktree: is_wt,
                    paused: false,
                    throttled: 0,
                    agents: 0,
                    builds: 0,
                    cpu_pct: 0.0,
                    mem_bytes: 0,
                    read_bps: 0,
                    write_bps: 0,
                    orphan: None,
                    procs: Vec::new(),
                });
                match kind {
                    ProcKind::Agent => ws.agents += 1,
                    ProcKind::Build => ws.builds += 1,
                    _ => {}
                }
                if info.nice > 0 {
                    ws.throttled += 1;
                }
                ws.cpu_pct += info.cpu_pct;
                ws.mem_bytes += info.mem_bytes;
                ws.read_bps += read_bps;
                ws.write_bps += write_bps;
                ws.procs.push(info.clone());
            }
            all.push(info);
        }

        let mut workspaces: Vec<Workspace> = groups.into_values().collect();
        for w in &mut workspaces {
            w.procs.sort_by(|a, b| {
                (a.kind as u8)
                    .cmp(&(b.kind as u8))
                    .then(b.cpu_pct.total_cmp(&a.cpu_pct))
            });
            w.paused = !w.procs.is_empty() && w.procs.iter().all(|p| p.stopped);
            w.orphan = orphan_reason(w);
        }
        workspaces.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));

        let mut gl: Vec<GroupUsage> = usage.into_values().collect();
        gl.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));

        let proc_count = all.len();
        let mut pick: Vec<u32> = Vec::new();
        all.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
        pick.extend(all.iter().take(60).map(|p| p.pid));
        all.sort_by_key(|p| std::cmp::Reverse(p.mem_bytes));
        pick.extend(all.iter().take(60).map(|p| p.pid));
        all.sort_by_key(|p| std::cmp::Reverse(p.read_bps + p.write_bps));
        pick.extend(
            all.iter()
                .take(30)
                .filter(|p| p.read_bps + p.write_bps > 0)
                .map(|p| p.pid),
        );
        pick.sort_unstable();
        pick.dedup();
        let mut top: Vec<ProcInfo> = all
            .into_iter()
            .filter(|p| pick.binary_search(&p.pid).is_ok())
            .collect();
        top.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));

        let (disk_mount, disk_total, disk_free) = home_disk(&self.home);
        let load = System::load_average()
            .map(|l| [l.one, l.five, l.fifteen])
            .unwrap_or_default();

        Snapshot {
            at_unix: now_unix(),
            cpu_pct: self.sys.global_cpu_usage(),
            cpu_count: self.sys.cpus().len(),
            load,
            mem_total: self.sys.total_memory(),
            mem_available: self.sys.available_memory(),
            swap_total: self.sys.total_swap(),
            swap_used: self.sys.used_swap(),
            disk_mount,
            disk_total,
            disk_free,
            read_bps: read_total,
            write_bps: write_total,
            proc_count,
            agent_count,
            build_count,
            workspaces,
            groups: gl.into_iter().take(40).collect(),
            top,
        }
    }

    pub fn process_kinds(&self) -> Vec<(Pid, ProcKind)> {
        self.sys
            .processes()
            .iter()
            .map(|(pid, p)| (*pid, classify(&p.name().to_string_lossy())))
            .collect()
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

fn orphan_reason(w: &Workspace) -> Option<String> {
    if !w.exists {
        return Some("worktree directory was deleted but processes are still running".into());
    }
    if w.agents == 0 && w.cpu_pct > 50.0 {
        let oldest = w.procs.iter().map(|p| p.run_secs).max().unwrap_or(0);
        if oldest > 120 {
            return Some(format!(
                "no agent attached, {:.0}% CPU for {}",
                w.cpu_pct,
                fmt_dur(oldest)
            ));
        }
    }
    None
}

fn fmt_dur(s: u64) -> String {
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

fn app_group(exe: Option<&Path>, name: &str) -> String {
    if let Some(exe) = exe {
        let s = exe.to_string_lossy();
        if let Some(i) = s.find(".app/") {
            let head = &s[..i];
            let app = head.rsplit('/').next().unwrap_or(head);
            return app.to_string();
        }
    }
    name.to_string()
}

/// Attribute each process to the git checkout it works in: its own working
/// directory's checkout, else the nearest own ancestor's (up to 8 levels). Only
/// processes owned by `uid` with a working directory under `home` are attributed.
pub fn attribute(
    procs: &[ProcFacts],
    uid: u32,
    home: &Path,
    cache: &mut RootCache,
) -> HashMap<u32, (PathBuf, bool)> {
    let mut direct: HashMap<u32, (u32, bool, DirectEntry)> = HashMap::with_capacity(procs.len());
    for p in procs {
        let own = p.uid == Some(uid);
        let root = if own {
            p.cwd.as_deref().and_then(|c| cached_root(cache, c, home))
        } else {
            None
        };
        direct.insert(p.pid, (p.ppid, own, root));
    }
    let resolve = |pid: u32| -> Option<(PathBuf, bool)> {
        let (mut parent, own, root) = direct.get(&pid)?.clone();
        if root.is_some() || !own {
            return root;
        }
        for _ in 0..8 {
            let (pp, pown, proot) = direct.get(&parent)?;
            if !pown || *pp == 0 {
                return None;
            }
            if proot.is_some() {
                return proot.clone();
            }
            parent = *pp;
        }
        None
    };
    procs
        .iter()
        .filter_map(|p| resolve(p.pid).map(|r| (p.pid, r)))
        .collect()
}

fn cached_root(cache: &mut RootCache, cwd: &Path, home: &Path) -> Option<(PathBuf, bool)> {
    if let Some(hit) = cache.get(cwd) {
        return hit.clone();
    }
    let found = find_root(cwd, home);
    if cache.len() > 20_000 {
        cache.clear();
    }
    cache.insert(cwd.to_path_buf(), found.clone());
    found
}

fn cached_branch(
    cache: &mut HashMap<PathBuf, (Instant, Option<String>)>,
    root: &Path,
) -> Option<String> {
    if let Some((t, b)) = cache.get(root) {
        if t.elapsed().as_secs() < 30 {
            return b.clone();
        }
    }
    let b = git_branch(root);
    cache.insert(root.to_path_buf(), (Instant::now(), b.clone()));
    b
}

fn find_root(cwd: &Path, home: &Path) -> Option<(PathBuf, bool)> {
    if !cwd.starts_with(home) || cwd == home {
        return None;
    }
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        if dir == home {
            break;
        }
        if let Ok(meta) = std::fs::symlink_metadata(dir.join(".git")) {
            return Some((dir.to_path_buf(), meta.is_file()));
        }
        cur = dir.parent();
    }
    if !cwd.exists() {
        let comps: Vec<_> = cwd.components().collect();
        for i in 1..comps.len() {
            if comps[i].as_os_str() == "worktrees"
                && comps[i - 1].as_os_str() == ".claude"
                && i + 1 < comps.len()
            {
                let root: PathBuf = comps[..=i + 1].iter().collect();
                return Some((root, true));
            }
        }
    }
    None
}

fn repo_name(root: &Path, is_worktree: bool) -> String {
    if is_worktree {
        let comps: Vec<_> = root.components().collect();
        for i in 0..comps.len() {
            if comps[i].as_os_str() == "worktrees" && i >= 2 {
                return comps[i - 2].as_os_str().to_string_lossy().to_string();
            }
        }
    }
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn git_branch(root: &Path) -> Option<String> {
    let dotgit = root.join(".git");
    let head = if dotgit.is_dir() {
        dotgit.join("HEAD")
    } else {
        let txt = std::fs::read_to_string(&dotgit).ok()?;
        let dir = txt.trim().strip_prefix("gitdir:")?.trim();
        let dir = PathBuf::from(dir);
        let dir = if dir.is_absolute() {
            dir
        } else {
            root.join(dir)
        };
        dir.join("HEAD")
    };
    let txt = std::fs::read_to_string(head).ok()?;
    let txt = txt.trim();
    match txt.strip_prefix("ref: refs/heads/") {
        Some(b) => Some(b.to_string()),
        None => Some(txt.chars().take(8).collect()),
    }
}

pub fn priority_of(pid: u32) -> i32 {
    unsafe { libc::getpriority(libc::PRIO_PROCESS as _, pid as libc::id_t) }
}

fn shorten_cmd(cmd: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let out: Vec<String> = cmd
        .split_whitespace()
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                part.rsplit('/').next().unwrap_or(part).to_string()
            } else {
                part.replace(home.as_ref(), "~")
            }
        })
        .collect();
    let s = out.join(" ");
    if s.chars().count() > 220 {
        let mut t: String = s.chars().take(219).collect();
        t.push('…');
        t
    } else {
        s
    }
}

fn label_for(root: &Path, is_worktree: bool) -> String {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !is_worktree {
        return name;
    }
    format!("{} › {name}", repo_name(root, true))
}

fn home_disk(home: &Path) -> (PathBuf, u64, u64) {
    let Ok(disks) = Disks::new_with_refreshed_list() else {
        return (PathBuf::from("/"), 0, 0);
    };
    let mut best: Option<&sysinfo::Disk> = None;
    for d in disks.iter() {
        let mp = d.mount_point();
        if home.starts_with(mp) {
            let longer = best
                .map(|b| mp.as_os_str().len() > b.mount_point().as_os_str().len())
                .unwrap_or(true);
            if longer {
                best = Some(d);
            }
        }
    }
    match best {
        Some(d) => (
            d.mount_point().to_path_buf(),
            d.total_space(),
            d.available_space(),
        ),
        None => (PathBuf::from("/"), 0, 0),
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{FakeProcesses, ProcessSource};

    #[test]
    fn attribution_follows_cwd_and_parents_from_a_fake_table() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let repo = home.join("code/repo");
        let wt = repo.join(".claude/worktrees/w");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: x").unwrap();
        let mut table = FakeProcesses::empty()
            .with(10, "claude", Some(wt.join("src")), 1)
            .with(11, "cargo", None, 1)
            .with(12, "zsh", Some(repo.clone()), 1)
            .with(13, "zsh", Some(home.clone()), 1);
        table.procs[1].ppid = 10;
        let mut foreign = table.procs[2].clone();
        foreign.pid = 14;
        foreign.uid = Some(table.uid + 1);
        table.procs.push(foreign);
        let mut cache = RootCache::new();
        let roots = attribute(&table.list(), table.current_uid(), &home, &mut cache);
        assert_eq!(roots.get(&10), Some(&(wt.clone(), true)));
        assert_eq!(roots.get(&11), Some(&(wt.clone(), true)), "child inherits");
        assert_eq!(roots.get(&12), Some(&(repo.clone(), false)));
        assert!(!roots.contains_key(&13), "home itself is no workspace");
        assert!(!roots.contains_key(&14), "other users are not attributed");
    }
}
