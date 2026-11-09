use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, ConfigError, LoadedConfig};
use crate::diskmap::{self, DiskMap, PruneResult};
pub use crate::journal::Action;
use crate::journal::Journal;
use crate::monitor::{now_unix, Monitor, ProcKind, Snapshot};
use crate::paths::Paths;
use crate::process::{own_cwds, ProcessSource, SystemProcesses};
use crate::reclaim::{self, ScanReport};
use crate::safety::{
    pid_alive, ActionKind, ActionResult, Gate, Outcome, Plan, Request, Signal, SkipReason,
};

const HISTORY_LEN: usize = 600;
const SAMPLE_SECS: u64 = 3;
/// Activity entries kept in memory and loaded from the journal on start.
const ACTIONS_KEPT: usize = 500;

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
    /// Why the config file was not used, if it was not. While set, the engine runs
    /// on defaults with every automatic destructive behaviour off.
    pub config_error: Option<String>,
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
    fn push(&mut self, a: Action) {
        self.actions.push_front(a);
        self.actions.truncate(ACTIONS_KEPT);
    }

    /// Drop finished items from the cached scans so the UI stops offering them.
    fn forget(&mut self, results: &[ActionResult]) {
        let done: HashSet<PathBuf> = results
            .iter()
            .filter(|r| r.is_done())
            .map(|r| PathBuf::from(&r.target))
            .collect();
        if done.is_empty() {
            return;
        }
        if let Some(scan) = self.scan.as_mut() {
            scan.items.retain(|i| !done.contains(&i.path));
            scan.total_bytes = scan.items.iter().map(|i| i.bytes).sum();
            scan.stale_bytes = scan.items.iter().filter(|i| i.stale).map(|i| i.bytes).sum();
        }
        if let Some(m) = self.diskmap.as_mut() {
            m.worktrees.retain(|w| !done.contains(&w.path));
            m.worktree_bytes = m.worktrees.iter().map(|w| w.bytes).sum();
            m.sweepable_bytes = m
                .worktrees
                .iter()
                .filter(|w| w.sweepable)
                .map(|w| w.bytes)
                .sum();
        }
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

/// Bytes automatic reclaim should free: up to `auto_reclaim_target_gb` of free
/// space. `None` when nothing is needed. Never "everything".
pub fn auto_reclaim_need(cfg: &Config, disk_free: u64) -> Option<u64> {
    let target = cfg.auto_reclaim_target_gb.saturating_mul(1_000_000_000);
    let need = target.saturating_sub(disk_free);
    (need > 0).then_some(need)
}

#[derive(Clone)]
pub struct Engine {
    pub state: Arc<Mutex<State>>,
    history: Arc<Mutex<VecDeque<Sample>>>,
    scan_requested: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
    paths: Arc<Paths>,
    journal: Arc<Journal>,
    procs: Arc<dyn ProcessSource>,
}

impl Engine {
    /// An engine for this user, from the environment's paths and config file.
    pub fn from_env() -> Self {
        let paths = Paths::from_env();
        let loaded = Config::load_from(&paths);
        Self::new(paths, loaded)
    }

    pub fn new(paths: Paths, loaded: LoadedConfig) -> Self {
        Self::with_processes(paths, loaded, Arc::new(SystemProcesses::new()))
    }

    /// An engine reading the process table from `procs` (tests use a fake).
    pub fn with_processes(
        paths: Paths,
        loaded: LoadedConfig,
        procs: Arc<dyn ProcessSource>,
    ) -> Self {
        let journal = Journal::new(&paths);
        let actions: VecDeque<Action> = journal.tail(ACTIONS_KEPT).into_iter().rev().collect();
        let state = State {
            version: env!("CARGO_PKG_VERSION").into(),
            config: loaded.config,
            config_error: loaded.error.map(|e| e.to_string()),
            snapshot: Snapshot::default(),
            scan: None,
            diskmap: None,
            scanning: false,
            health: Health::Good,
            forecast: Forecast::default(),
            slots: Slots::default(),
            actions,
            throttled: 0,
            low_disk: false,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY_LEN))),
            scan_requested: Arc::new(AtomicBool::new(true)),
            next_id: Arc::new(AtomicU64::new(1)),
            paths: Arc::new(paths),
            journal: Arc::new(journal),
            procs,
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
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

    pub fn config_error(&self) -> Option<String> {
        self.state.lock().unwrap().config_error.clone()
    }

    /// Validate, save atomically, then apply. A file that failed to load is moved
    /// aside, never overwritten (see [`Config::save_to`]).
    pub fn set_config(&self, cfg: Config) -> Result<(), ConfigError> {
        cfg.save_to(&self.paths, now_unix())?;
        {
            let mut st = self.state.lock().unwrap();
            st.config = cfg;
            st.config_error = None;
        }
        self.request_scan();
        Ok(())
    }

    pub fn request_scan(&self) {
        self.scan_requested.store(true, Ordering::SeqCst);
    }

    /// Record an activity entry in memory and in the journal.
    pub fn log(
        &self,
        kind: &str,
        detail: String,
        bytes: u64,
        workspace: Option<String>,
        source: &str,
    ) {
        self.record(Action::note(
            now_unix(),
            kind,
            detail,
            bytes,
            workspace,
            source,
        ));
    }

    fn record(&self, a: Action) {
        let _ = self.journal.append(&a);
        self.state.lock().unwrap().push(a);
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

    /// Automatic destructive behaviour is allowed only with a config that loaded
    /// cleanly.
    fn automation_allowed(&self) -> bool {
        self.state.lock().unwrap().config_error.is_none()
    }

    fn monitor_loop(&self) {
        let mut mon = Monitor::with_home(self.paths.home.clone());
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

            let mut notes = Vec::new();
            {
                let mut st = self.state.lock().unwrap();
                st.snapshot = snap.clone();
                st.health = health;
                st.forecast = forecast;
                st.throttled = reniced.len();
                let low = snap.disk_total > 0 && snap.disk_free_gb() < cfg.alert_below_gb as f64;
                if low && !st.low_disk {
                    notes.push(Action::note(
                        now_unix(),
                        "alert",
                        format!("Free space below {} GB", cfg.alert_below_gb),
                        snap.disk_free,
                        None,
                        "monitor",
                    ));
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
                        notes.push(Action::note(
                            now,
                            "throttle",
                            format!("Lowered priority of {n} build process(es) under pressure"),
                            0,
                            Some(label),
                            "optimizer",
                        ));
                    }
                    if newly.len() > named {
                        notes.push(Action::note(
                            now,
                            "throttle",
                            format!(
                                "Lowered priority of {} build process(es)",
                                newly.len() - named
                            ),
                            0,
                            None,
                            "optimizer",
                        ));
                    }
                }
            }
            for n in notes {
                self.record(n);
            }

            if cfg.auto_reclaim
                && self.automation_allowed()
                && snap.disk_free_gb() < cfg.low_disk_gb as f64
                && last_reclaim.elapsed() > Duration::from_secs(600)
            {
                last_reclaim = Instant::now();
                if let Some(need) = auto_reclaim_need(&cfg, snap.disk_free) {
                    self.reclaim_stale(Some(need), "optimizer");
                }
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
                if cfg.auto_sweep_worktrees && due && self.automation_allowed() {
                    self.sweep_worktrees("optimizer");
                }
                last = Instant::now();
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    pub fn run_scan(&self) -> ScanReport {
        let cfg = {
            let mut st = self.state.lock().unwrap();
            st.scanning = true;
            st.config.clone()
        };
        let cwds = own_cwds(&*self.procs);
        let now = now_unix();
        let report = reclaim::scan(&cfg, &self.paths, &cwds, now);
        self.state.lock().unwrap().scan = Some(report.clone());
        let map = diskmap::scan(&cfg, &self.paths, &cwds, now);
        let mut st = self.state.lock().unwrap();
        st.diskmap = Some(map);
        st.scanning = false;
        report
    }

    fn gate_run<R>(&self, f: impl FnOnce(&Gate) -> R) -> R {
        let cfg = self.config();
        let gate =
            Gate::new(&cfg, &self.paths, &*self.procs, now_unix()).with_journal(&self.journal);
        f(&gate)
    }

    /// Check requests against the facts now, without acting.
    pub fn plan(&self, requests: &[Request]) -> Plan {
        self.gate_run(|g| g.plan(requests))
    }

    /// Execute approved requests through the safety gate. Each is re-checked now.
    pub fn execute(&self, requests: &[Request], source: &str) -> Vec<ActionResult> {
        let now = now_unix();
        let results = self.gate_run(|g| g.execute(requests, source));
        let mut st = self.state.lock().unwrap();
        for (r, req) in results.iter().zip(requests) {
            let sig = match req {
                Request::Signal { signal, .. } => Some(*signal),
                _ => None,
            };
            st.push(r.to_action(now, source, sig));
        }
        st.forget(&results);
        results
    }

    fn reject(
        &self,
        action: ActionKind,
        target: String,
        detail: &str,
        source: &str,
    ) -> ActionResult {
        let r = ActionResult {
            action,
            target,
            outcome: Outcome::Skipped {
                reason: SkipReason::UnknownTarget,
                detail: detail.into(),
            },
        };
        self.record(r.to_action(now_unix(), source, None));
        r
    }

    /// Delete specific caches (no staleness requirement).
    pub fn clean_paths(&self, paths: &[PathBuf], source: &str) -> Vec<ActionResult> {
        let reqs: Vec<Request> = paths
            .iter()
            .map(|p| Request::DeleteCache {
                path: p.clone(),
                require_stale: false,
            })
            .collect();
        self.execute(&reqs, source)
    }

    /// Stale caches to delete, re-checked now. `None` means every stale cache.
    pub fn reclaim_plan(&self, want_bytes: Option<u64>) -> Plan {
        let existing = self.state.lock().unwrap().scan.clone();
        let scan = match existing {
            Some(s) => s,
            None => self.run_scan(),
        };
        self.gate_run(|g| g.plan_reclaim(&scan.items, want_bytes))
    }

    /// Build a reclaim plan and execute its ready items.
    pub fn reclaim_stale(&self, want_bytes: Option<u64>, source: &str) -> Vec<ActionResult> {
        let plan = self.reclaim_plan(want_bytes);
        self.execute(&plan.approved(), source)
    }

    /// Idle agent worktrees a sweep would remove, re-checked now.
    pub fn sweep_plan(&self) -> Plan {
        let existing = self.state.lock().unwrap().diskmap.clone();
        let map = match existing {
            Some(m) => m,
            None => {
                self.run_scan();
                self.state
                    .lock()
                    .unwrap()
                    .diskmap
                    .clone()
                    .unwrap_or_default()
            }
        };
        self.gate_run(|g| g.plan_sweep(&map.worktrees))
    }

    /// Build a sweep plan and execute its ready items.
    pub fn sweep_worktrees(&self, source: &str) -> Vec<ActionResult> {
        let plan = self.sweep_plan();
        self.execute(&plan.approved(), source)
    }

    pub fn prune_cache(&self, key: &str, source: &str) -> PruneResult {
        let r = diskmap::prune_cache(&self.paths, key);
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

    /// Remove worktrees by path. The expected HEAD is the one in the last scan
    /// (what the caller saw); a worktree whose directory is gone is pruned instead.
    pub fn remove_worktrees(
        &self,
        paths: &[PathBuf],
        allow_dirty: bool,
        source: &str,
    ) -> Vec<ActionResult> {
        let map = self
            .state
            .lock()
            .unwrap()
            .diskmap
            .clone()
            .unwrap_or_default();
        let mut out = Vec::new();
        for p in paths {
            let Some(wt) = map.worktrees.iter().find(|w| &w.path == p) else {
                out.push(self.reject(
                    ActionKind::RemoveWorktree,
                    p.display().to_string(),
                    "not a known worktree; rescan first",
                    source,
                ));
                continue;
            };
            let req = if wt.missing {
                Request::PruneWorktree {
                    repo: wt.repo.clone(),
                    path: wt.path.clone(),
                }
            } else {
                Request::RemoveWorktree {
                    path: wt.path.clone(),
                    expected_head: wt.head.clone().unwrap_or_default(),
                    allow_dirty,
                    require_idle: false,
                }
            };
            out.extend(self.execute(std::slice::from_ref(&req), source));
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
        let targets: Vec<_> = ws
            .procs
            .iter()
            .filter(|p| p.own && p.pid != me && p.pid > 1)
            .collect();
        if action == WorkspaceAction::Deprioritize {
            let done = targets
                .iter()
                .filter(|p| renice(p.pid, nice.max(10)))
                .count();
            self.log(
                "throttle",
                format!("Lowered priority of {done} process(es)"),
                0,
                Some(ws.label.clone()),
                source,
            );
            return Ok(done);
        }
        let mut reqs = Vec::new();
        for p in &targets {
            let sigs: &[Signal] = match action {
                WorkspaceAction::Pause => &[Signal::Stop],
                WorkspaceAction::Resume => &[Signal::Cont],
                WorkspaceAction::StopBuilds if p.kind == ProcKind::Build => &[Signal::Term],
                WorkspaceAction::StopBuilds => &[],
                WorkspaceAction::KillAll => &[Signal::Cont, Signal::Term],
                WorkspaceAction::Deprioritize => &[],
            };
            for s in sigs {
                reqs.push(Request::Signal {
                    pid: p.pid,
                    signal: *s,
                    expected_start_time: p.start_time,
                });
            }
        }
        let results = self.execute(&reqs, source);
        let last_sig = match action {
            WorkspaceAction::Pause => Signal::Stop,
            WorkspaceAction::Resume => Signal::Cont,
            _ => Signal::Term,
        };
        let done = results
            .iter()
            .zip(&reqs)
            .filter(|(r, q)| {
                r.is_done() && matches!(q, Request::Signal { signal, .. } if *signal == last_sig)
            })
            .count();
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
        let signal = match action {
            ProcAction::Term => Signal::Term,
            ProcAction::Kill => Signal::Kill,
            ProcAction::Stop => Signal::Stop,
            ProcAction::Cont => Signal::Cont,
            ProcAction::Renice { nice } => {
                let ok = unsafe {
                    libc::setpriority(
                        libc::PRIO_PROCESS as _,
                        pid as libc::id_t,
                        nice.clamp(0, 20),
                    ) == 0
                };
                if !ok {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                self.log(
                    "throttle",
                    format!("renice {} → {} ({pid})", nice.clamp(0, 20), info.name),
                    0,
                    Some(info.group),
                    source,
                );
                return Ok(());
            }
        };
        let req = Request::Signal {
            pid,
            signal,
            expected_start_time: info.start_time,
        };
        let r = self.execute(std::slice::from_ref(&req), source);
        match r.first().map(|r| &r.outcome) {
            Some(Outcome::Done { .. }) => Ok(()),
            Some(Outcome::Skipped { detail, .. }) => Err(detail.clone()),
            None => Err("no result".into()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::FakeProcesses;

    fn engine(cfg: Config) -> (tempfile::TempDir, Engine) {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let e = Engine::with_processes(
            paths,
            LoadedConfig {
                config: cfg,
                error: None,
            },
            Arc::new(FakeProcesses::empty()),
        );
        (t, e)
    }

    #[test]
    fn slots_queue_in_order_and_release() {
        let (_t, e) = engine(Config {
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

    #[test]
    fn auto_reclaim_never_means_everything() {
        let cfg = Config {
            low_disk_gb: 100,
            auto_reclaim_target_gb: 60,
            ..Config::default()
        };
        // The audit's case: free space already above the target.
        assert_eq!(auto_reclaim_need(&cfg, 80_000_000_000), None);
        assert_eq!(auto_reclaim_need(&cfg, 60_000_000_000), None);
        assert_eq!(
            auto_reclaim_need(&cfg, 50_000_000_000),
            Some(10_000_000_000)
        );
        // And such a config does not validate in the first place.
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_error_turns_automation_off_and_is_in_state() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), b"{ broken").unwrap();
        let loaded = Config::load_from(&paths);
        let e = Engine::with_processes(paths.clone(), loaded, Arc::new(FakeProcesses::empty()));
        let st = e.snapshot_state();
        assert!(st.config_error.is_some());
        assert!(!st.config.auto_reclaim && !st.config.auto_sweep_worktrees);
        assert!(!e.automation_allowed());
        e.set_config(Config::default()).unwrap();
        assert!(e.config_error().is_none() && e.automation_allowed());
    }

    #[test]
    fn activity_survives_a_restart() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let mk = || {
            Engine::with_processes(
                paths.clone(),
                LoadedConfig {
                    config: Config::default(),
                    error: None,
                },
                Arc::new(FakeProcesses::empty()),
            )
        };
        let e = mk();
        e.log("alert", "one".into(), 0, None, "test");
        let r = e.clean_paths(&[PathBuf::from("relative/target")], "test");
        assert_eq!(r[0].reason(), Some(SkipReason::NotAbsolute));
        let e2 = mk();
        let st = e2.snapshot_state();
        assert_eq!(st.actions.len(), 2);
        assert_eq!(st.actions[0].status.as_deref(), Some("skipped"));
        assert_eq!(st.actions[1].detail, "one");
    }
}
