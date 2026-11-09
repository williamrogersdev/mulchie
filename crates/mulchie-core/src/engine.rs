use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::diskmap::{self, DiskMap, PruneResult, RemoveResult};
use crate::monitor::{now_unix, Monitor, ProcKind, Snapshot};
use crate::reclaim::{self, CleanResult, ScanReport};

const HISTORY_LEN: usize = 600;
const SAMPLE_SECS: u64 = 3;

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub at_unix: u64,
    pub kind: String,
    pub detail: String,
    pub bytes: u64,
    pub workspace: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAction {
    Pause,
    Resume,
    Deprioritize,
    StopBuilds,
    KillAll,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ProcAction {
    Term,
    Kill,
    Stop,
    Cont,
    Renice { nice: i32 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Good,
    Busy,
    Strained,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub t: u64,
    pub cpu: f32,
    pub load1: f64,
    pub mem_used: u64,
    pub swap: u64,
    pub disk_free: u64,
    pub read_bps: u64,
    pub write_bps: u64,
    pub ws: Vec<(String, f32, u64)>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Forecast {
    pub bytes_per_hour: f64,
    pub hours_to_low: Option<f64>,
    pub hours_to_full: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lease {
    pub id: u64,
    pub label: String,
    pub pid: u32,
    pub since_unix: u64,
    pub jobs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Waiter {
    pub id: u64,
    pub label: String,
    pub pid: u32,
    pub since_unix: u64,
    pub last_poll: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Slots {
    pub capacity: usize,
    pub jobs_per_slot: usize,
    pub active: Vec<Lease>,
    pub waiting: Vec<Waiter>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaseReply {
    pub granted: bool,
    pub ticket: u64,
    pub jobs: usize,
    pub position: usize,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Advice {
    pub health: Health,
    pub ok_to_start_heavy_work: bool,
    pub free_slots: usize,
    pub suggested_jobs: usize,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub version: String,
    pub config: Config,
    pub snapshot: Snapshot,
    pub scan: Option<ScanReport>,
    pub diskmap: Option<DiskMap>,
    pub scanning: bool,
    pub health: Health,
    pub forecast: Forecast,
    pub slots: Slots,
    pub actions: VecDeque<Action>,
    pub throttled: usize,
    pub low_disk: bool,
}

impl State {
    fn log(
        &mut self,
        kind: &str,
        detail: String,
        bytes: u64,
        workspace: Option<String>,
        source: &str,
    ) {
        self.actions.push_front(Action {
            at_unix: now_unix(),
            kind: kind.into(),
            detail,
            bytes,
            workspace,
            source: source.into(),
        });
        self.actions.truncate(500);
    }
}

pub fn health_of(s: &Snapshot, cfg: &Config) -> Health {
    let free_gb = s.disk_free_gb();
    let mem = s.mem_free_pct();
    let overload = s.cpu_count > 0 && s.load[0] > s.cpu_count as f64 * 4.0;
    if free_gb < (cfg.low_disk_gb as f64) / 3.0 || mem < cfg.throttle_mem_free_pct / 2.0 {
        Health::Critical
    } else if free_gb < cfg.low_disk_gb as f64 || mem < cfg.throttle_mem_free_pct || overload {
        Health::Strained
    } else if s.cpu_pct > cfg.throttle_cpu_pct {
        Health::Busy
    } else {
        Health::Good
    }
}

fn capacity(cfg: &Config, cores: usize) -> usize {
    if cfg.max_heavy_jobs > 0 {
        cfg.max_heavy_jobs
    } else {
        (cores / 4).max(2)
    }
}

#[derive(Clone)]
pub struct Engine {
    pub state: Arc<Mutex<State>>,
    history: Arc<Mutex<VecDeque<Sample>>>,
    scan_requested: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        let state = State {
            version: env!("CARGO_PKG_VERSION").into(),
            config,
            snapshot: Snapshot::default(),
            scan: None,
            diskmap: None,
            scanning: false,
            health: Health::Good,
            forecast: Forecast::default(),
            slots: Slots::default(),
            actions: VecDeque::new(),
            throttled: 0,
            low_disk: false,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY_LEN))),
            scan_requested: Arc::new(AtomicBool::new(true)),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn snapshot_state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn history(&self) -> Vec<Sample> {
        self.history.lock().unwrap().iter().cloned().collect()
    }

    pub fn config(&self) -> Config {
        self.state.lock().unwrap().config.clone()
    }

    pub fn set_config(&self, cfg: Config) -> std::io::Result<()> {
        cfg.save()?;
        self.state.lock().unwrap().config = cfg;
        self.request_scan();
        Ok(())
    }

    pub fn request_scan(&self) {
        self.scan_requested.store(true, Ordering::SeqCst);
    }

    pub fn log(
        &self,
        kind: &str,
        detail: String,
        bytes: u64,
        workspace: Option<String>,
        source: &str,
    ) {
        self.state
            .lock()
            .unwrap()
            .log(kind, detail, bytes, workspace, source);
    }

    pub fn start(&self) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("mulchie-monitor".into())
            .spawn(move || me.monitor_loop())
            .ok();
        let me = self.clone();
        std::thread::Builder::new()
            .name("mulchie-scan".into())
            .spawn(move || me.scan_loop())
            .ok();
    }

    fn monitor_loop(&self) {
        let mut mon = Monitor::new();
        let mut reniced: HashSet<u32> = HashSet::new();
        let mut last_reclaim = Instant::now() - Duration::from_secs(3600);
        loop {
            let tick = Instant::now();
            let snap = mon.sample();
            let cfg = self.config();
            let health = health_of(&snap, &cfg);

            let mut ws_top: Vec<(String, f32, u64)> = snap
                .workspaces
                .iter()
                .map(|w| (w.label.clone(), w.cpu_pct, w.mem_bytes))
                .collect();
            ws_top.sort_by(|a, b| b.1.total_cmp(&a.1));
            ws_top.truncate(10);
            let forecast = {
                let mut h = self.history.lock().unwrap();
                h.push_back(Sample {
                    t: snap.at_unix,
                    cpu: snap.cpu_pct,
                    load1: snap.load[0],
                    mem_used: snap.mem_used(),
                    swap: snap.swap_used,
                    disk_free: snap.disk_free,
                    read_bps: snap.read_bps,
                    write_bps: snap.write_bps,
                    ws: ws_top,
                });
                while h.len() > HISTORY_LEN {
                    h.pop_front();
                }
                forecast(&h, &cfg)
            };

            let pressured = snap.mem_free_pct() < cfg.throttle_mem_free_pct
                || snap.cpu_pct > cfg.throttle_cpu_pct;
            let kinds = mon.process_kinds();
            let mut newly = Vec::new();
            if cfg.throttle_builds && pressured {
                for (pid, kind) in &kinds {
                    let pid = pid.as_u32();
                    if *kind != ProcKind::Build || reniced.contains(&pid) {
                        continue;
                    }
                    if renice(pid, cfg.build_nice) {
                        reniced.insert(pid);
                        newly.push(pid);
                    }
                }
            }
            let alive: HashSet<u32> = kinds.iter().map(|(p, _)| p.as_u32()).collect();
            reniced.retain(|p| alive.contains(p));

            {
                let mut st = self.state.lock().unwrap();
                st.snapshot = snap.clone();
                st.health = health;
                st.forecast = forecast;
                st.throttled = reniced.len();
                let low = snap.disk_total > 0 && snap.disk_free_gb() < cfg.alert_below_gb as f64;
                if low && !st.low_disk {
                    st.log(
                        "alert",
                        format!("Free space below {} GB", cfg.alert_below_gb),
                        snap.disk_free,
                        None,
                        "monitor",
                    );
                }
                st.low_disk = low;
                let cap = capacity(&cfg, snap.cpu_count);
                st.slots.capacity = cap;
                st.slots.jobs_per_slot = (snap.cpu_count / cap).max(1);
                st.slots.active.retain(|l| pid_alive(l.pid));
                let now = now_unix();
                st.slots
                    .waiting
                    .retain(|w| pid_alive(w.pid) && now.saturating_sub(w.last_poll) < 30);
                if !newly.is_empty() {
                    let mut by_ws: BTreeMap<String, usize> = BTreeMap::new();
                    let mut seen = HashSet::new();
                    for p in snap
                        .workspaces
                        .iter()
                        .flat_map(|w| w.procs.iter())
                        .chain(snap.top.iter())
                    {
                        if newly.contains(&p.pid) && seen.insert(p.pid) {
                            *by_ws.entry(p.group.clone()).or_default() += 1;
                        }
                    }
                    let named: usize = by_ws.values().sum();
                    for (label, n) in by_ws {
                        st.log(
                            "throttle",
                            format!("Lowered priority of {n} build process(es) under pressure"),
                            0,
                            Some(label),
                            "optimizer",
                        );
                    }
                    if newly.len() > named {
                        st.log(
                            "throttle",
                            format!(
                                "Lowered priority of {} build process(es)",
                                newly.len() - named
                            ),
                            0,
                            None,
                            "optimizer",
                        );
                    }
                }
            }

            if cfg.auto_reclaim
                && snap.disk_free_gb() < cfg.low_disk_gb as f64
                && last_reclaim.elapsed() > Duration::from_secs(600)
            {
                last_reclaim = Instant::now();
                let need = ((cfg.auto_reclaim_target_gb as f64 * 1e9) as u64)
                    .saturating_sub(snap.disk_free);
                self.reclaim_stale(need, "optimizer");
            }

            std::thread::sleep(Duration::from_secs(SAMPLE_SECS).saturating_sub(tick.elapsed()));
        }
    }

    fn scan_loop(&self) {
        let mut last = Instant::now() - Duration::from_secs(86_400);
        loop {
            let cfg = self.config();
            let due = last.elapsed() > Duration::from_secs(cfg.scan_interval_mins.max(1) * 60);
            if self.scan_requested.swap(false, Ordering::SeqCst) || due {
                self.run_scan();
                if cfg.auto_sweep_worktrees && due {
                    self.sweep_worktrees("optimizer");
                }
                last = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn busy(&self) -> Vec<PathBuf> {
        let busy = self.state.lock().unwrap().snapshot.busy_paths();
        if busy.is_empty() {
            Monitor::new().sample().busy_paths()
        } else {
            busy
        }
    }

    pub fn run_scan(&self) -> ScanReport {
        let cfg = {
            let mut st = self.state.lock().unwrap();
            st.scanning = true;
            st.config.clone()
        };
        let busy = self.busy();
        let report = reclaim::scan(&cfg, &busy);
        self.state.lock().unwrap().scan = Some(report.clone());
        let map = diskmap::scan(&cfg, &busy);
        let mut st = self.state.lock().unwrap();
        st.diskmap = Some(map);
        st.scanning = false;
        report
    }

    pub fn clean_paths(&self, paths: &[PathBuf], source: &str) -> Vec<CleanResult> {
        let cfg = self.config();
        let busy = self.state.lock().unwrap().snapshot.busy_paths();
        let mut out = Vec::new();
        for p in paths {
            if let Err(e) = reclaim::verify_cleanable(p, &cfg) {
                out.push(CleanResult {
                    path: p.clone(),
                    freed: 0,
                    error: Some(e),
                });
                continue;
            }
            let proj = p.parent().unwrap_or(p);
            let root = reclaim::git_root(proj)
                .map(|r| r.0)
                .unwrap_or_else(|| proj.to_path_buf());
            if busy.contains(&root) {
                out.push(CleanResult {
                    path: p.clone(),
                    freed: 0,
                    error: Some("an agent or build is running in this project".into()),
                });
                continue;
            }
            out.push(reclaim::clean(p, &cfg));
        }
        let freed: u64 = out.iter().map(|r| r.freed).sum();
        let ok: Vec<&CleanResult> = out.iter().filter(|r| r.error.is_none()).collect();
        let mut st = self.state.lock().unwrap();
        if !ok.is_empty() {
            let what = if ok.len() == 1 {
                short_path(&ok[0].path)
            } else {
                format!("{} build caches", ok.len())
            };
            st.log("reclaim", format!("Cleared {what}"), freed, None, source);
        }
        let done: HashSet<PathBuf> = ok.iter().map(|r| r.path.clone()).collect();
        if let Some(scan) = st.scan.as_mut() {
            scan.items.retain(|i| !done.contains(&i.path));
            scan.total_bytes = scan.items.iter().map(|i| i.bytes).sum();
            scan.stale_bytes = scan.items.iter().filter(|i| i.stale).map(|i| i.bytes).sum();
        }
        out
    }

    pub fn reclaim_stale(&self, want_bytes: u64, source: &str) -> Vec<CleanResult> {
        let existing = self.state.lock().unwrap().scan.clone();
        let scan = match existing {
            Some(s) => s,
            None => self.run_scan(),
        };
        let mut picked = Vec::new();
        let mut sum = 0u64;
        for item in scan.items.iter().filter(|i| i.stale) {
            if want_bytes > 0 && sum >= want_bytes {
                break;
            }
            sum += item.bytes;
            picked.push(item.path.clone());
        }
        self.clean_paths(&picked, source)
    }

    pub fn sweep_plan(&self) -> Vec<diskmap::Worktree> {
        let st = self.state.lock().unwrap();
        let busy = st.snapshot.busy_paths();
        st.diskmap
            .as_ref()
            .map(|m| {
                m.worktrees
                    .iter()
                    .filter(|w| w.sweepable && !busy.contains(&w.path))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn sweep_worktrees(&self, source: &str) -> Vec<RemoveResult> {
        let paths: Vec<PathBuf> = self.sweep_plan().into_iter().map(|w| w.path).collect();
        self.remove_worktrees(&paths, true, source)
    }

    pub fn prune_cache(&self, key: &str, source: &str) -> PruneResult {
        let r = diskmap::prune_cache(key);
        if r.ok {
            self.log(
                "reclaim",
                format!("Pruned {key} cache"),
                r.freed,
                None,
                source,
            );
            if let Some(m) = self.state.lock().unwrap().diskmap.as_mut() {
                if let Some(c) = m.categories.iter_mut().find(|c| c.key == key) {
                    c.bytes = c.bytes.saturating_sub(r.freed);
                }
            }
        }
        r
    }

    pub fn remove_worktrees(
        &self,
        paths: &[PathBuf],
        backup_dirty: bool,
        source: &str,
    ) -> Vec<RemoveResult> {
        let (map, busy) = {
            let st = self.state.lock().unwrap();
            (
                st.diskmap.clone().unwrap_or_default(),
                st.snapshot.busy_paths(),
            )
        };
        let mut out = Vec::new();
        for p in paths {
            let Some(wt) = map.worktrees.iter().find(|w| &w.path == p) else {
                out.push(RemoveResult {
                    path: p.clone(),
                    ok: false,
                    freed: 0,
                    message: "not a known worktree; rescan first".into(),
                    backup: None,
                });
                continue;
            };
            let mut wt = wt.clone();
            if busy.contains(&wt.path) {
                wt.in_use = true;
            }
            let r = diskmap::remove_worktree(&wt, backup_dirty);
            if r.ok {
                self.log(
                    "worktree",
                    format!("Removed worktree ({})", r.message),
                    r.freed,
                    Some(format!("{} › {}", wt.repo_name, wt.name)),
                    source,
                );
            }
            out.push(r);
        }
        let done: HashSet<PathBuf> = out
            .iter()
            .filter(|r| r.ok)
            .map(|r| r.path.clone())
            .collect();
        if let Some(m) = self.state.lock().unwrap().diskmap.as_mut() {
            m.worktrees.retain(|w| !done.contains(&w.path));
            m.worktree_bytes = m.worktrees.iter().map(|w| w.bytes).sum();
        }
        out
    }

    pub fn workspace_action(
        &self,
        path: &Path,
        action: WorkspaceAction,
        source: &str,
    ) -> Result<usize, String> {
        let (ws, nice) = {
            let st = self.state.lock().unwrap();
            let ws = st
                .snapshot
                .workspaces
                .iter()
                .find(|w| w.path == path)
                .cloned()
                .ok_or("workspace has no running processes")?;
            (ws, st.config.build_nice)
        };
        let me = std::process::id();
        let mut done = 0;
        for p in ws
            .procs
            .iter()
            .filter(|p| p.own && p.pid != me && p.pid > 1)
        {
            let ok = match action {
                WorkspaceAction::Pause => signal(p.pid, libc::SIGSTOP),
                WorkspaceAction::Resume => signal(p.pid, libc::SIGCONT),
                WorkspaceAction::Deprioritize => renice(p.pid, nice.max(10)),
                WorkspaceAction::StopBuilds => {
                    p.kind == ProcKind::Build && signal(p.pid, libc::SIGTERM)
                }
                WorkspaceAction::KillAll => {
                    signal(p.pid, libc::SIGCONT);
                    signal(p.pid, libc::SIGTERM)
                }
            };
            if ok {
                done += 1;
            }
        }
        let (kind, detail) = match action {
            WorkspaceAction::Pause => ("pause", format!("Paused {done} process(es)")),
            WorkspaceAction::Resume => ("resume", format!("Resumed {done} process(es)")),
            WorkspaceAction::Deprioritize => (
                "throttle",
                format!("Lowered priority of {done} process(es)"),
            ),
            WorkspaceAction::StopBuilds => {
                ("stop", format!("Sent SIGTERM to {done} build process(es)"))
            }
            WorkspaceAction::KillAll => ("stop", format!("Sent SIGTERM to all {done} process(es)")),
        };
        self.log(kind, detail, 0, Some(ws.label.clone()), source);
        Ok(done)
    }

    pub fn process_action(&self, pid: u32, action: ProcAction, source: &str) -> Result<(), String> {
        if pid <= 1 || pid == std::process::id() {
            return Err("refusing to signal this process".into());
        }
        let info = self
            .state
            .lock()
            .unwrap()
            .snapshot
            .find_proc(pid)
            .cloned()
            .ok_or("unknown pid; it may have exited")?;
        if !info.own {
            return Err("process belongs to another user".into());
        }
        let ok = match action {
            ProcAction::Term => signal(pid, libc::SIGTERM),
            ProcAction::Kill => signal(pid, libc::SIGKILL),
            ProcAction::Stop => signal(pid, libc::SIGSTOP),
            ProcAction::Cont => signal(pid, libc::SIGCONT),
            ProcAction::Renice { nice } => unsafe {
                libc::setpriority(
                    libc::PRIO_PROCESS as _,
                    pid as libc::id_t,
                    nice.clamp(0, 20),
                ) == 0
            },
        };
        if !ok {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let (kind, detail) = match action {
            ProcAction::Term => ("stop", format!("SIGTERM → {} ({pid})", info.name)),
            ProcAction::Kill => ("stop", format!("SIGKILL → {} ({pid})", info.name)),
            ProcAction::Stop => ("pause", format!("SIGSTOP → {} ({pid})", info.name)),
            ProcAction::Cont => ("resume", format!("SIGCONT → {} ({pid})", info.name)),
            ProcAction::Renice { nice } => (
                "throttle",
                format!("renice {} → {} ({pid})", nice.clamp(0, 20), info.name),
            ),
        };
        self.log(kind, detail, 0, Some(info.group), source);
        Ok(())
    }

    pub fn advice(&self) -> Advice {
        let st = self.state.lock().unwrap();
        let s = &st.snapshot;
        let cfg = &st.config;
        let free = st.slots.capacity.saturating_sub(st.slots.active.len());
        let mut reasons = Vec::new();
        if free == 0 {
            reasons.push(format!("all {} build slots are taken", st.slots.capacity));
        }
        if s.mem_free_pct() < cfg.throttle_mem_free_pct {
            reasons.push(format!("memory is {:.0}% free", s.mem_free_pct()));
        }
        if s.cpu_count > 0 && s.load[0] > s.cpu_count as f64 * 2.0 {
            reasons.push(format!(
                "load average {:.0} on {} cores",
                s.load[0], s.cpu_count
            ));
        }
        if s.disk_free_gb() < cfg.low_disk_gb as f64 {
            reasons.push(format!("only {:.0} GB disk free", s.disk_free_gb()));
        }
        Advice {
            health: st.health.clone(),
            ok_to_start_heavy_work: reasons.is_empty(),
            free_slots: free,
            suggested_jobs: st.slots.jobs_per_slot.max(1),
            reasons,
        }
    }

    pub fn acquire(&self, label: &str, pid: u32, ticket: Option<u64>) -> LeaseReply {
        let mut st = self.state.lock().unwrap();
        let now = now_unix();
        let jobs = st.slots.jobs_per_slot.max(1);
        let id = match ticket {
            Some(t) if st.slots.active.iter().any(|l| l.id == t) => {
                return LeaseReply {
                    granted: true,
                    ticket: t,
                    jobs,
                    position: 0,
                    reason: "already granted".into(),
                };
            }
            Some(t) if st.slots.waiting.iter().any(|w| w.id == t) => t,
            _ => {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                st.slots.waiting.push(Waiter {
                    id,
                    label: label.into(),
                    pid,
                    since_unix: now,
                    last_poll: now,
                });
                id
            }
        };
        if let Some(w) = st.slots.waiting.iter_mut().find(|w| w.id == id) {
            w.last_poll = now;
        }
        let position = st
            .slots
            .waiting
            .iter()
            .position(|w| w.id == id)
            .unwrap_or(0);
        let cap = st.slots.capacity.max(1);
        let mem_ok = st.snapshot.mem_total == 0
            || st.snapshot.mem_free_pct() >= st.config.throttle_mem_free_pct
            || st.slots.active.is_empty();
        if position == 0 && st.slots.active.len() < cap && mem_ok {
            let w = st.slots.waiting.remove(0);
            st.slots.active.push(Lease {
                id,
                label: w.label,
                pid: w.pid,
                since_unix: now,
                jobs,
            });
            let reason = format!("slot {}/{}", st.slots.active.len(), cap);
            return LeaseReply {
                granted: true,
                ticket: id,
                jobs,
                position: 0,
                reason,
            };
        }
        let reason = if !mem_ok {
            "waiting for memory pressure to drop".to_string()
        } else if position > 0 {
            format!("{position} job(s) ahead in the queue")
        } else {
            format!("all {cap} build slots are busy")
        };
        LeaseReply {
            granted: false,
            ticket: id,
            jobs,
            position,
            reason,
        }
    }

    pub fn release(&self, ticket: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        let before = st.slots.active.len() + st.slots.waiting.len();
        st.slots.active.retain(|l| l.id != ticket);
        st.slots.waiting.retain(|w| w.id != ticket);
        before != st.slots.active.len() + st.slots.waiting.len()
    }
}

fn forecast(h: &VecDeque<Sample>, cfg: &Config) -> Forecast {
    let pts: Vec<(f64, f64)> = h.iter().map(|s| (s.t as f64, s.disk_free as f64)).collect();
    if pts.len() < 20 {
        return Forecast::default();
    }
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let num: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if den == 0.0 {
        return Forecast::default();
    }
    let per_hour = -(num / den) * 3600.0;
    let free = pts.last().map(|p| p.1).unwrap_or(0.0);
    let low = cfg.low_disk_gb as f64 * 1e9;
    if per_hour > 1e8 {
        Forecast {
            bytes_per_hour: per_hour,
            hours_to_low: Some(((free - low) / per_hour).max(0.0)),
            hours_to_full: Some(free / per_hour),
        }
    } else {
        Forecast {
            bytes_per_hour: per_hour,
            hours_to_low: None,
            hours_to_full: None,
        }
    }
}

fn pid_alive(pid: u32) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn signal(pid: u32, sig: i32) -> bool {
    unsafe { libc::kill(pid as i32, sig) == 0 }
}

fn renice(pid: u32, nice: i32) -> bool {
    unsafe {
        *errno() = 0;
        let cur = libc::getpriority(libc::PRIO_PROCESS as _, pid as libc::id_t);
        if *errno() != 0 || cur >= nice {
            return false;
        }
        libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, nice) == 0
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut libc::c_int {
    libc::__error()
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno() -> *mut libc::c_int {
    libc::__errno_location()
}

fn short_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(i) = s.find("/.claude/worktrees/") {
        let repo = s[..i].rsplit('/').next().unwrap_or("");
        let wt = s[i + "/.claude/worktrees/".len()..]
            .split('/')
            .next()
            .unwrap_or("");
        return format!("{repo} › {wt} cache");
    }
    let parts: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    match parts.len() {
        n if n >= 2 => format!("{} cache", parts[n - 2]),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_queue_in_order_and_release() {
        let e = Engine::new(Config {
            max_heavy_jobs: 1,
            ..Config::default()
        });
        {
            let mut st = e.state.lock().unwrap();
            st.slots.capacity = 1;
            st.slots.jobs_per_slot = 4;
            st.snapshot.mem_total = 100;
            st.snapshot.mem_available = 90;
        }
        let me = std::process::id();
        let a = e.acquire("a", me, None);
        assert!(a.granted);
        let b = e.acquire("b", me, None);
        assert!(!b.granted);
        let c = e.acquire("c", me, None);
        assert_eq!(c.position, 1);
        assert!(e.release(a.ticket));
        let c2 = e.acquire("c", me, Some(c.ticket));
        assert!(!c2.granted);
        let b2 = e.acquire("b", me, Some(b.ticket));
        assert!(b2.granted);
        assert_eq!(b2.jobs, 4);
    }
}
