//! The engine: a sample loop (every 3 s: machine snapshot, throttling, leases,
//! alerts) and a scan loop (caches and worktrees, plus every destructive job),
//! both supervised, sharing one [`State`].
//!
//! Destructive work never runs on the sample loop. The sample loop asks for it
//! over a job channel ([`Job`]) and the scan loop carries it out through the
//! safety gate.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{Config, ConfigError, LoadedConfig};
use crate::consumers::{GroupKind, ProcSample};
use crate::diskmap::{self, DiskMap, PruneResult};
pub use crate::journal::Action;
use crate::journal::Journal;
use crate::lock;
use crate::monitor::{now_unix, KindInfo, Monitor, ProcKind, Snapshot};
use crate::paths::Paths;
use crate::process::{own_cwds, ProcessSource, SystemProcesses};
use crate::procplan::{draft_group, draft_pid, ProcessPlan, ProcessPlanRequest};
use crate::reclaim::{self, ScanReport};
use crate::safety::{pid_alive, ActionResult, Gate, Outcome, Plan, Request, Signal};
use crate::watch::{self, Finding, FindingCounts, WatchProbe};
use serde_json::{json, Value};

#[path = "engine_sessions.rs"]
mod sessions;
use sessions::Supervision;

#[path = "engine_hook.rs"]
mod hook;
pub use hook::{GuardCheckReply, GuardCheckRequest, HookEvent, HookReply, HookRequest};

#[path = "engine_updates.rs"]
mod updates_glue;
pub use sessions::{
    LimitsStatus, PausedProc, SessionBrief, SessionDetail, SessionMember, SessionsState,
    PAUSED_FILE, STATE_SESSIONS_KEPT,
};
use updates_glue::Updates;
pub use updates_glue::UpdatesStatus;

const HISTORY_LEN: usize = 600;
pub const SAMPLE_SECS: u64 = 3;
/// Health reads `stale` when the last sample is older than this.
pub const STALE_AFTER_SECS: u64 = 15;
/// Activity entries kept in memory and loaded from the journal on start.
const ACTIONS_KEPT: usize = 500;
/// Automatic reclaim runs at most this often.
const RECLAIM_COOLDOWN: Duration = Duration::from_secs(600);
/// The scan loop wakes at least this often to check whether a scan is due.
const SCAN_POLL: Duration = Duration::from_secs(2);
/// Longest lease TTL a caller may ask for.
pub const MAX_LEASE_TTL_SECS: u64 = 86_400;
/// A waiter that has not polled for this long leaves the queue, whatever its
/// TTL: the TTL is the lease's length once granted, not a licence to hold the
/// head of the queue without polling.
pub const WAITER_POLL_SECS: u64 = 30;
/// A supervised loop that ran this long before panicking restarts without
/// backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A non-holder engine compares the config file with its copy this often.
const CONFIG_RECHECK: Duration = Duration::from_secs(1);

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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Good,
    Busy,
    Strained,
    Critical,
    /// No sample in the last [`STALE_AFTER_SECS`]: the figures are old.
    Stale,
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
    /// The process the lease belongs to; it ends when that process exits.
    pub pid: Option<u32>,
    pub since_unix: u64,
    pub jobs: usize,
    /// The lease ends unless renewed (the same ticket sent again) within this.
    pub ttl_secs: Option<u64>,
    pub expires_unix: Option<u64>,
    /// The start time of `pid` when the lease was taken, when the process
    /// table knows it; a reused pid does not keep the lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    /// Returned once, to the acquirer; `/v1/release` must present it. Never
    /// serialized into state.
    #[serde(skip)]
    pub release_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Waiter {
    pub id: u64,
    pub label: String,
    pub pid: Option<u32>,
    pub since_unix: u64,
    pub last_poll: u64,
    pub ttl_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    /// Returned once, to the acquirer; `/v1/release` must present it. Never
    /// serialized into state.
    #[serde(skip)]
    pub release_secret: Option<String>,
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
    pub expires_unix: Option<u64>,
    /// Only in the reply that created the ticket: present it to release the
    /// lease (`POST /v1/release {ticket, secret}`). Keep it private.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_secret: Option<String>,
}

/// Why a release was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseError {
    /// The lease has a release secret and none was sent.
    SecretRequired,
    /// The secret sent is not the lease's.
    WrongSecret,
}

impl ReleaseError {
    pub fn code(self) -> &'static str {
        match self {
            ReleaseError::SecretRequired => "release_secret_required",
            ReleaseError::WrongSecret => "wrong_release_secret",
        }
    }
}

/// A process the caller previewed: act on it only while it is still the
/// process that started at `expected_start_time`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct PidRef {
    pub pid: u32,
    pub expected_start_time: u64,
}

/// A build-slot request. A lease needs a live `pid`, a `ttl_secs`, or both.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LeaseRequest {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub ticket: Option<u64>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// With `ticket`: the `release_secret` from the reply that created it.
    /// Renewing or polling a ticket that has a secret requires it.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    NeedsPidOrTtl,
    /// Renewing or polling a ticket with a release secret needs the secret.
    SecretRequired,
    /// The secret sent is not the ticket's.
    WrongSecret,
    PidNotAlive(u32),
    TtlOutOfRange(u64),
    /// pid 1 (or 0), or a process of another user: its lifetime says nothing
    /// about the caller's build.
    PidNotAllowed(u32),
    /// No random secret could be made for a new ticket; no ticket was made.
    SecretUnavailable(String),
}

impl LeaseError {
    pub fn code(&self) -> &'static str {
        match self {
            LeaseError::NeedsPidOrTtl => "lease_needs_pid_or_ttl",
            LeaseError::PidNotAlive(_) => "pid_not_alive",
            LeaseError::TtlOutOfRange(_) => "ttl_out_of_range",
            LeaseError::PidNotAllowed(_) => "pid_not_allowed",
            LeaseError::SecretRequired => "release_secret_required",
            LeaseError::WrongSecret => "wrong_release_secret",
            LeaseError::SecretUnavailable(_) => "secret_unavailable",
        }
    }
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::NeedsPidOrTtl => write!(
                f,
                "a lease needs the pid of a live process or a ttl_secs (renew by sending the ticket again)"
            ),
            LeaseError::PidNotAlive(p) => write!(f, "process {p} is not running"),
            LeaseError::TtlOutOfRange(t) => {
                write!(f, "ttl_secs {t} is out of range 1..={MAX_LEASE_TTL_SECS}")
            }
            LeaseError::PidNotAllowed(p) => write!(
                f,
                "process {p} is pid 1 or belongs to another user; lease a process of your own"
            ),
            LeaseError::SecretRequired => write!(
                f,
                "this ticket has a release secret; send it as \"secret\" to renew or poll"
            ),
            LeaseError::WrongSecret => write!(f, "the secret sent is not this ticket's"),
            LeaseError::SecretUnavailable(e) => {
                write!(f, "could not make a release secret for a new ticket: {e}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Advice {
    pub health: Health,
    pub ok_to_start_heavy_work: bool,
    pub free_slots: usize,
    pub suggested_jobs: usize,
    pub reasons: Vec<String>,
}

/// One supervised loop's restarts.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LoopStatus {
    pub restarts: u32,
    pub last_panic: Option<String>,
    pub last_panic_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Loops {
    pub sample: LoopStatus,
    pub scan: LoopStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopName {
    Sample,
    Scan,
}

/// What `/v1/health` reports, read without cloning the whole state.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSummary {
    pub name: &'static str,
    pub version: String,
    pub health: Health,
    pub stale: bool,
    pub sample_age_secs: Option<u64>,
    pub disk_free: u64,
    pub disk_total: u64,
    pub cpu_pct: f32,
    pub load: [f64; 3],
    pub mem_free_pct: f32,
    pub agents: usize,
    pub builds: usize,
    pub free_slots: usize,
    pub ok_to_start_heavy_work: bool,
    pub reclaimable_stale: u64,
    pub restarts: u32,
    /// Why the action journal could not be written, while that is so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_error: Option<String>,
    /// Journal entries that could not be written since the engine started.
    pub journal_lost: u64,
    /// Seconds since the last completed cache scan; `None` before the first.
    pub scan_age_secs: Option<u64>,
    /// The last scan is older than twice the scan interval (plus 10 minutes):
    /// the scan loop may be stuck, and scan figures are old.
    pub scan_stale: bool,
    /// Open watch findings by severity (see `GET /v1/findings`).
    pub findings: FindingCounts,
    /// Agent sessions running now (see `GET /v1/sessions`).
    pub sessions_running: usize,
    /// Limit breaches not yet cleared (see `GET /v1/limits/status`).
    pub breaches_active: usize,
    /// Open update and advisory findings (see `GET /v1/updates`).
    pub updates_available: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub version: String,
    pub config: Config,
    /// Why the config file was not used, if it was not. While set, the engine runs
    /// on defaults with every automatic destructive behaviour off.
    pub config_error: Option<String>,
    /// Shared with readers; a poll clones a pointer, not the process list.
    pub snapshot: Arc<Snapshot>,
    /// Seconds since the last sample; `None` before the first one.
    pub sample_age_secs: Option<u64>,
    pub scan: Option<Arc<ScanReport>>,
    pub diskmap: Option<Arc<DiskMap>>,
    pub scanning: bool,
    pub health: Health,
    pub forecast: Forecast,
    pub slots: Slots,
    pub actions: VecDeque<Action>,
    pub throttled: usize,
    pub low_disk: bool,
    pub loops: Loops,
    /// Automatic reclaim, sweeping and throttling run only when true: the config
    /// loaded cleanly and this engine holds the engine lock.
    pub automation: bool,
    /// Why the action journal could not be written, while that is so. Actions
    /// still run; their results carry `journal_error` too.
    pub journal_error: Option<String>,
    /// Journal entries that could not be written since the engine started.
    pub journal_lost: u64,
    /// Build processes whose original priority could not be restored.
    pub restore_failed: usize,
    /// Seconds since the last completed cache scan; `None` before the first.
    pub scan_age_secs: Option<u64>,
    /// See [`HealthSummary::scan_stale`].
    pub scan_stale: bool,
    /// Watch findings, most severe first; dismissed ones are included with
    /// `dismissed: true`. Empty while watch is off or before its first run.
    pub findings: Vec<Finding>,
    pub finding_counts: FindingCounts,
    /// When the watch detectors last ran.
    pub findings_unix: Option<u64>,
    /// Agent sessions, limit breaches and the activity record, compact.
    pub sessions: SessionsState,
    /// Update checks, compact (the full report: `GET /v1/updates`).
    pub updates: UpdatesStatus,
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
            let scan = Arc::make_mut(scan);
            scan.items.retain(|i| !done.contains(&i.path));
            scan.total_bytes = scan.items.iter().map(|i| i.bytes).sum();
            scan.stale_bytes = scan.items.iter().filter(|i| i.stale).map(|i| i.bytes).sum();
        }
        if let Some(m) = self.diskmap.as_mut() {
            let m = Arc::make_mut(m);
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

    /// (seconds since the last scan, stale)
    fn scan_age(&self, now: u64) -> (Option<u64>, bool) {
        let Some(at) = self.scan.as_ref().map(|s| s.at_unix).filter(|a| *a > 0) else {
            return (None, false);
        };
        let age = now.saturating_sub(at);
        let limit = self.config.scan_interval_mins.max(1) * 60 * 2 + 600;
        (Some(age), age > limit)
    }

    fn sample_age(&self, now: u64) -> Option<u64> {
        (self.snapshot.at_unix > 0).then(|| now.saturating_sub(self.snapshot.at_unix))
    }

    /// Health as of `now`: stale when no sample is recent.
    fn health_at(&self, now: u64) -> Health {
        match self.sample_age(now) {
            Some(a) if a <= STALE_AFTER_SECS => self.health,
            _ => Health::Stale,
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

/// The real update sources; in this crate's tests none (no test reaches
/// the network or runs a version command unless it passes fakes).
fn update_sources(home: PathBuf) -> Updates {
    if cfg!(test) {
        let _ = home;
        Updates::new(
            Arc::new(crate::updates::NoNetwork),
            Arc::new(crate::updates::NoCommands),
        )
    } else {
        Updates::new(
            crate::updates::default_http(),
            Arc::new(crate::updates::SystemRunner { home }),
        )
    }
}

/// RFC 7396 JSON merge patch.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Some(p) = patch.as_object() else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    if let Some(t) = target.as_object_mut() {
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
}

/// Findings the watch run produces (every category but limits and updates,
/// which the sample loop and the update checker keep).
fn watch_owned(f: &Finding) -> bool {
    !matches!(
        f.kind.category(),
        watch::FindingCategory::Limit | watch::FindingCategory::Update
    )
}

/// Lock a mutex, recovering the data if a panicking thread poisoned it. Every
/// update the engine makes leaves the state consistent between statements, so
/// a panic elsewhere never makes it unusable.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Work the scan loop does on request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Scan,
    /// Delete stale caches until about `want_bytes` are free.
    Reclaim {
        want_bytes: u64,
        source: String,
    },
    /// Remove idle agent worktrees.
    Sweep {
        source: String,
    },
}

#[derive(Default)]
struct Jobs {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

impl Jobs {
    /// Queue `job` unless one of the same kind is already waiting.
    fn push(&self, job: Job) -> bool {
        let mut q = lock(&self.queue);
        let same = |j: &Job| std::mem::discriminant(j) == std::mem::discriminant(&job);
        if q.iter().any(same) {
            return false;
        }
        q.push_back(job);
        self.ready.notify_all();
        true
    }

    fn pop_wait(&self, wait: Duration) -> Option<Job> {
        let mut q = lock(&self.queue);
        if q.is_empty() && !wait.is_zero() {
            q = self
                .ready
                .wait_timeout(q, wait)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        q.pop_front()
    }

    fn pending(&self) -> Vec<Job> {
        lock(&self.queue).iter().cloned().collect()
    }
}

/// A build process Mulchie lowered the priority of, and what it was before.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reniced {
    start_time: u64,
    original: i32,
    restore_failed: bool,
}

/// What the sample loop keeps between ticks.
#[derive(Default)]
pub struct SampleCtx {
    monitor: Option<Monitor>,
    last_reclaim: Option<Instant>,
}

/// What the scan loop keeps between ticks.
#[derive(Default)]
pub struct ScanCtx {
    last_scan: Option<Instant>,
    last_watch: Option<Instant>,
}

/// What the watch detectors keep between runs.
#[derive(Default)]
struct WatchMem {
    tracker: watch::Tracker,
    sigs: watch::SigCache,
    /// Quarantine flags already read, so each run reads the next ones.
    quarantine: watch::FlagCache,
    /// Listening sockets from the last run, and when.
    listeners: Option<Vec<watch::Listener>>,
    listeners_unix: Option<u64>,
    last_notify: Option<u64>,
}

/// New code-signature checks per watch run (each up to 2 s).
const SIG_CHECKS_PER_RUN: usize = 12;
/// Loose executables checked for the quarantine flag per run.
const QUARANTINE_CHECKS_PER_RUN: usize = 200;
/// At most one notification for new high-severity findings per this long.
const FINDING_NOTIFY_SECS: u64 = 600;
/// Findings journaled one by one per run; the rest are summed up.
const FINDINGS_JOURNALED: usize = 20;

/// A child process in [`ProcessDetail`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChildInfo {
    pub pid: u32,
    pub name: String,
    pub start_time: u64,
}

/// `GET /v1/process/<pid>`: everything Mulchie knows about one process.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessDetail {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Runs as this user.
    pub own: bool,
    pub exe: Option<PathBuf>,
    /// argv with secrets redacted; empty for other users' processes.
    pub cmd: Vec<String>,
    /// Working directory; `None` for other users' processes or when unreadable.
    pub cwd: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub workspace_label: Option<String>,
    /// The consumer group key (`app:…`, `workspace:…`, `process:…`, `system`).
    pub group: Option<String>,
    pub kind: Option<ProcKind>,
    pub start_time: u64,
    pub age_secs: Option<u64>,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub stopped: bool,
    /// Parent, grandparent, … up to pid 1, nearest first.
    pub parents: Vec<watch::ParentInfo>,
    pub children: Vec<ChildInfo>,
    /// Listening TCP sockets as of the last watch run (`listening_unix`);
    /// `None` when unknown.
    pub listening: Option<Vec<String>>,
    pub listening_unix: Option<u64>,
    /// macOS code signature, when the watch has checked it.
    pub signature: Option<watch::SigStatus>,
    /// The executable carries the download quarantine flag (macOS).
    pub quarantined: Option<bool>,
    /// Why Mulchie will not signal it, when it will not.
    pub protected: Option<String>,
    /// Watch findings about this process (same pid and start time).
    pub findings: Vec<Finding>,
}

/// The config file's bytes, `None` when there is none or it cannot be read.
fn read_config_bytes(paths: &Paths) -> Option<Vec<u8>> {
    std::fs::read(paths.config_file()).ok()
}

/// A6: the revision of the config file's content: 64-bit FNV-1a of its
/// bytes in hex (no file hashes as no bytes). Stable across engines and
/// versions; it only tells contents apart.
pub fn config_revision_of(bytes: Option<&[u8]>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes.unwrap_or_default() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[derive(Clone)]
pub struct Engine {
    state: Arc<Mutex<State>>,
    history: Arc<Mutex<VecDeque<Sample>>>,
    jobs: Arc<Jobs>,
    next_id: Arc<AtomicU64>,
    paths: Arc<Paths>,
    journal: Arc<Journal>,
    procs: Arc<dyn ProcessSource>,
    shutdown: Arc<AtomicBool>,
    automation_enabled: Arc<AtomicBool>,
    notify: Arc<AtomicBool>,
    reniced: Arc<Mutex<HashMap<u32, Reniced>>>,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    backoff_ms: Arc<AtomicU64>,
    watch: Arc<Mutex<WatchMem>>,
    probe: Arc<dyn WatchProbe>,
    /// The port the API serves on, 0 when it does not.
    api_port: Arc<AtomicU64>,
    /// The config file's bytes as this engine last loaded or wrote them
    /// (`None`: no file). Held across every config read-modify-write.
    config_disk: Arc<Mutex<Option<Vec<u8>>>>,
    /// When a non-holder last compared the config file with its copy.
    config_checked: Arc<Mutex<Option<Instant>>>,
    /// Sessions, limits and limit actions in flight (lock after `state`,
    /// never while holding it).
    sup: Arc<Mutex<Supervision>>,
    /// Wakes the limits worker.
    limit_wake: Arc<Condvar>,
    /// Held across a limit pause (record, SIGSTOP, update) and by shutdown
    /// before it resumes everything.
    pause_lock: Arc<Mutex<()>>,
    /// The activity record writer, started on first use.
    record: Arc<std::sync::OnceLock<crate::record::Record>>,
    /// Update checks: sources, cache and the worker's state.
    updates: Arc<Updates>,
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
        let config_disk = read_config_bytes(&paths);
        let home_for_runner = paths.home.clone();
        let actions: VecDeque<Action> = journal.tail(ACTIONS_KEPT).into_iter().rev().collect();
        let config_error = loaded.error.map(|e| e.to_string());
        let state = State {
            version: env!("CARGO_PKG_VERSION").into(),
            config: loaded.config,
            automation: config_error.is_none(),
            config_error,
            snapshot: Arc::new(Snapshot::default()),
            sample_age_secs: None,
            scan: None,
            diskmap: None,
            scanning: false,
            health: Health::Good,
            forecast: Forecast::default(),
            slots: Slots::default(),
            actions,
            throttled: 0,
            low_disk: false,
            loops: Loops::default(),
            journal_error: None,
            journal_lost: 0,
            restore_failed: 0,
            scan_age_secs: None,
            scan_stale: false,
            findings: Vec::new(),
            finding_counts: FindingCounts::default(),
            findings_unix: None,
            sessions: SessionsState::default(),
            updates: UpdatesStatus::default(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            history: Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY_LEN))),
            jobs: Arc::new(Jobs::default()),
            next_id: Arc::new(AtomicU64::new(1)),
            paths: Arc::new(paths),
            journal: Arc::new(journal),
            procs,
            shutdown: Arc::new(AtomicBool::new(false)),
            automation_enabled: Arc::new(AtomicBool::new(true)),
            notify: Arc::new(AtomicBool::new(false)),
            reniced: Arc::new(Mutex::new(HashMap::new())),
            threads: Arc::new(Mutex::new(Vec::new())),
            backoff_ms: Arc::new(AtomicU64::new(1000)),
            watch: Arc::new(Mutex::new(WatchMem::default())),
            probe: Arc::new(watch::SystemProbe),
            api_port: Arc::new(AtomicU64::new(0)),
            config_disk: Arc::new(Mutex::new(config_disk)),
            config_checked: Arc::new(Mutex::new(None)),
            sup: Arc::new(Mutex::new(Supervision::default())),
            limit_wake: Arc::new(Condvar::new()),
            pause_lock: Arc::new(Mutex::new(())),
            record: Arc::new(std::sync::OnceLock::new()),
            updates: Arc::new(update_sources(home_for_runner)),
        }
    }

    /// Use `probe` for listening sockets, signatures and quarantine flags
    /// (tests use a fake).
    pub fn with_watch_probe(mut self, probe: Arc<dyn WatchProbe>) -> Self {
        self.probe = probe;
        self
    }

    /// Record the port the API serves on, for the posture check.
    pub fn set_api_port(&self, port: u16) {
        self.api_port.store(port as u64, Ordering::SeqCst);
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Take the per-user engine lock. On [`lock::LockError::Held`] another
    /// engine runs; the caller should act as its client (see
    /// [`crate::api::Client`]) or call [`Engine::set_automation_enabled`] with
    /// `false` before starting.
    pub fn acquire_lock(&self) -> Result<lock::EngineLock, lock::LockError> {
        lock::EngineLock::acquire(&self.paths, now_unix())
    }

    /// The whole state with `sample_age_secs` and a stale health filled in.
    pub fn snapshot_state(&self) -> State {
        let now = now_unix();
        let mut st = lock(&self.state).clone();
        st.sample_age_secs = st.sample_age(now);
        st.health = st.health_at(now);
        st.automation = st.automation && self.automation_allowed();
        st.journal_error = self.journal.last_error();
        st.journal_lost = self.journal.lost();
        (st.scan_age_secs, st.scan_stale) = st.scan_age(now);
        st.restore_failed = lock(&self.reniced)
            .values()
            .filter(|r| r.restore_failed)
            .count();
        st.sessions = self.sessions_state(&st.config);
        st.updates = self.updates_status(false);
        st
    }

    /// The figures `/v1/health` reports, without cloning scans or processes.
    pub fn health_summary(&self) -> HealthSummary {
        let now = now_unix();
        let adv = self.advice();
        let (sessions_running, breaches_active) = self.session_counts();
        let updates_available = self.updates_available();
        let st = lock(&self.state);
        let s = &st.snapshot;
        let health = st.health_at(now);
        HealthSummary {
            name: "mulchie",
            version: st.version.clone(),
            stale: health == Health::Stale,
            health,
            sample_age_secs: st.sample_age(now),
            disk_free: s.disk_free,
            disk_total: s.disk_total,
            cpu_pct: s.cpu_pct,
            load: s.load,
            mem_free_pct: s.mem_free_pct(),
            agents: s.agent_count,
            builds: s.build_count,
            free_slots: adv.free_slots,
            ok_to_start_heavy_work: adv.ok_to_start_heavy_work,
            reclaimable_stale: st.scan.as_ref().map(|s| s.stale_bytes).unwrap_or(0),
            restarts: st.loops.sample.restarts + st.loops.scan.restarts,
            journal_error: self.journal.last_error(),
            journal_lost: self.journal.lost(),
            scan_age_secs: st.scan_age(now).0,
            scan_stale: st.scan_age(now).1,
            findings: st.finding_counts,
            sessions_running,
            breaches_active,
            updates_available,
        }
    }

    /// Replace the machine snapshot (the CLI samples once without loops).
    pub fn set_snapshot(&self, snap: Snapshot) {
        let cfg = self.config();
        let mut st = lock(&self.state);
        st.health = health_of(&snap, &cfg);
        st.snapshot = Arc::new(snap);
    }

    pub fn history(&self) -> Vec<Sample> {
        lock(&self.history).iter().cloned().collect()
    }

    pub fn config(&self) -> Config {
        self.refresh_config_if_changed();
        lock(&self.state).config.clone()
    }

    /// The config and the revision of the file content it came from, read
    /// together (A6).
    pub fn config_and_revision(&self) -> (Config, String) {
        self.refresh_config_if_changed();
        let disk = lock(&self.config_disk);
        let rev = config_revision_of(disk.as_deref());
        let cfg = lock(&self.state).config.clone();
        (cfg, rev)
    }

    /// With `expected`, refuse ([`ConfigError::Changed`]) unless the file's
    /// content now is that revision. Called with the config lock held.
    fn check_revision(
        &self,
        disk: &mut Option<Vec<u8>>,
        now_bytes: &Option<Vec<u8>>,
        expected: Option<&str>,
    ) -> Result<(), ConfigError> {
        let Some(exp) = expected else {
            return Ok(());
        };
        let now = config_revision_of(now_bytes.as_deref());
        if now == exp.trim() {
            return Ok(());
        }
        if now_bytes != disk {
            self.adopt_disk_config(disk, now_bytes.clone());
        }
        Err(ConfigError::Changed(format!(
            "the config is at revision {now}, not {}; read it again (GET /v1/config) and reapply the change",
            exp.trim()
        )))
    }

    /// Only the engine that holds the engine lock writes the config file.
    fn may_write_config(&self) -> Result<(), ConfigError> {
        if self.automation_enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Take whatever the running engine wrote, so a caller that reads
        // again sees it.
        let mut disk = lock(&self.config_disk);
        let now_bytes = read_config_bytes(&self.paths);
        if now_bytes != *disk {
            self.adopt_disk_config(&mut disk, now_bytes);
        }
        Err(ConfigError::NotLockHolder(
            "another Mulchie engine holds the engine lock and owns the config; send the change to it (PUT or PATCH /v1/config)".into(),
        ))
    }

    /// An engine that does not hold the lock never writes the config; it
    /// takes the file's content whenever the running engine changed it
    /// (checked at most once a second, one small file read).
    fn refresh_config_if_changed(&self) {
        if self.automation_enabled.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut at = lock(&self.config_checked);
            if at.is_some_and(|t| t.elapsed() < CONFIG_RECHECK) {
                return;
            }
            *at = Some(Instant::now());
        }
        // A writer holds it: it adopts the file itself.
        let Ok(mut disk) = self.config_disk.try_lock() else {
            return;
        };
        let now_bytes = read_config_bytes(&self.paths);
        if now_bytes != *disk {
            self.adopt_disk_config(&mut disk, now_bytes);
        }
    }

    /// Apply a JSON merge patch (RFC 7396: objects merge, `null` removes a
    /// key so it takes its default, anything else replaces) to the config
    /// file's current content, validate, save and use it. Keys the patch
    /// does not name keep their value, so clients never send the whole file.
    pub fn patch_config(&self, patch: &Value) -> Result<Config, ConfigError> {
        self.patch_config_if(patch, None).map(|(c, _)| c)
    }

    /// [`Engine::patch_config`], only while the file is at revision
    /// `expected` (when given). Returns the config and its new revision.
    pub fn patch_config_if(
        &self,
        patch: &Value,
        expected: Option<&str>,
    ) -> Result<(Config, String), ConfigError> {
        if !patch.is_object() {
            return Err(ConfigError::Parse(
                "a config patch is a JSON object of the keys to change".into(),
            ));
        }
        self.may_write_config()?;
        let mut disk = lock(&self.config_disk);
        let now_bytes = read_config_bytes(&self.paths);
        self.check_revision(&mut disk, &now_bytes, expected)?;
        if now_bytes != *disk {
            // A patch names only what it changes: apply it to the file's
            // current content rather than refusing.
            self.adopt_disk_config(&mut disk, now_bytes);
        }
        if let Some(e) = self.config_error() {
            return Err(ConfigError::Invalid(vec![format!(
                "the config file is not usable ({e}); replace it with PUT /v1/config"
            )]));
        }
        let mut v = serde_json::to_value(lock(&self.state).config.clone())
            .map_err(|e| ConfigError::Io(e.to_string()))?;
        merge_patch(&mut v, patch);
        let cfg: Config =
            serde_json::from_value(v).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if let Some(k) = crate::config::unknown_key(patch, &cfg) {
            return Err(ConfigError::UnknownKey(k));
        }
        self.save_config_locked(&mut disk, cfg.clone())?;
        Ok((cfg, config_revision_of(disk.as_deref())))
    }

    pub fn config_error(&self) -> Option<String> {
        lock(&self.state).config_error.clone()
    }

    /// Validate, save atomically, then apply. A file that failed to load is moved
    /// aside, never overwritten (see [`Config::save_to`]).
    ///
    /// Refused with [`ConfigError::Changed`] when the file changed on disk
    /// since this engine loaded or wrote it (another engine, an editor): the
    /// engine then takes the file's content, so a caller reads the config
    /// again and reapplies its change.
    pub fn set_config(&self, cfg: Config) -> Result<(), ConfigError> {
        self.set_config_if(cfg, None).map(|_| ())
    }

    /// [`Engine::set_config`], only while the file is at revision `expected`
    /// (when given). Returns the new revision.
    pub fn set_config_if(
        &self,
        cfg: Config,
        expected: Option<&str>,
    ) -> Result<String, ConfigError> {
        self.may_write_config()?;
        let mut disk = lock(&self.config_disk);
        let now_bytes = read_config_bytes(&self.paths);
        self.check_revision(&mut disk, &now_bytes, expected)?;
        if now_bytes != *disk {
            self.adopt_disk_config(&mut disk, now_bytes);
            return Err(ConfigError::Changed(format!(
                "{} was changed by something else since Mulchie read it; read the config again and reapply the change",
                self.paths.config_file().display()
            )));
        }
        self.save_config_locked(&mut disk, cfg)?;
        Ok(config_revision_of(disk.as_deref()))
    }

    /// Take the file's current content as the engine's config, as a start
    /// would. Called with the config lock held.
    fn adopt_disk_config(&self, disk: &mut Option<Vec<u8>>, bytes: Option<Vec<u8>>) {
        *disk = bytes;
        let loaded = Config::load_from(&self.paths);
        let mut st = lock(&self.state);
        st.config_error = loaded.error.as_ref().map(|e| e.to_string());
        st.automation = st.config_error.is_none();
        st.config = loaded.config;
    }

    /// Save and use `cfg`. Called with the config lock held.
    fn save_config_locked(
        &self,
        disk: &mut Option<Vec<u8>>,
        cfg: Config,
    ) -> Result<(), ConfigError> {
        cfg.save_to(&self.paths, now_unix())?;
        *disk = read_config_bytes(&self.paths);
        {
            let mut st = lock(&self.state);
            st.config = cfg;
            st.config_error = None;
            st.automation = true;
        }
        self.request_scan();
        Ok(())
    }

    /// Queue a scan on the scan loop. False when one is already queued.
    pub fn request_scan(&self) -> bool {
        self.jobs.push(Job::Scan)
    }

    /// Jobs waiting for the scan loop.
    pub fn pending_jobs(&self) -> Vec<Job> {
        self.jobs.pending()
    }

    /// Turn automatic reclaim, sweeping and throttling on or off for this
    /// engine. A second engine (the app while `mulchie serve` holds the lock)
    /// runs with it off.
    pub fn set_automation_enabled(&self, on: bool) {
        self.automation_enabled.store(on, Ordering::SeqCst);
    }

    /// Show desktop notifications for alerts and automatic actions (headless
    /// `serve`; the app shows its own).
    pub fn enable_notifications(&self) {
        self.notify.store(true, Ordering::SeqCst);
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
        if a.kind == "alert" && self.notify.load(Ordering::SeqCst) {
            crate::notify::notify("Mulchie", &a.detail);
        }
        let _ = self.journal.append(&a);
        lock(&self.state).push(a);
    }

    // ------------------------------------------------------------- lifecycle

    /// Start the supervised sample and scan loops.
    pub fn start(&self) {
        let mut threads = lock(&self.threads);
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-sample".into())
            .spawn(move || me.supervise(LoopName::Sample, SampleCtx::default, Engine::sample_tick))
        {
            threads.push(h);
        }
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-scan".into())
            .spawn(move || {
                me.supervise(LoopName::Scan, ScanCtx::default, |e, c| {
                    e.scan_tick(c, SCAN_POLL)
                })
            })
        {
            threads.push(h);
        }
        self.start_limits_worker(&mut threads);
        self.start_updates(&mut threads);
    }

    pub fn is_shut_down(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Stop the loops, wait up to `wait` for them (a scan in progress finishes
    /// its current step first), restore build priorities Mulchie lowered and
    /// resume processes it paused for a limit.
    pub fn shutdown(&self, wait: Duration) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.jobs.ready.notify_all();
        self.limit_wake.notify_all();
        self.updates.wake.notify_all();
        let handles: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        let deadline = Instant::now() + wait;
        for h in handles {
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        self.restore_priorities("shutdown");
        {
            // A pause still running finishes first; none starts after.
            let _pausing = lock(&self.pause_lock);
            self.resume_all_paused("shutdown");
        }
        self.flush_record();
    }

    /// Sleep up to `d`, returning early on shutdown.
    fn nap(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.is_shut_down() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    /// Run `tick` until shutdown. A panic is caught, logged and counted in
    /// `state.loops`, and the loop restarts with fresh context after a backoff
    /// that doubles per consecutive failure (up to a minute) and resets after a
    /// minute of healthy running.
    pub(crate) fn supervise<C>(
        &self,
        name: LoopName,
        make: impl Fn() -> C,
        mut tick: impl FnMut(&Engine, &mut C),
    ) {
        let mut failures: u32 = 0;
        while !self.is_shut_down() {
            let started = Instant::now();
            let r = catch_unwind(AssertUnwindSafe(|| {
                let mut ctx = make();
                while !self.is_shut_down() {
                    tick(self, &mut ctx);
                }
            }));
            let Err(panic) = r else { break };
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            if started.elapsed() >= HEALTHY_RUN {
                failures = 0;
            }
            failures = failures.saturating_add(1);
            let label = match name {
                LoopName::Sample => "sample",
                LoopName::Scan => "scan",
            };
            {
                let mut st = lock(&self.state);
                let ls = match name {
                    LoopName::Sample => &mut st.loops.sample,
                    LoopName::Scan => &mut st.loops.scan,
                };
                ls.restarts += 1;
                ls.last_panic = Some(msg.clone());
                ls.last_panic_unix = Some(now_unix());
            }
            self.log(
                "error",
                format!("{label} loop failed ({msg}); restarting"),
                0,
                None,
                "engine",
            );
            let base = self.backoff_ms.load(Ordering::SeqCst);
            let ms = base.saturating_mul(1u64 << (failures - 1).min(16));
            self.nap(Duration::from_millis(ms).min(MAX_BACKOFF));
        }
    }

    /// Automatic destructive behaviour is allowed only with a config that loaded
    /// cleanly, on the engine that holds the lock.
    fn automation_allowed(&self) -> bool {
        self.automation_enabled.load(Ordering::SeqCst) && lock(&self.state).config_error.is_none()
    }

    // ----------------------------------------------------------- sample loop

    /// One sample-loop iteration: sample, update state, sleep out the period.
    pub fn sample_tick(&self, ctx: &mut SampleCtx) {
        let tick = Instant::now();
        let cfg = self.config();
        let mon = ctx
            .monitor
            .get_or_insert_with(|| Monitor::with_home(self.paths.home.clone()));
        mon.set_roots(&cfg.roots(&self.paths));
        mon.set_extra_agents(&cfg.agents.extra);
        let snap = mon.sample();
        let kinds = mon.process_kinds();
        self.apply_sample(snap, &kinds, ctx);
        self.nap(Duration::from_secs(SAMPLE_SECS).saturating_sub(tick.elapsed()));
    }

    /// Everything a sample changes: history, health, forecast, throttling,
    /// leases, alerts, and a reclaim request when the disk is low.
    pub fn apply_sample(&self, snap: Snapshot, kinds: &[KindInfo], ctx: &mut SampleCtx) {
        let cfg = self.config();
        let now = now_unix();
        let health = health_of(&snap, &cfg);
        let mut ws_top: Vec<(String, f32, u64)> = snap
            .workspaces
            .iter()
            .map(|w| (w.label.clone(), w.cpu_pct, w.mem_bytes))
            .collect();
        ws_top.sort_by(|a, b| b.1.total_cmp(&a.1));
        ws_top.truncate(10);
        let forecast = {
            let mut h = lock(&self.history);
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

        if cfg.watch.enabled {
            lock(&self.watch)
                .tracker
                .observe(now, snap.members.values().flatten());
        }

        let pressured =
            snap.mem_free_pct() < cfg.throttle_mem_free_pct || snap.cpu_pct > cfg.throttle_cpu_pct;
        let automation = self.automation_allowed();
        let newly = if automation && cfg.throttle_builds && pressured {
            self.throttle(kinds, cfg.build_nice)
        } else {
            // Pressure cleared, throttling turned off, or this engine is not
            // the one that throttles: put priorities back.
            self.forget_exited(kinds);
            self.restore_priorities("optimizer");
            Vec::new()
        };
        let throttled = lock(&self.reniced).len();

        self.observe_sessions(
            now,
            &snap.session_procs,
            crate::limits::Machine {
                cores: snap.cpu_count,
                mem_mb: snap.mem_total as f64 / 1_048_576.0,
                strained: matches!(health, Health::Strained | Health::Critical),
            },
            &cfg,
        );

        let mut notes = Vec::new();
        {
            let mut st = lock(&self.state);
            st.health = health;
            st.forecast = forecast;
            st.throttled = throttled;
            let low = snap.disk_total > 0 && snap.disk_free_gb() < cfg.alert_below_gb as f64;
            if low && !st.low_disk {
                notes.push(Action::note(
                    now,
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
            expire_leases(&mut st.slots, now, &|p, s| self.lease_pid_alive(p, s));
            if !newly.is_empty() {
                notes.extend(throttle_notes(&snap, &newly, now));
            }
            st.snapshot = Arc::new(snap);
        }
        for n in notes {
            self.record(n);
        }

        let st_snapshot = lock(&self.state).snapshot.clone();
        if cfg.auto_reclaim
            && automation
            && st_snapshot.disk_total > 0
            && st_snapshot.disk_free_gb() < cfg.low_disk_gb as f64
            && ctx
                .last_reclaim
                .map(|t| t.elapsed() > RECLAIM_COOLDOWN)
                .unwrap_or(true)
        {
            if let Some(need) = auto_reclaim_need(&cfg, st_snapshot.disk_free) {
                ctx.last_reclaim = Some(Instant::now());
                self.jobs.push(Job::Reclaim {
                    want_bytes: need,
                    source: "optimizer".into(),
                });
            }
        }
    }

    /// Lower the priority of the user's own build processes. Returns the pids
    /// newly lowered; each is remembered with its previous priority.
    fn throttle(&self, kinds: &[KindInfo], nice: i32) -> Vec<u32> {
        self.forget_exited(kinds);
        let me = std::process::id();
        let mut rn = lock(&self.reniced);
        let mut newly = Vec::new();
        for k in kinds {
            if !k.own || k.kind != ProcKind::Build || k.pid == me || k.pid <= 1 {
                continue;
            }
            if rn.contains_key(&k.pid) {
                continue;
            }
            // Read now, just before renicing: a pid reused since the sample
            // is not touched.
            if !self.same_own_process(k.pid, k.start_time) {
                continue;
            }
            if let Some(original) = lower_priority(k.pid, nice) {
                rn.insert(
                    k.pid,
                    Reniced {
                        start_time: k.start_time,
                        original,
                        restore_failed: false,
                    },
                );
                newly.push(k.pid);
            }
        }
        newly
    }

    /// `pid` is, read now, still the user's own process that started at
    /// `start_time`.
    fn same_own_process(&self, pid: u32, start_time: u64) -> bool {
        self.procs
            .get(pid)
            .is_some_and(|p| p.uid == Some(self.procs.current_uid()) && p.start_time == start_time)
    }

    /// Stop tracking processes that exited (or whose pid was reused).
    fn forget_exited(&self, kinds: &[KindInfo]) {
        let alive: HashMap<u32, u64> = kinds.iter().map(|k| (k.pid, k.start_time)).collect();
        lock(&self.reniced).retain(|pid, r| alive.get(pid) == Some(&r.start_time));
    }

    /// Put back the priorities Mulchie lowered. Raising priority again usually
    /// needs privilege: a failure is journaled once and the process stays
    /// counted as throttled until it exits.
    fn restore_priorities(&self, source: &str) {
        let (mut restored, mut failed) = (0usize, 0usize);
        {
            let mut rn = lock(&self.reniced);
            rn.retain(|pid, r| {
                // Read now: a pid that exited or was reused is not ours to
                // renice; forget it.
                if !self.same_own_process(*pid, r.start_time) {
                    return false;
                }
                if r.restore_failed {
                    return true;
                }
                if set_nice(*pid, r.original).is_ok() {
                    restored += 1;
                    false
                } else {
                    failed += 1;
                    r.restore_failed = true;
                    true
                }
            });
        }
        if restored > 0 {
            self.log(
                "throttle",
                format!("Restored the priority of {restored} build process(es)"),
                0,
                None,
                source,
            );
        }
        if failed > 0 {
            self.log(
                "throttle",
                format!(
                    "Could not restore the priority of {failed} build process(es); raising priority needs privilege, so they keep the lower priority until they exit"
                ),
                0,
                None,
                source,
            );
        }
    }

    // ------------------------------------------------------------- scan loop

    /// One scan-loop iteration: a periodic scan when due, else the next job
    /// (waiting up to `wait` for one).
    pub fn scan_tick(&self, ctx: &mut ScanCtx, wait: Duration) {
        let cfg = self.config();
        let watch_due = ctx
            .last_watch
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(cfg.watch.interval_secs.max(10)));
        // Wait for the first sample: the detectors need its process kinds.
        if watch_due && lock(&self.state).snapshot.at_unix > 0 {
            self.run_watch();
            ctx.last_watch = Some(Instant::now());
        }
        let interval = Duration::from_secs(cfg.scan_interval_mins.max(1) * 60);
        let due = ctx
            .last_scan
            .map(|t| t.elapsed() >= interval)
            .unwrap_or(true);
        if due {
            let first = ctx.last_scan.is_none();
            self.run_scan();
            ctx.last_scan = Some(Instant::now());
            if !first && cfg.auto_sweep_worktrees && self.automation_allowed() {
                self.jobs.push(Job::Sweep {
                    source: "optimizer".into(),
                });
            }
            return;
        }
        let Some(job) = self.jobs.pop_wait(wait) else {
            return;
        };
        self.run_job(job, ctx);
    }

    fn run_job(&self, job: Job, ctx: &mut ScanCtx) {
        match job {
            Job::Scan => {
                self.run_scan();
                ctx.last_scan = Some(Instant::now());
            }
            Job::Reclaim { want_bytes, source } => {
                if self.automation_allowed() {
                    let r = self.reclaim_now(Some(want_bytes), &source);
                    self.announce(&r, "reclaimed");
                }
            }
            Job::Sweep { source } => {
                if self.automation_allowed() {
                    let r = self.sweep_now(&source);
                    self.announce(&r, "swept");
                }
            }
        }
    }

    fn announce(&self, results: &[ActionResult], verb: &str) {
        let freed: u64 = results.iter().map(|r| r.freed_bytes()).sum();
        if freed > 0 && self.notify.load(Ordering::SeqCst) {
            crate::notify::notify(
                "Mulchie",
                &format!("Automatically {verb} {}", crate::human_bytes(freed)),
            );
        }
    }

    pub fn run_scan(&self) -> ScanReport {
        let cfg = {
            let mut st = lock(&self.state);
            st.scanning = true;
            st.config.clone()
        };
        let cwds = own_cwds(&*self.procs);
        let now = now_unix();
        let report = reclaim::scan(&cfg, &self.paths, &cwds, now);
        lock(&self.state).scan = Some(Arc::new(report.clone()));
        let map = diskmap::scan(&cfg, &self.paths, &cwds, now);
        let mut st = lock(&self.state);
        st.diskmap = Some(Arc::new(map));
        st.scanning = false;
        report
    }

    // ----------------------------------------------------------------- watch

    /// Run the watch detectors now (the scan loop does every
    /// `watch.interval_secs`): read the process table and the probes, turn them
    /// into findings, store them in state, journal what appeared or went away,
    /// and notify about new high-severity ones. Read-only: nothing is acted on.
    pub fn run_watch(&self) -> Vec<Finding> {
        use std::os::unix::fs::MetadataExt;
        let cfg = self.config();
        let now = now_unix();
        if !cfg.watch.enabled {
            let mut st = lock(&self.state);
            st.findings.retain(|f| !watch_owned(f));
            st.finding_counts = FindingCounts::of(&st.findings);
            st.findings_unix = None;
            return Vec::new();
        }
        let procs = self.procs.list();
        let uid = self.procs.current_uid();
        let self_pid = self.procs.self_pid();
        let (snap, config_error, prev) = {
            let st = lock(&self.state);
            (
                st.snapshot.clone(),
                st.config_error.clone(),
                st.findings
                    .iter()
                    .filter(|f| watch_owned(f))
                    .cloned()
                    .collect::<Vec<Finding>>(),
            )
        };
        let samples: HashMap<u32, ProcSample> = snap
            .members
            .values()
            .flatten()
            .map(|p| (p.pid, p.clone()))
            .collect();
        let roots: Vec<PathBuf> = cfg
            .roots(&self.paths)
            .into_iter()
            .map(|r| r.canonicalize().unwrap_or(r))
            .collect();
        let temps = watch::temp_dirs(&self.paths.home);
        let listeners = if cfg.watch.check_listeners {
            self.probe.listeners()
        } else {
            None
        };

        // Signatures: a few new executables per run, outside the watch lock
        // (codesign can take seconds); quarantine flags: one getxattr each.
        let mut sigs = HashMap::new();
        let mut quarantined = HashSet::new();
        if cfg.watch.check_signatures {
            let (known, quar_known): (HashSet<PathBuf>, HashSet<PathBuf>) = {
                let w = lock(&self.watch);
                let own: Vec<PathBuf> = procs
                    .iter()
                    .filter(|p| p.uid == Some(uid))
                    .filter_map(|p| p.exe.clone())
                    .collect();
                (
                    own.iter()
                        .filter(|e| w.sigs.get(e).is_some())
                        .cloned()
                        .collect(),
                    own.iter()
                        .filter(|e| w.quarantine.get(e).is_some())
                        .cloned()
                        .collect(),
                )
            };
            let fresh: Vec<(PathBuf, Option<watch::SigStatus>)> = watch::signature_candidates(
                &procs,
                uid,
                &|p| known.contains(p),
                &temps,
                SIG_CHECKS_PER_RUN,
            )
            .into_iter()
            .map(|e| {
                let s = self.probe.signature(&e);
                (e, s)
            })
            .collect();
            // Loose executables not read yet (or changed since): each run
            // reads the next ones, so every candidate is reached in turn.
            let quar_fresh: Vec<(PathBuf, bool)> = watch::signature_candidates(
                &procs,
                uid,
                &|p| quar_known.contains(p) || crate::consumers::outer_bundle(p).is_some(),
                &temps,
                QUARANTINE_CHECKS_PER_RUN,
            )
            .into_iter()
            .map(|e| {
                let q = self.probe.quarantined(&e);
                (e, q)
            })
            .collect();
            let mut w = lock(&self.watch);
            for (e, q) in quar_fresh {
                w.quarantine.put(&e, q);
            }
            for p in procs.iter().filter(|p| p.uid == Some(uid)) {
                if let Some(e) = &p.exe {
                    if w.quarantine.get(e) == Some(true) {
                        quarantined.insert(e.clone());
                    }
                }
            }
            // Unknown (output codesign gave that is not understood) counts
            // for this run only: it is not cached, so it is read again.
            let mut unknown = HashSet::new();
            for (e, s) in fresh {
                match s {
                    Some(watch::SigStatus::Unknown) => {
                        unknown.insert(e);
                    }
                    Some(s) => w.sigs.put(&e, s),
                    None => {}
                }
            }
            for p in procs.iter().filter(|p| p.uid == Some(uid)) {
                if let Some(e) = &p.exe {
                    if let Some(s) = w.sigs.get(e) {
                        sigs.insert(e.clone(), s);
                    } else if unknown.contains(e) {
                        sigs.insert(e.clone(), watch::SigStatus::Unknown);
                    }
                }
            }
        }

        let mode = |p: &Path| std::fs::metadata(p).ok().map(|m| m.mode());
        let port = self.api_port.load(Ordering::SeqCst);
        let posture = watch::Posture {
            config_dir: self.paths.config_dir.clone(),
            config_dir_mode: mode(&self.paths.config_dir),
            token_modes: [
                crate::api::token_path_in(&self.paths),
                crate::api::read_only_token_path_in(&self.paths),
            ]
            .into_iter()
            .filter_map(|p| mode(&p).map(|m| (p, m)))
            .collect(),
            api_port: (port > 0).then_some(port as u16),
            lock_held_by_other: if self.automation_enabled.load(Ordering::SeqCst) {
                None
            } else {
                lock::running_engine(&self.paths)
                    .map(|h| h.pid)
                    .filter(|p| *p != self_pid)
            },
            config_error,
        };

        // Orphan candidates: who adopted them, and whether a service manager
        // started them (one getsid, and on Linux one cgroup read, each).
        let subreapers: Vec<u32> = procs
            .iter()
            .filter(|p| crate::consumers::is_user_manager(&p.name, &p.cmd))
            .map(|p| p.pid)
            .collect();
        let service_started: HashSet<u32> = procs
            .iter()
            .filter(|p| p.uid == Some(uid) && (p.ppid == 1 || subreapers.contains(&p.ppid)))
            .filter(|p| self.procs.service_started(p.pid))
            .map(|p| p.pid)
            .collect();
        let listeners_unknown = cfg.watch.check_listeners && listeners.is_none();
        let mut raw = {
            let mut w = lock(&self.watch);
            // Unknown listeners keep the last known list and its time.
            if !listeners_unknown {
                w.listeners = listeners.clone();
                w.listeners_unix = listeners.as_ref().map(|_| now);
            }
            let input = watch::WatchInput {
                now,
                cfg: &cfg.watch,
                procs: &procs,
                samples: &samples,
                tracker: &w.tracker,
                listeners: listeners.as_deref(),
                signatures: &sigs,
                quarantined: &quarantined,
                uid,
                self_pid,
                home: &self.paths.home,
                roots: &roots,
                temp_dirs: &temps,
                posture: &posture,
                service_started: &service_started,
                subreapers: &subreapers,
            };
            watch::detect(&input)
        };
        if listeners_unknown {
            let alive: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start_time)).collect();
            watch::carry_listener_findings(&mut raw, &prev, &|p, s| alive.get(&p) == Some(&s));
        }
        let (findings, changes) = watch::finalize(raw, &prev, &cfg.watch, now);
        {
            let mut st = lock(&self.state);
            st.findings.retain(|f| !watch_owned(f));
            st.findings.extend(findings.iter().cloned());
            sessions::sort_findings(&mut st.findings);
            st.finding_counts = FindingCounts::of(&st.findings);
            st.findings_unix = Some(now);
        }
        if let Some(ls) = &listeners {
            let pairs: Vec<(u32, u16)> = ls.iter().map(|l| (l.pid, l.port)).collect();
            lock(&self.sup).tracker.set_listening(&pairs);
        }
        self.journal_findings(&changes, now);
        findings
    }

    /// Journal findings that appeared or went away, and notify about new
    /// high-severity ones (rate limited, only with notifications on).
    fn journal_findings(&self, changes: &watch::Changes, now: u64) {
        let ws = |f: &Finding| {
            f.evidence
                .workspace
                .as_ref()
                .map(|w| w.display().to_string())
        };
        let appeared: Vec<&Finding> = changes.appeared.iter().filter(|f| !f.dismissed).collect();
        for f in appeared.iter().take(FINDINGS_JOURNALED) {
            self.record(Action::note(
                now,
                "finding",
                format!("[{}] {}", f.kind.code(), f.title),
                0,
                ws(f),
                "watch",
            ));
        }
        for f in changes.gone.iter().take(FINDINGS_JOURNALED) {
            self.record(Action::note(
                now,
                "finding",
                format!("No longer seen: {}", f.title),
                0,
                ws(f),
                "watch",
            ));
        }
        let more = appeared.len().saturating_sub(FINDINGS_JOURNALED)
            + changes.gone.len().saturating_sub(FINDINGS_JOURNALED);
        if more > 0 {
            self.record(Action::note(
                now,
                "finding",
                format!("{more} more finding change(s); see GET /v1/findings"),
                0,
                None,
                "watch",
            ));
        }
        let high: Vec<&&Finding> = appeared
            .iter()
            .filter(|f| f.severity == crate::consumers::Severity::High)
            .collect();
        if high.is_empty() || !self.notify.load(Ordering::SeqCst) {
            return;
        }
        let mut w = lock(&self.watch);
        if w.last_notify
            .is_some_and(|t| now.saturating_sub(t) < FINDING_NOTIFY_SECS)
        {
            return;
        }
        w.last_notify = Some(now);
        drop(w);
        let body = if high.len() > 1 {
            format!("{} (and {} more)", high[0].title, high.len() - 1)
        } else {
            high[0].title.clone()
        };
        crate::notify::notify("Mulchie", &body);
    }

    /// Counts of the stored findings and when the watch last ran.
    pub fn finding_summary(&self) -> (FindingCounts, Option<u64>) {
        let st = lock(&self.state);
        (st.finding_counts, st.findings_unix)
    }

    /// Findings from the last watch run, at least `min_severity`, of `kind`.
    pub fn findings(
        &self,
        min_severity: Option<crate::consumers::Severity>,
        kind: Option<watch::FindingKind>,
        include_dismissed: bool,
    ) -> Vec<Finding> {
        let st = lock(&self.state);
        watch::filter(&st.findings, min_severity, kind, include_dismissed)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Hide finding `id` for `days` (default 30, 1 to 3650). Saved in the
    /// config's `watch.dismissed`. Returns when the dismissal ends.
    pub fn dismiss_finding(
        &self,
        id: &str,
        days: Option<u64>,
    ) -> Result<u64, (&'static str, String)> {
        self.dismiss_finding_known(id, days).map(|(until, _)| until)
    }

    /// As [`Engine::dismiss_finding`], also saying whether `id` names a
    /// current finding or one dismissed before (`false`: probably a typo;
    /// the dismissal is saved anyway, for a finding that may come back).
    pub fn dismiss_finding_known(
        &self,
        id: &str,
        days: Option<u64>,
    ) -> Result<(u64, bool), (&'static str, String)> {
        let known = {
            let st = lock(&self.state);
            st.findings.iter().any(|f| f.id == id)
                || st.config.watch.dismissed.iter().any(|d| d.id == id)
        };
        let days = days.unwrap_or(30);
        if !(1..=3650).contains(&days) {
            return Err(("invalid_body", "days must be 1 to 3650".into()));
        }
        if id.is_empty() || id.len() > 512 {
            return Err(("invalid_body", "id must be 1 to 512 characters".into()));
        }
        let until = now_unix() + days * 86_400;
        self.edit_dismissals(|d| {
            d.retain(|x| x.id != id);
            d.push(crate::config::Dismissal {
                id: id.to_string(),
                until_unix: Some(until),
            });
        })?;
        Ok((until, known))
    }

    /// Show finding `id` again. False when it was not dismissed.
    pub fn undismiss_finding(&self, id: &str) -> Result<bool, (&'static str, String)> {
        let mut found = false;
        self.edit_dismissals(|d| {
            let before = d.len();
            d.retain(|x| x.id != id);
            found = d.len() != before;
        })?;
        Ok(found)
    }

    fn edit_dismissals(
        &self,
        f: impl FnOnce(&mut Vec<crate::config::Dismissal>),
    ) -> Result<(), (&'static str, String)> {
        self.may_write_config()
            .map_err(|e| ("not_lock_holder", e.to_string()))?;
        if let Some(e) = self.config_error() {
            return Err((
                "config_not_loaded",
                format!("the config file was not used ({e}); fix it before dismissing findings"),
            ));
        }
        let now = now_unix();
        // One read-modify-write under the config lock. When the file changed
        // on disk since it was read, the edit is applied to the file's
        // current content (merged), not to the engine's older copy.
        let mut disk = lock(&self.config_disk);
        let now_bytes = read_config_bytes(&self.paths);
        if now_bytes != *disk {
            self.adopt_disk_config(&mut disk, now_bytes);
            if let Some(e) = self.config_error() {
                return Err((
                    "config_not_loaded",
                    format!("the config file changed on disk and is not usable ({e}); fix it before dismissing findings"),
                ));
            }
        }
        let mut cfg = self.config();
        watch::prune_dismissed(&mut cfg.watch, now);
        f(&mut cfg.watch.dismissed);
        self.save_config_locked(&mut disk, cfg.clone())
            .map_err(|e| ("invalid_config", e.to_string()))?;
        drop(disk);
        // Apply to the stored findings now, not at the next run.
        let mut st = lock(&self.state);
        for x in st.findings.iter_mut() {
            let d = cfg.watch.dismissed.iter().find(|d| d.id == x.id);
            x.dismissed = d.is_some_and(|d| d.until_unix.is_none_or(|u| u > now));
            x.dismissed_until = d.filter(|_| x.dismissed).and_then(|d| d.until_unix);
        }
        st.finding_counts = FindingCounts::of(&st.findings);
        Ok(())
    }

    /// Everything known about one process, read now. `None` when it does not
    /// exist.
    pub fn process_detail(&self, pid: u32) -> Option<ProcessDetail> {
        let procs = self.procs.list();
        let p = procs.iter().find(|p| p.pid == pid)?;
        let uid = self.procs.current_uid();
        let own = p.uid == Some(uid);
        let now = now_unix();
        let by_pid: HashMap<u32, &crate::process::ProcFacts> =
            procs.iter().map(|q| (q.pid, q)).collect();
        let mut parents = Vec::new();
        let mut cur = p.ppid;
        let mut seen = HashSet::new();
        while cur > 0 && seen.insert(cur) && parents.len() < 64 {
            let Some(q) = by_pid.get(&cur) else { break };
            parents.push(watch::ParentInfo {
                pid: q.pid,
                name: q.name.clone(),
            });
            cur = q.ppid;
        }
        let mut children: Vec<ChildInfo> = procs
            .iter()
            .filter(|q| q.ppid == pid && q.pid != pid)
            .map(|q| ChildInfo {
                pid: q.pid,
                name: q.name.clone(),
                start_time: q.start_time,
            })
            .collect();
        children.sort_by_key(|c| c.pid);
        let (snap, findings) = {
            let st = lock(&self.state);
            (
                st.snapshot.clone(),
                st.findings
                    .iter()
                    .filter(|f| {
                        f.evidence.pid == Some(pid) && f.evidence.start_time == Some(p.start_time)
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let (group, sample) = snap
            .members
            .iter()
            .find_map(|(k, v)| {
                v.iter()
                    .find(|s| s.pid == pid && s.start_time == p.start_time)
                    .map(|s| (k.clone(), s.clone()))
            })
            .unzip();
        let (listening, listening_unix, signature) = {
            let w = lock(&self.watch);
            let l = w.listeners.as_ref().map(|ls| {
                let mut v: Vec<String> = ls
                    .iter()
                    .filter(|l| l.pid == pid)
                    .map(|l| l.shown())
                    .collect();
                v.sort();
                v.dedup();
                v
            });
            let sig = p.exe.as_deref().and_then(|e| w.sigs.get(e));
            (l, w.listeners_unix, sig)
        };
        let quarantined = (own && cfg!(target_os = "macos"))
            .then(|| p.exe.as_deref().map(|e| self.probe.quarantined(e)))
            .flatten();
        let mut protected = crate::consumers::protected_reason(
            pid,
            &p.name,
            p.exe.as_deref(),
            self.procs.self_pid(),
        );
        if protected.is_none() {
            protected = self.gate_run(|g| g.user_service(p));
        }
        if protected.is_none() && !own {
            protected = Some("another user's process".into());
        }
        Some(ProcessDetail {
            pid,
            ppid: p.ppid,
            name: p.name.clone(),
            own,
            exe: p.exe.clone(),
            cmd: if own {
                crate::redact::redact_args(&p.cmd)
            } else {
                Vec::new()
            },
            cwd: if own { p.cwd.clone() } else { None },
            workspace: sample
                .as_ref()
                .and_then(|s| s.workspace.as_ref().map(|w| w.0.clone())),
            workspace_label: sample
                .as_ref()
                .and_then(|s| s.workspace.as_ref().map(|w| w.1.clone())),
            group,
            kind: sample.as_ref().map(|s| s.kind),
            start_time: p.start_time,
            age_secs: (p.start_time > 0).then(|| now.saturating_sub(p.start_time)),
            cpu_pct: sample.as_ref().map(|s| s.cpu_pct),
            mem_bytes: sample.as_ref().map(|s| s.mem_bytes),
            stopped: sample.as_ref().is_some_and(|s| s.stopped),
            parents,
            children,
            listening,
            listening_unix,
            signature,
            quarantined,
            protected,
            findings,
        })
    }

    // ------------------------------------------------------- plan and execute

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

    /// Execute exactly these requests through the safety gate; each is
    /// re-checked now. Nothing else is touched.
    pub fn execute(&self, requests: &[Request], source: &str) -> Vec<ActionResult> {
        let now = now_unix();
        let results = self.gate_run(|g| g.execute(requests, source));
        {
            let mut st = lock(&self.state);
            for (r, req) in results.iter().zip(requests) {
                let sig = match req {
                    Request::Signal { signal, .. } => Some(*signal),
                    _ => None,
                };
                st.push(r.to_action(now, source, sig));
            }
            st.forget(&results);
        }
        self.note_signals(requests, &results);
        results
    }

    fn cache_requests(paths: &[PathBuf]) -> Vec<Request> {
        paths
            .iter()
            .map(|p| Request::DeleteCache {
                path: p.clone(),
                require_stale: false,
            })
            .collect()
    }

    /// Delete exactly these caches (no staleness requirement).
    pub fn clean_paths(&self, paths: &[PathBuf], source: &str) -> Vec<ActionResult> {
        self.execute(&Self::cache_requests(paths), source)
    }

    /// What deleting these caches would do, checked now.
    pub fn clean_plan(&self, paths: &[PathBuf]) -> Plan {
        self.plan(&Self::cache_requests(paths))
    }

    fn cached_scan(&self) -> Arc<ScanReport> {
        let existing = lock(&self.state).scan.clone();
        match existing {
            Some(s) => s,
            None => Arc::new(self.run_scan()),
        }
    }

    fn cached_diskmap(&self) -> Arc<DiskMap> {
        if let Some(m) = lock(&self.state).diskmap.clone() {
            return m;
        }
        self.run_scan();
        lock(&self.state).diskmap.clone().unwrap_or_default()
    }

    /// Stale caches to delete, re-checked now. `None` means every stale cache.
    pub fn reclaim_plan(&self, want_bytes: Option<u64>) -> Plan {
        let scan = self.cached_scan();
        self.gate_run(|g| g.plan_reclaim(&scan.items, want_bytes))
    }

    /// Plan and execute in one step. For the engine's automatic reclaim and an
    /// explicit `{"auto": true}`; interactive callers plan, show, then execute
    /// the approved items.
    pub fn reclaim_now(&self, want_bytes: Option<u64>, source: &str) -> Vec<ActionResult> {
        let plan = self.reclaim_plan(want_bytes);
        self.execute(&plan.approved(), source)
    }

    /// Idle agent worktrees a sweep would remove, re-checked now. Ready items
    /// carry the HEAD read now as their expected HEAD.
    pub fn sweep_plan(&self) -> Plan {
        let map = self.cached_diskmap();
        self.gate_run(|g| g.plan_sweep(&map.worktrees))
    }

    /// Plan and execute a sweep in one step (automatic sweep, `{"auto": true}`).
    pub fn sweep_now(&self, source: &str) -> Vec<ActionResult> {
        let plan = self.sweep_plan();
        self.execute(&plan.approved(), source)
    }

    /// What removing these worktrees would do, checked now. A ready removal
    /// carries the HEAD read now; executing it later fails if HEAD moves. A
    /// path whose directory is gone and that the last scan saw as a missing
    /// worktree becomes a prune.
    pub fn worktree_plan(&self, paths: &[PathBuf], allow_dirty: bool) -> Plan {
        let map = lock(&self.state).diskmap.clone();
        let reqs: Vec<Request> = paths
            .iter()
            .map(|p| {
                let missing = map
                    .as_ref()
                    .and_then(|m| m.worktrees.iter().find(|w| &w.path == p && w.missing));
                match missing {
                    Some(w) if !p.exists() => Request::PruneWorktree {
                        repo: w.repo.clone(),
                        path: w.path.clone(),
                    },
                    _ => Request::RemoveWorktree {
                        path: p.clone(),
                        expected_head: String::new(),
                        allow_dirty,
                        require_idle: false,
                    },
                }
            })
            .collect();
        self.plan(&reqs)
    }

    /// Run a package manager's prune command, refused while that tool runs.
    pub fn prune_cache(&self, key: &str, source: &str) -> PruneResult {
        let r = diskmap::prune_cache(&self.paths, &*self.procs, key);
        let mut a = Action::note(
            now_unix(),
            "reclaim",
            if r.ok {
                format!("Pruned {key} cache")
            } else {
                format!(
                    "Did not prune {key} cache: {}",
                    r.output.lines().last().unwrap_or("")
                )
            },
            r.freed,
            None,
            source,
        );
        a.target = Some(key.to_string());
        a.status = Some(if r.ok { "done" } else { "skipped" }.into());
        a.reason = r.reason;
        self.record(a);
        if r.freed > 0 {
            if let Some(m) = lock(&self.state).diskmap.as_mut() {
                let m = Arc::make_mut(m);
                if let Some(c) = m.categories.iter_mut().find(|c| c.key == key) {
                    c.bytes = c.bytes.saturating_sub(r.freed);
                }
            }
        }
        r
    }

    // -------------------------------------------------------------- processes

    /// A workspace action on exactly the processes the caller previewed: each
    /// `{pid, expected_start_time}` must still be in the workspace and still
    /// be that process (the gate rechecks); anything else is skipped, never
    /// widened to the workspace's current processes.
    pub fn workspace_action_on(
        &self,
        path: &Path,
        action: WorkspaceAction,
        pids: &[PidRef],
        source: &str,
    ) -> Result<Vec<ActionResult>, String> {
        let (ws, nice) = {
            let st = lock(&self.state);
            let ws = st
                .snapshot
                .workspaces
                .iter()
                .find(|w| w.path == path)
                .cloned()
                .ok_or("workspace has no running processes")?;
            (ws, st.config.build_nice.max(10))
        };
        let mut out = Vec::new();
        let mut reqs = Vec::new();
        for r in pids {
            let Some(p) = ws.procs.iter().find(|p| p.pid == r.pid) else {
                out.push(ActionResult {
                    action: crate::safety::ActionKind::Signal,
                    target: format!("pid {}", r.pid),
                    outcome: Outcome::Skipped {
                        reason: crate::safety::SkipReason::UnknownTarget,
                        detail: "not a process of this workspace in the latest sample".into(),
                    },
                    journal_error: None,
                });
                continue;
            };
            let sigs: &[Signal] = match action {
                WorkspaceAction::Pause => &[Signal::Stop],
                WorkspaceAction::Resume => &[Signal::Cont],
                WorkspaceAction::StopBuilds if p.kind == ProcKind::Build => &[Signal::Term],
                WorkspaceAction::StopBuilds => &[],
                WorkspaceAction::KillAll => &[Signal::Cont, Signal::Term],
                WorkspaceAction::Deprioritize => {
                    reqs.push(Request::Renice {
                        pid: r.pid,
                        nice,
                        expected_start_time: r.expected_start_time,
                    });
                    &[]
                }
            };
            for s in sigs {
                reqs.push(Request::Signal {
                    pid: r.pid,
                    signal: *s,
                    expected_start_time: r.expected_start_time,
                });
            }
        }
        out.extend(self.execute(&reqs, source));
        Ok(out)
    }

    pub fn workspace_action(
        &self,
        path: &Path,
        action: WorkspaceAction,
        source: &str,
    ) -> Result<usize, String> {
        let (ws, nice) = {
            let st = lock(&self.state);
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
            // Through the gate: it re-reads each process (owner, start time),
            // refuses protected ones, only lowers, and journals each.
            let reqs: Vec<Request> = targets
                .iter()
                .map(|p| Request::Renice {
                    pid: p.pid,
                    nice: nice.max(10),
                    expected_start_time: p.start_time,
                })
                .collect();
            let done = self
                .execute(&reqs, source)
                .iter()
                .filter(|r| r.is_done())
                .count();
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

    /// Plan an action on one pid (optionally with its descendants) or on a
    /// consumer group, from facts read now. Every item carries the start time
    /// seen; send the approved requests to [`Engine::execute`].
    pub fn process_plan(&self, req: &ProcessPlanRequest) -> ProcessPlan {
        let now = now_unix();
        let procs = self.procs.list();
        let me = std::process::id();
        let mut ancestors = ancestors_of(&procs, me);
        if let Some(c) = req.caller_pid {
            ancestors.push(c);
            ancestors.extend(ancestors_of(&procs, c));
        }
        let (drafted, target, name, action) = match (&req.group, req.pid) {
            _ if req.session.is_some() => {
                let sid = req.session.clone().unwrap_or_default();
                let Some(action) = req.action else {
                    return ProcessPlan::refused(
                        now,
                        &sid,
                        "",
                        "invalid_request",
                        "a session plan needs an action".into(),
                    );
                };
                let act = serde_json::to_value(action)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                let (session, members) = match self.session_members(&sid, req.include_agent) {
                    Ok(x) => x,
                    Err((code, detail)) => {
                        return ProcessPlan::refused(now, &sid, &act, code, detail)
                    }
                };
                let alive: HashMap<u32, u64> =
                    procs.iter().map(|p| (p.pid, p.start_time)).collect();
                let members: Vec<ProcSample> = members
                    .into_iter()
                    .filter(|m| alive.get(&m.pid) == Some(&m.start_time) && m.pid != me)
                    .collect();
                let name = format!("{} session", session.agent);
                if members.is_empty() {
                    return ProcessPlan::refused(
                        now,
                        &sid,
                        &act,
                        "unknown_group",
                        if req.include_agent {
                            format!("no running processes in session {sid} in the latest sample")
                        } else {
                            format!("session {sid} runs no processes besides the agent; set include_agent to include it")
                        },
                    );
                }
                let nice = lock(&self.state).config.build_nice.max(10);
                let d = draft_group(
                    GroupKind::Process,
                    &name,
                    None,
                    None,
                    &members,
                    action,
                    nice,
                    cfg!(target_os = "macos"),
                );
                (d, sid, name, act)
            }
            (Some(key), _) => {
                let Some(action) = req.action else {
                    return ProcessPlan::refused(
                        now,
                        key,
                        "",
                        "invalid_request",
                        "a group plan needs an action".into(),
                    );
                };
                let act = serde_json::to_value(action)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                let (members, group, nice) = {
                    let st = lock(&self.state);
                    (
                        st.snapshot.members.get(key).cloned().unwrap_or_default(),
                        st.snapshot.consumer(key).cloned(),
                        st.config.build_nice.max(10),
                    )
                };
                // Only processes still alive with the same start time.
                let alive: HashMap<u32, u64> =
                    procs.iter().map(|p| (p.pid, p.start_time)).collect();
                let members: Vec<ProcSample> = members
                    .into_iter()
                    .filter(|m| alive.get(&m.pid) == Some(&m.start_time) && m.pid != me)
                    .collect();
                let kind = group_kind(key);
                let bundle = key.strip_prefix("app:").map(PathBuf::from);
                let name = group
                    .as_ref()
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| default_group_name(key, &members));
                if members.is_empty() {
                    return ProcessPlan::refused(
                        now,
                        key,
                        &act,
                        "unknown_group",
                        format!("no running processes in {key} in the latest sample"),
                    );
                }
                let ctx_ids = HashMap::new();
                let user_managers: Vec<u32> = procs
                    .iter()
                    .filter(|p| crate::consumers::is_user_manager(&p.name, &p.cmd))
                    .map(|p| p.pid)
                    .collect();
                let ctx = crate::consumers::GroupCtx {
                    cores: 1,
                    mem_total: 0,
                    self_pid: me,
                    ancestors: &ancestors,
                    macos: cfg!(target_os = "macos"),
                    bundle_ids: &ctx_ids,
                    user_managers: &user_managers,
                };
                if let Some(r) = crate::consumers::group_protection(kind, &members, &ctx) {
                    return ProcessPlan::refused(now, key, &act, "protected", r);
                }
                let bundle_id = group
                    .as_ref()
                    .and_then(|g| g.bundle_id.clone())
                    .or_else(|| bundle.as_deref().and_then(crate::consumers::bundle_id));
                let d = draft_group(
                    kind,
                    &name,
                    bundle.as_deref(),
                    bundle_id.as_deref(),
                    &members,
                    action,
                    nice,
                    cfg!(target_os = "macos"),
                );
                (d, key.clone(), name, act)
            }
            (None, Some(pid)) => {
                let signal = req.signal.unwrap_or(Signal::Term);
                let act = serde_json::to_value(signal)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                // The caller, its shell, terminal or agent, and Mulchie's own
                // ancestors: the whole plan is refused (a tree plan of one of
                // them would hold the caller too).
                if ancestors.contains(&pid) {
                    let who = if req.caller_pid == Some(pid) {
                        "the caller itself"
                    } else if req
                        .caller_pid
                        .is_some_and(|c| ancestors_of(&procs, c).contains(&pid))
                    {
                        "an ancestor of the caller (its shell, terminal or agent)"
                    } else {
                        "an ancestor of Mulchie"
                    };
                    return ProcessPlan::refused(
                        now,
                        &format!("pid {pid}"),
                        &act,
                        "protected",
                        format!("pid {pid} is {who}; it is never signalled"),
                    );
                }
                let name = procs
                    .iter()
                    .find(|p| p.pid == pid)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("pid {pid}"));
                (
                    draft_pid(&procs, pid, signal, req.tree),
                    format!("pid {pid}"),
                    name,
                    act,
                )
            }
            (None, None) => {
                return ProcessPlan::refused(
                    now,
                    "",
                    "",
                    "invalid_request",
                    "expected {\"pid\": N} or {\"group\": \"<key>\", \"action\": \"quit\"}".into(),
                )
            }
        };
        let d = match drafted {
            Ok(d) => d,
            Err((code, detail)) => {
                return ProcessPlan::refused(now, &target, &action, code, detail)
            }
        };
        // No per-item ancestor check is needed: a tree plan holds the target
        // and its descendants, so one of them is an ancestor of the caller or
        // of Mulchie only when the target is, and that is refused above (from
        // the same `procs`). Group plans refuse them in `group_protection`.
        let items = self.plan(&d.requests).items;
        let ready = items.iter().any(|i| i.ready);
        ProcessPlan {
            created_unix: now,
            target,
            name,
            action,
            ready,
            refused: None,
            detail: d.detail,
            items,
            affected: d.affected,
            restart: d.restart,
        }
    }

    /// One action on `pid` only if it is still the process started at
    /// `expected_start_time`; a mismatch is a skipped result, not an error.
    pub fn process_action_expecting(
        &self,
        pid: u32,
        action: ProcAction,
        expected_start_time: u64,
        source: &str,
    ) -> ActionResult {
        let req = match action {
            ProcAction::Renice { nice } => Request::Renice {
                pid,
                nice: nice.clamp(0, 20),
                expected_start_time,
            },
            a => Request::Signal {
                pid,
                signal: match a {
                    ProcAction::Kill => Signal::Kill,
                    ProcAction::Stop => Signal::Stop,
                    ProcAction::Cont => Signal::Cont,
                    _ => Signal::Term,
                },
                expected_start_time,
            },
        };
        self.execute(std::slice::from_ref(&req), source)
            .into_iter()
            .next()
            .unwrap_or(ActionResult {
                action: req.action(),
                target: req.target(),
                outcome: Outcome::Skipped {
                    reason: crate::safety::SkipReason::UnknownTarget,
                    detail: "no result".into(),
                },
                journal_error: None,
            })
    }

    pub fn process_action(&self, pid: u32, action: ProcAction, source: &str) -> Result<(), String> {
        if pid <= 1 || pid == std::process::id() {
            return Err("refusing to signal this process".into());
        }
        let info = lock(&self.state)
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
                // Through the gate: owner, start time and protected checks,
                // lowering only, journaled.
                let req = Request::Renice {
                    pid,
                    nice: nice.clamp(0, 20),
                    expected_start_time: info.start_time,
                };
                let r = self.execute(std::slice::from_ref(&req), source);
                return match r.first().map(|r| &r.outcome) {
                    Some(Outcome::Done { .. }) => Ok(()),
                    Some(Outcome::Skipped { detail, .. }) => Err(detail.clone()),
                    None => Err("no result".into()),
                };
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
        let now = now_unix();
        let st = lock(&self.state);
        let s = &st.snapshot;
        let cfg = &st.config;
        let health = st.health_at(now);
        let free = st.slots.capacity.saturating_sub(st.slots.active.len());
        let mut reasons = Vec::new();
        if health == Health::Stale {
            reasons.push("the engine has no recent sample of this machine".to_string());
        }
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
        if s.disk_total > 0 && s.disk_free_gb() < cfg.low_disk_gb as f64 {
            reasons.push(format!("only {:.0} GB disk free", s.disk_free_gb()));
        }
        Advice {
            health,
            ok_to_start_heavy_work: reasons.is_empty(),
            free_slots: free,
            suggested_jobs: st.slots.jobs_per_slot.max(1),
            reasons,
        }
    }

    // ------------------------------------------------------------ build queue

    pub fn acquire(&self, req: &LeaseRequest) -> Result<LeaseReply, LeaseError> {
        self.acquire_at(req, now_unix())
    }

    /// Join the queue, poll a ticket, or renew a granted lease (send its ticket
    /// again). A lease needs a live pid or a TTL; with a TTL it ends unless
    /// renewed in time, with a pid when that process exits.
    pub fn acquire_at(&self, req: &LeaseRequest, now: u64) -> Result<LeaseReply, LeaseError> {
        let pid = req.pid.filter(|p| *p > 0);
        if let Some(p) = pid {
            if p == 1 {
                return Err(LeaseError::PidNotAllowed(p));
            }
        }
        if let Some(t) = req.ttl_secs {
            if t == 0 || t > MAX_LEASE_TTL_SECS {
                return Err(LeaseError::TtlOutOfRange(t));
            }
        }
        let mut pid_start_time = None;
        if let Some(p) = pid {
            if !pid_alive(p) {
                return Err(LeaseError::PidNotAlive(p));
            }
            // Alive but not ours to signal: another user's process.
            if !own_pid_alive(p) {
                return Err(LeaseError::PidNotAllowed(p));
            }
            if let Some(f) = self.procs.get(p) {
                if f.uid.is_some_and(|u| u != self.procs.current_uid()) {
                    return Err(LeaseError::PidNotAllowed(p));
                }
                pid_start_time = Some(f.start_time);
            }
        }
        // Labels leave the engine (state, journal): no secrets, bounded.
        let label = if req.label.is_empty() {
            pid.map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "lease".into())
        } else {
            crate::redact::redact_label(&req.label)
        };
        let mut st = lock(&self.state);
        expire_leases(&mut st.slots, now, &|p, s| self.lease_pid_alive(p, s));
        // A known ticket (renewal or poll) keeps what it was granted with; a
        // new request must say how it ends.
        let known = req.ticket.is_some_and(|t| {
            st.slots.active.iter().any(|l| l.id == t) || st.slots.waiting.iter().any(|w| w.id == t)
        });
        if !known && pid.is_none() && req.ttl_secs.is_none() {
            return Err(LeaseError::NeedsPidOrTtl);
        }
        if known {
            // Whoever renews or polls a ticket must be the one who took it.
            let t = req.ticket.unwrap_or_default();
            let want = st
                .slots
                .active
                .iter()
                .find(|l| l.id == t)
                .map(|l| l.release_secret.clone())
                .or_else(|| {
                    st.slots
                        .waiting
                        .iter()
                        .find(|w| w.id == t)
                        .map(|w| w.release_secret.clone())
                })
                .flatten();
            if let Some(want) = want {
                match req.secret.as_deref() {
                    None => return Err(LeaseError::SecretRequired),
                    Some(got) if !crate::api::constant_time_eq(got.as_bytes(), want.as_bytes()) => {
                        return Err(LeaseError::WrongSecret)
                    }
                    Some(_) => {}
                }
            }
        }
        let jobs = st.slots.jobs_per_slot.max(1);
        let ticket = req.ticket;
        if let Some(l) = ticket.and_then(|t| st.slots.active.iter_mut().find(|l| l.id == t)) {
            // A renewal extends a TTL lease; it never shortens one and never
            // turns a pid lease into a TTL lease.
            if let Some(old_ttl) = l.ttl_secs {
                let ttl = req.ttl_secs.unwrap_or(old_ttl);
                let new_expiry = now + ttl;
                if l.expires_unix.is_none_or(|e| new_expiry >= e) {
                    l.ttl_secs = Some(ttl);
                    l.expires_unix = Some(new_expiry);
                }
            }
            return Ok(LeaseReply {
                granted: true,
                ticket: l.id,
                jobs: l.jobs,
                position: 0,
                reason: "renewed".into(),
                expires_unix: l.expires_unix,
                release_secret: None,
            });
        }
        let mut new_secret = None;
        let id = match ticket.filter(|t| st.slots.waiting.iter().any(|w| w.id == *t)) {
            Some(t) => t,
            None => {
                // Never a lease without a secret: anyone could release it.
                let secret =
                    release_secret().map_err(|e| LeaseError::SecretUnavailable(e.to_string()))?;
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                new_secret = Some(secret);
                st.slots.waiting.push(Waiter {
                    id,
                    label,
                    pid,
                    since_unix: now,
                    last_poll: now,
                    ttl_secs: req.ttl_secs,
                    pid_start_time,
                    release_secret: new_secret.clone(),
                });
                id
            }
        };
        if let Some(w) = st.slots.waiting.iter_mut().find(|w| w.id == id) {
            w.last_poll = now;
            // A waiter's TTL may grow, never shrink, and a pid waiter never
            // gains one.
            if let (Some(new), Some(old)) = (req.ttl_secs, w.ttl_secs) {
                w.ttl_secs = Some(new.max(old));
            }
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
            let expires_unix = w.ttl_secs.map(|t| now + t);
            st.slots.active.push(Lease {
                id,
                label: w.label,
                pid: w.pid,
                since_unix: now,
                jobs,
                ttl_secs: w.ttl_secs,
                expires_unix,
                pid_start_time: w.pid_start_time,
                release_secret: w.release_secret,
            });
            let reason = format!("slot {}/{}", st.slots.active.len(), cap);
            return Ok(LeaseReply {
                granted: true,
                ticket: id,
                jobs,
                position: 0,
                reason,
                expires_unix,
                release_secret: new_secret,
            });
        }
        let reason = if !mem_ok {
            "waiting for memory pressure to drop".to_string()
        } else if position > 0 {
            format!("{position} job(s) ahead in the queue")
        } else {
            format!("all {cap} build slots are busy")
        };
        Ok(LeaseReply {
            granted: false,
            ticket: id,
            jobs,
            position,
            reason,
            expires_unix: None,
            release_secret: new_secret,
        })
    }

    /// Release `ticket` when `secret` matches the lease's release secret (a
    /// lease or waiter that has one cannot be released without it).
    /// `Ok(false)` when there is no such ticket.
    /// Release `ticket` without its secret: an admin release (full API token
    /// only), for a lease whose holder crashed and took its secret with it.
    /// Journaled as such. Returns whether a lease or waiter was released.
    pub fn release_admin(&self, ticket: u64, source: &str) -> bool {
        let label = {
            let mut st = lock(&self.state);
            let label = st
                .slots
                .active
                .iter()
                .find(|l| l.id == ticket)
                .map(|l| l.label.clone())
                .or_else(|| {
                    st.slots
                        .waiting
                        .iter()
                        .find(|w| w.id == ticket)
                        .map(|w| w.label.clone())
                });
            if label.is_some() {
                st.slots.active.retain(|l| l.id != ticket);
                st.slots.waiting.retain(|w| w.id != ticket);
            }
            label
        };
        let Some(label) = label else {
            return false;
        };
        self.log(
            "lease",
            format!(
                "Admin release of build slot ticket {ticket} ({label}) without its release secret"
            ),
            0,
            None,
            source,
        );
        true
    }

    pub fn release_with(&self, ticket: u64, secret: Option<&str>) -> Result<bool, ReleaseError> {
        let mut st = lock(&self.state);
        let held = st
            .slots
            .active
            .iter()
            .find(|l| l.id == ticket)
            .map(|l| l.release_secret.clone())
            .or_else(|| {
                st.slots
                    .waiting
                    .iter()
                    .find(|w| w.id == ticket)
                    .map(|w| w.release_secret.clone())
            });
        let Some(want) = held else {
            return Ok(false);
        };
        if let Some(want) = want {
            match secret {
                None => return Err(ReleaseError::SecretRequired),
                Some(got) if !crate::api::constant_time_eq(got.as_bytes(), want.as_bytes()) => {
                    return Err(ReleaseError::WrongSecret)
                }
                Some(_) => {}
            }
        }
        st.slots.active.retain(|l| l.id != ticket);
        st.slots.waiting.retain(|w| w.id != ticket);
        Ok(true)
    }

    /// Release `ticket` without a secret: for in-process callers (the app,
    /// the CLI's own engine). The HTTP route uses [`Engine::release_with`].
    pub fn release(&self, ticket: u64) -> bool {
        let mut st = lock(&self.state);
        let before = st.slots.active.len() + st.slots.waiting.len();
        st.slots.active.retain(|l| l.id != ticket);
        st.slots.waiting.retain(|w| w.id != ticket);
        before != st.slots.active.len() + st.slots.waiting.len()
    }

    /// Drop leases whose process exited or whose TTL passed, and waiters that
    /// stopped polling.
    pub fn expire_leases_at(&self, now: u64) {
        expire_leases(&mut lock(&self.state).slots, now, &|p, s| {
            self.lease_pid_alive(p, s)
        });
    }

    /// A lease's process is still the one that took it: signalable by us
    /// (EPERM is not enough) and, when known, with the same start time.
    fn lease_pid_alive(&self, pid: u32, start_time: Option<u64>) -> bool {
        if !own_pid_alive(pid) {
            return false;
        }
        match start_time {
            Some(s) => self.procs.get(pid).is_none_or(|f| f.start_time == s),
            None => true,
        }
    }

    #[cfg(test)]
    pub(crate) fn state_mut(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.state)
    }

    #[cfg(test)]
    pub(crate) fn set_restart_backoff(&self, d: Duration) {
        self.backoff_ms
            .store(d.as_millis() as u64, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: make [`release_secret`] fail on this thread.
    pub(crate) static FAIL_SECRETS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A new lease's release secret.
fn release_secret() -> std::io::Result<String> {
    #[cfg(test)]
    if FAIL_SECRETS.with(|f| f.get()) {
        return Err(std::io::Error::other("no randomness (test)"));
    }
    crate::paths::random_hex(16)
}

fn expire_leases(slots: &mut Slots, now: u64, alive: &dyn Fn(u32, Option<u64>) -> bool) {
    slots.active.retain(|l| {
        l.pid.is_none_or(|p| alive(p, l.pid_start_time))
            && l.expires_unix.map(|e| now < e).unwrap_or(true)
    });
    slots.waiting.retain(|w| {
        w.pid.is_none_or(|p| alive(p, w.pid_start_time))
            && now.saturating_sub(w.last_poll) < WAITER_POLL_SECS
    });
}

/// The parent, grandparent, … of `pid` (not pid 1).
fn ancestors_of(procs: &[crate::process::ProcFacts], pid: u32) -> Vec<u32> {
    let ppid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut out = Vec::new();
    let mut cur = ppid.get(&pid).copied().unwrap_or(0);
    while cur > 1 && out.len() < 32 && !out.contains(&cur) {
        out.push(cur);
        cur = ppid.get(&cur).copied().unwrap_or(0);
    }
    out
}

fn group_kind(key: &str) -> GroupKind {
    if key.starts_with("workspace:") {
        GroupKind::Workspace
    } else if key.starts_with("app:") {
        GroupKind::App
    } else if key == "system" {
        GroupKind::System
    } else {
        GroupKind::Process
    }
}

fn default_group_name(key: &str, members: &[ProcSample]) -> String {
    if let Some(b) = key.strip_prefix("app:") {
        return Path::new(b)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| b.to_string());
    }
    if let Some(ws) = members.first().and_then(|m| m.workspace.as_ref()) {
        return ws.1.clone();
    }
    key.strip_prefix("process:").unwrap_or(key).to_string()
}

/// `kill(pid, 0)` succeeds: the process exists and this user may signal it.
fn own_pid_alive(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32 && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn throttle_notes(snap: &Snapshot, newly: &[u32], now: u64) -> Vec<Action> {
    let mut notes = Vec::new();
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
    notes
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

/// The current nice value of `pid`, if readable.
pub fn nice_of(pid: u32) -> Option<i32> {
    unsafe {
        *errno() = 0;
        let cur = libc::getpriority(libc::PRIO_PROCESS as _, pid as libc::id_t);
        (*errno() == 0).then_some(cur)
    }
}

/// Raise `pid`'s nice value to `nice` when it is lower. Returns the previous
/// value when it changed.
fn lower_priority(pid: u32, nice: i32) -> Option<i32> {
    let cur = nice_of(pid)?;
    if cur >= nice {
        return None;
    }
    set_nice(pid, nice).ok().map(|_| cur)
}

/// Set the nice value of `pid`. On Linux nice is per thread, so every thread
/// in `/proc/<pid>/task` is set (threads started later inherit from the thread
/// that creates them, not from this call).
pub fn set_nice(pid: u32, nice: i32) -> std::io::Result<()> {
    let set = |id: u32| -> std::io::Result<()> {
        if unsafe { libc::setpriority(libc::PRIO_PROCESS as _, id as libc::id_t, nice) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };
    if cfg!(target_os = "linux") {
        if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            let tids: Vec<u32> = rd
                .flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
                .collect();
            if !tids.is_empty() {
                let mut first_err = None;
                let mut any = false;
                for t in tids {
                    match set(t) {
                        Ok(()) => any = true,
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                return match (any, first_err) {
                    (true, _) | (false, None) => Ok(()),
                    (false, Some(e)) => Err(e),
                };
            }
        }
    }
    set(pid)
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
#[path = "engine_tests.rs"]
mod tests;
