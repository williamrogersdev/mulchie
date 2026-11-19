//! A profile of each project's dev server, run by run, and the
//! `dev_server_slow_start` and `dev_server_heavier` findings it raises.
//!
//! A dev server is a long-running process of the user's that sits in a
//! workspace and listens on a TCP port: `next dev`, `vite`, `rails s`, a web
//! app run with `cargo run`. Nothing is probed for it. The watch run already
//! reads the process table and the listening sockets, and the sample loop
//! already reads each process's memory and CPU; the [`Tracker`] is fed from
//! both, in memory, with no git and no disk.
//!
//! A run is one process (pid and start time). For each run Mulchie keeps:
//!
//! - **time to listen**: from the process's start to the watch run that first
//!   saw it listening. A listening socket is only seen once per watch run
//!   (`watch.interval_secs`, 60 by default), so the figure is an upper bound,
//!   and the true value lies within `listen_uncertainty_ms` below it. It is
//!   only recorded when Mulchie saw the process before it listened, or it
//!   started after the previous watch run; a process that was already
//!   listening when Mulchie first looked has no time to listen.
//! - **settled memory**: the median resident memory of the process and its
//!   descendants over minutes 2 to 5 of the run (at least 3 samples).
//! - **memory slope**: the least-squares slope of that memory from minute 2
//!   on, in MB per hour, once the run has been watched for 10 minutes.
//! - **peak CPU** (percent of one core, the tree summed) and peak memory.
//! - **how it ended**: `running` for a record written while it lives,
//!   `exited` once the process is gone.
//!
//! A record is appended to `<config dir>/devservers.jsonl` ([`crate::jsonl`]:
//! mode 0600, never through a symbolic link, renamed to `.1` at 2 MB) when the
//! run ends and every [`SNAPSHOT_SECS`] while it lasts. A run that ended
//! within [`MIN_RUN_SECS`] of when it was first seen listening is not
//! recorded (a test server, not a dev server).
//!
//! Runs are grouped by workspace and the command as
//! [`crate::issues::normalize_command`] sees it. Each group's runs, one per
//! process (the newest record of each), are compared with [`crate::perf`]:
//! `dev_server_slow_start` when the latest runs each took much longer to
//! listen, and `dev_server_heavier` when the latest runs each settled at much
//! more memory. Both name the baseline, the recent runs and the moves of
//! `HEAD` between the last normal run and the first slow one, with the
//! sessions that were working then. Nothing acts on a run or a finding.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::commits::{self, CommitEntry};
use crate::config::PerfConfig;
use crate::consumers::Severity;
use crate::jsonl::JsonlFile;
use crate::paths::Paths;
use crate::perf::{self, human_ms, Thresholds};
use crate::watch::{ApiRequest, Evidence, Finding, FindingCategory, FindingKind, Suggestion};

/// The file is renamed to `devservers.jsonl.1` past this size.
pub const ROTATE_BYTES: u64 = 2 * 1024 * 1024;
/// Records the engine keeps in memory (the newest).
pub const KEPT: usize = 5_000;
/// A run is tracked at most this many at a time.
pub const MAX_RUNS: usize = 64;
/// A record is written this often while a run lasts.
pub const SNAPSHOT_SECS: u64 = 30 * 60;
/// A run seen listening for less than this long is not recorded.
pub const MIN_RUN_SECS: u64 = 60;
/// The memory window that counts as settled, in seconds since the start.
pub const SETTLE_FROM_SECS: u64 = 120;
pub const SETTLE_TO_SECS: u64 = 300;
/// Samples in the settle window needed for a figure.
pub const MIN_SETTLE_SAMPLES: usize = 3;
/// A slope needs this much watched time, and this many samples.
pub const MIN_SLOPE_SPAN_SECS: u64 = 600;
pub const MIN_SLOPE_SAMPLES: u64 = 10;
/// Settled memory counts as higher only when it is this much above the
/// baseline (the ratio is `perf.slower_ratio`).
pub const MIN_HEAVIER_BYTES: f64 = 256.0 * 1_048_576.0;
/// Ports kept per run.
pub const MAX_PORTS: usize = 8;
/// Commits listed in a finding's evidence, at most (the newest).
pub const MAX_EVIDENCE_COMMITS: usize = crate::timings::MAX_EVIDENCE_COMMITS;
/// Runs of finished servers waiting for the worker, at most.
const MAX_FINISHED: usize = 64;
/// Settle-window samples kept per run, at most.
const MAX_EARLY: usize = 256;

/// How a run stands.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// Still alive when the record was written.
    #[default]
    Running,
    /// The process is gone.
    Exited,
}

/// One record of one run. A run that lasts is recorded more than once; the
/// newest record of a (pid, start) is the run as far as Mulchie knows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DevServer {
    /// When the record was written.
    pub at_unix: u64,
    pub pid: u32,
    /// The process's start (seconds since the epoch); with `pid` it is the run.
    pub start_unix: u64,
    pub workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The workspace's `HEAD` when Mulchie first saw the run listening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The command line, redacted and cut ([`crate::timings::clean_command`]).
    #[serde(default)]
    pub command: String,
    /// The command as runs are grouped: [`crate::issues::normalize_command`].
    #[serde(default)]
    pub key: String,
    /// The ports it listened on, the first few.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Process start to the first watch run that saw it listening, an upper
    /// bound: the true time is within `listen_uncertainty_ms` below it.
    /// Absent when Mulchie first met the process already listening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_uncertainty_ms: Option<u64>,
    /// `watch.interval_secs` when it was measured.
    #[serde(default)]
    pub watch_interval_secs: u64,
    /// Median memory of the process tree over minutes 2 to 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_mem_bytes: Option<u64>,
    /// MB per hour from minute 2 on; absent before 10 minutes were watched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_slope_mb_per_hour: Option<f64>,
    /// Percent of one core, the process tree summed.
    #[serde(default)]
    pub peak_cpu_pct: f32,
    #[serde(default)]
    pub peak_mem_bytes: u64,
    /// Processes in the tree at the last sample.
    #[serde(default)]
    pub processes: usize,
    /// Seconds from the start to the last sample that saw it.
    #[serde(default)]
    pub run_secs: u64,
    #[serde(default)]
    pub end: End,
}

pub fn file(paths: &Paths) -> JsonlFile {
    JsonlFile::at(paths.config_dir.join("devservers.jsonl"), ROTATE_BYTES)
}

/// The runs `records` describe: the newest record of each (pid, start), in
/// the order the runs started, oldest first.
pub fn runs(records: &[DevServer]) -> Vec<&DevServer> {
    let mut latest: HashMap<(&Path, u32, u64), &DevServer> = HashMap::new();
    for r in records {
        let k = (r.workspace.as_path(), r.pid, r.start_unix);
        match latest.get(&k) {
            Some(old) if old.at_unix > r.at_unix => {}
            _ => {
                latest.insert(k, r);
            }
        }
    }
    let mut v: Vec<&DevServer> = latest.into_values().collect();
    v.sort_by(|a, b| (a.start_unix, a.pid, &a.workspace).cmp(&(b.start_unix, b.pid, &b.workspace)));
    v
}

// ------------------------------------------------------------- tracker

/// A process that listens, as the watch run found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listening {
    pub pid: u32,
    pub start_time: u64,
    pub ports: Vec<u16>,
    pub workspace: PathBuf,
    /// Redacted and cut.
    pub command: String,
}

/// One process of a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleProc {
    pub pid: u32,
    pub ppid: u32,
    pub start_time: u64,
    pub mem_bytes: u64,
    pub cpu_pct: f32,
}

/// Running sums for a least-squares line through (seconds, MiB).
#[derive(Debug, Clone, Default)]
struct Line {
    n: u64,
    sx: f64,
    sy: f64,
    sxx: f64,
    sxy: f64,
    min_x: f64,
    max_x: f64,
}

impl Line {
    fn add(&mut self, x: f64, y: f64) {
        if self.n == 0 {
            self.min_x = x;
            self.max_x = x;
        }
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.n += 1;
        self.sx += x;
        self.sy += y;
        self.sxx += x * x;
        self.sxy += x * y;
    }

    /// MiB per second.
    fn slope(&self) -> Option<f64> {
        let n = self.n as f64;
        let d = n * self.sxx - self.sx * self.sx;
        (self.n >= 2 && d.abs() > f64::EPSILON).then(|| (n * self.sxy - self.sx * self.sy) / d)
    }
}

#[derive(Debug, Clone)]
struct Run {
    pid: u32,
    start_time: u64,
    workspace: PathBuf,
    command: String,
    key: String,
    ports: Vec<u16>,
    first_seen_unix: u64,
    listen_ms: Option<u64>,
    listen_uncertainty_ms: Option<u64>,
    watch_interval_secs: u64,
    last_seen_unix: u64,
    last_record_unix: u64,
    early: Vec<u64>,
    line: Line,
    peak_cpu_pct: f32,
    peak_mem_bytes: u64,
    processes: usize,
    branch: Option<String>,
    head: Option<String>,
    session_id: Option<String>,
    agent: Option<String>,
    attributed: bool,
}

impl Run {
    fn record(&self, now: u64, end: End) -> DevServer {
        let settled = (self.early.len() >= MIN_SETTLE_SAMPLES).then(|| {
            let v: Vec<f64> = self.early.iter().map(|b| *b as f64).collect();
            perf::median(&v).unwrap_or(0.0) as u64
        });
        let slope = self
            .line
            .slope()
            .filter(|_| {
                self.line.n >= MIN_SLOPE_SAMPLES
                    && self.line.max_x - self.line.min_x >= MIN_SLOPE_SPAN_SECS as f64
            })
            .map(|mib_per_sec| (mib_per_sec * 3600.0 * 10.0).round() / 10.0);
        DevServer {
            at_unix: now,
            pid: self.pid,
            start_unix: self.start_time,
            workspace: self.workspace.clone(),
            branch: self.branch.clone(),
            head: self.head.clone(),
            session_id: self.session_id.clone(),
            agent: self.agent.clone(),
            command: self.command.clone(),
            key: self.key.clone(),
            ports: self.ports.clone(),
            listen_ms: self.listen_ms,
            listen_uncertainty_ms: self.listen_uncertainty_ms,
            watch_interval_secs: self.watch_interval_secs,
            settled_mem_bytes: settled,
            mem_slope_mb_per_hour: slope,
            peak_cpu_pct: self.peak_cpu_pct,
            peak_mem_bytes: self.peak_mem_bytes,
            processes: self.processes,
            run_secs: self.last_seen_unix.saturating_sub(self.start_time),
            end,
        }
    }
}

/// The runs Mulchie is watching, fed by the watch run and the sample loop.
/// Pure and in memory: the engine writes the records it hands out.
#[derive(Debug, Default)]
pub struct Tracker {
    live: HashMap<(u32, u64), Run>,
    /// Runs that ended, waiting for [`Tracker::due`].
    finished: Vec<Run>,
    /// The user's processes at the last watch run whose listeners were known.
    prev_seen: HashSet<(u32, u64)>,
    prev_watch_unix: Option<u64>,
}

impl Tracker {
    pub fn is_idle(&self) -> bool {
        self.live.is_empty()
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// One watch run. `seen` is every process of the user; `listening`
    /// the ones with a listening socket that sit in a workspace, `None` when
    /// the listeners could not be read this time (nothing changes then).
    pub fn observe_watch(
        &mut self,
        now: u64,
        interval_secs: u64,
        seen: &[(u32, u64)],
        listening: Option<&[Listening]>,
    ) {
        let Some(listening) = listening else {
            return;
        };
        let gap = self.prev_watch_unix.map(|p| now.saturating_sub(p));
        for l in listening {
            let id = (l.pid, l.start_time);
            if let Some(run) = self.live.get_mut(&id) {
                for p in &l.ports {
                    if run.ports.len() < MAX_PORTS && !run.ports.contains(p) {
                        run.ports.push(*p);
                    }
                }
                continue;
            }
            if self.live.len() >= MAX_RUNS {
                continue;
            }
            // Before this watch run the process was seen not listening, or
            // did not exist yet: either way it began to listen since.
            let observed = gap.is_some()
                && (self.prev_seen.contains(&id)
                    || self.prev_watch_unix.is_some_and(|p| l.start_time >= p));
            let since_start = now.saturating_sub(l.start_time);
            let (listen_ms, unc) = match gap {
                Some(g) if observed => (
                    Some(since_start * 1000),
                    Some((g.min(since_start) + 1) * 1000),
                ),
                _ => (None, None),
            };
            let mut ports: Vec<u16> = l.ports.iter().copied().take(MAX_PORTS).collect();
            ports.sort_unstable();
            ports.dedup();
            self.live.insert(
                id,
                Run {
                    pid: l.pid,
                    start_time: l.start_time,
                    workspace: l.workspace.clone(),
                    command: l.command.clone(),
                    key: crate::issues::normalize_command(&l.command),
                    ports,
                    first_seen_unix: now,
                    listen_ms,
                    listen_uncertainty_ms: unc,
                    watch_interval_secs: interval_secs,
                    last_seen_unix: now,
                    last_record_unix: now,
                    early: Vec::new(),
                    line: Line::default(),
                    peak_cpu_pct: 0.0,
                    peak_mem_bytes: 0,
                    processes: 1,
                    branch: None,
                    head: None,
                    session_id: None,
                    agent: None,
                    attributed: false,
                },
            );
        }
        self.prev_seen = seen.iter().copied().collect();
        self.prev_watch_unix = Some(now);
    }

    /// One sample of every process. An empty sample says nothing.
    pub fn observe_sample(&mut self, now: u64, procs: &[SampleProc]) {
        if self.live.is_empty() || procs.is_empty() {
            return;
        }
        let by_pid: HashMap<u32, &SampleProc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in procs {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        let mut gone = Vec::new();
        for (id, run) in self.live.iter_mut() {
            match by_pid.get(&id.0) {
                Some(p) if p.start_time == id.1 => {}
                _ => {
                    gone.push(*id);
                    continue;
                }
            }
            // The process and its descendants, each once.
            let mut seen = HashSet::from([id.0]);
            let mut stack = vec![id.0];
            let (mut mem, mut cpu) = (0u64, 0f32);
            while let Some(pid) = stack.pop() {
                if let Some(p) = by_pid.get(&pid) {
                    mem = mem.saturating_add(p.mem_bytes);
                    cpu += p.cpu_pct;
                }
                for c in children.get(&pid).into_iter().flatten() {
                    if seen.insert(*c) {
                        stack.push(*c);
                    }
                }
            }
            run.last_seen_unix = now;
            run.processes = seen.len();
            run.peak_cpu_pct = run.peak_cpu_pct.max(cpu);
            run.peak_mem_bytes = run.peak_mem_bytes.max(mem);
            let elapsed = now.saturating_sub(run.start_time);
            if (SETTLE_FROM_SECS..=SETTLE_TO_SECS).contains(&elapsed) && run.early.len() < MAX_EARLY
            {
                run.early.push(mem);
            }
            if elapsed >= SETTLE_FROM_SECS {
                run.line.add(
                    (elapsed - SETTLE_FROM_SECS) as f64,
                    mem as f64 / 1_048_576.0,
                );
            }
        }
        for id in gone {
            if let Some(run) = self.live.remove(&id) {
                if self.finished.len() >= MAX_FINISHED {
                    self.finished.remove(0);
                }
                self.finished.push(run);
            }
        }
    }

    /// Runs not yet attributed (workspace, pid, start): the live ones and
    /// the finished ones.
    pub fn unattributed(&self) -> Vec<(PathBuf, u32, u64)> {
        self.live
            .values()
            .chain(self.finished.iter())
            .filter(|r| !r.attributed)
            .map(|r| (r.workspace.clone(), r.pid, r.start_time))
            .collect()
    }

    pub fn attribute(
        &mut self,
        pid: u32,
        start: u64,
        branch: Option<String>,
        head: Option<String>,
        session: Option<(String, String)>,
    ) {
        let hit = |r: &&mut Run| r.pid == pid && r.start_time == start;
        for r in self
            .live
            .values_mut()
            .chain(self.finished.iter_mut())
            .filter(hit)
        {
            r.branch = branch.clone();
            r.head = head.clone();
            if let Some((id, agent)) = &session {
                r.session_id = Some(id.clone());
                r.agent = Some(agent.clone());
            }
            r.attributed = true;
        }
    }

    /// The records due now: one for each run that ended, and one for each
    /// run that lived [`SNAPSHOT_SECS`] since its last.
    pub fn due(&mut self, now: u64) -> Vec<DevServer> {
        let mut out = Vec::new();
        for r in std::mem::take(&mut self.finished) {
            if r.last_seen_unix.saturating_sub(r.first_seen_unix) >= MIN_RUN_SECS {
                out.push(r.record(now, End::Exited));
            }
        }
        for r in self.live.values_mut() {
            if now.saturating_sub(r.last_record_unix) >= SNAPSHOT_SECS {
                r.last_record_unix = now;
                out.push(r.record(now, End::Running));
            }
        }
        out.sort_by_key(|r| (r.start_unix, r.pid));
        out
    }

    /// The runs alive now, as they stand, oldest first.
    pub fn live(&self, now: u64) -> Vec<DevServer> {
        let mut v: Vec<DevServer> = self
            .live
            .values()
            .map(|r| r.record(now, End::Running))
            .collect();
        v.sort_by_key(|r| (r.start_unix, r.pid));
        v
    }

    /// Where the live runs are, for the periodic `HEAD` check.
    pub fn workspaces(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.live.values().map(|r| r.workspace.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

// ------------------------------------------------------------ findings

/// Which measure a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Measure {
    Listen,
    Memory,
}

impl Measure {
    fn kind(self) -> FindingKind {
        match self {
            Measure::Listen => FindingKind::DevServerSlowStart,
            Measure::Memory => FindingKind::DevServerHeavier,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Measure::Listen => "time_to_listen",
            Measure::Memory => "settled_memory",
        }
    }
}

/// The evidence of a dev server finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DevServerEvidence {
    /// The command as runs are grouped.
    pub command: String,
    /// `time_to_listen` (values in milliseconds) or `settled_memory` (bytes).
    pub measure: String,
    /// The median of the runs before the slow ones.
    pub baseline: u64,
    pub baseline_runs: usize,
    /// The latest runs, oldest first.
    pub recent: Vec<u64>,
    /// The median of `recent` over `baseline`.
    pub ratio: f64,
    /// `time_to_listen` only: the largest uncertainty of the runs compared
    /// (the watch interval's granularity), in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncertainty_ms: Option<u64>,
    /// `HEAD` at the last normal run and at the first slow run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_head: Option<String>,
    /// The recorded moves of `HEAD` between them, oldest first, with the
    /// sessions that were working then (at most [`MAX_EVIDENCE_COMMITS`],
    /// the newest).
    #[serde(default)]
    pub commits: Vec<CommitEntry>,
    #[serde(default)]
    pub commits_truncated: bool,
}

/// `<kind>:<16 hex digits of the workspace and command>`.
pub fn finding_id(kind: FindingKind, workspace: &Path, key: &str) -> String {
    format!(
        "{}:{:016x}",
        kind.code(),
        crate::issues::fnv1a(format!("{}\u{1f}{key}", workspace.to_string_lossy()).as_bytes())
    )
}

/// `/v1/devservers?workspace=…`.
pub fn route(workspace: Option<&Path>) -> String {
    match workspace {
        Some(w) => format!(
            "/v1/devservers?workspace={}",
            crate::api::query_escape(&w.to_string_lossy())
        ),
        None => "/v1/devservers".into(),
    }
}

/// The dev server findings for `records` (any order), with commit
/// attribution from `commit_log`.
pub fn findings(
    records: &[DevServer],
    commit_log: &[CommitEntry],
    cfg: &PerfConfig,
    now: u64,
) -> Vec<Finding> {
    let keep = (cfg.baseline_runs + cfg.slow_runs) * 4;
    let mut groups: HashMap<(&Path, &str), Vec<&DevServer>> = HashMap::new();
    for r in runs(records).into_iter().filter(|r| !r.key.is_empty()) {
        groups
            .entry((r.workspace.as_path(), r.key.as_str()))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for ((ws, key), group) in groups {
        for measure in [Measure::Listen, Measure::Memory] {
            let series: Vec<&DevServer> = group
                .iter()
                .copied()
                .filter(|r| match measure {
                    Measure::Listen => r.listen_ms.is_some(),
                    Measure::Memory => r.settled_mem_bytes.is_some(),
                })
                .collect();
            let series = &series[series.len().saturating_sub(keep)..];
            if let Some(f) = assess(measure, ws, key, series, commit_log, cfg, now) {
                out.push(f);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn assess(
    measure: Measure,
    ws: &Path,
    key: &str,
    runs: &[&DevServer],
    commit_log: &[CommitEntry],
    cfg: &PerfConfig,
    now: u64,
) -> Option<Finding> {
    let value = |r: &DevServer| match measure {
        Measure::Listen => r.listen_ms.unwrap_or(0) as f64,
        Measure::Memory => r.settled_mem_bytes.unwrap_or(0) as f64,
    };
    let series: Vec<f64> = runs.iter().map(|r| value(r)).collect();
    let uncertainty = runs
        .iter()
        .filter_map(|r| r.listen_uncertainty_ms)
        .max()
        .unwrap_or(0) as f64;
    let t = match measure {
        // A time to listen is only as fine as the watch interval: a
        // difference smaller than its uncertainty is not a difference.
        Measure::Listen => Thresholds {
            baseline_runs: cfg.baseline_runs,
            slow_runs: cfg.slow_runs,
            ratio: cfg.slower_ratio,
            min_increase: (cfg.min_slowdown_secs as f64 * 1000.0).max(uncertainty),
        },
        Measure::Memory => Thresholds {
            baseline_runs: cfg.baseline_runs,
            slow_runs: cfg.slow_runs,
            ratio: cfg.slower_ratio,
            min_increase: MIN_HEAVIER_BYTES,
        },
    };
    let s = perf::slower(&series, &t)?;
    if measure == Measure::Listen && s.baseline <= uncertainty {
        // The usual time is below what the watch interval can resolve.
        return None;
    }
    let (normal, slow) = (runs[s.last_normal], runs[s.first_slow]);
    let latest = runs[runs.len() - 1];
    let (from_head, to_head) = (normal.head.clone(), slow.head.clone());
    let mut moves = match (&from_head, &to_head) {
        (Some(a), Some(b)) => commits::between(commit_log, ws, a, b),
        _ => Vec::new(),
    };
    let truncated = moves.len() > MAX_EVIDENCE_COMMITS;
    if truncated {
        moves.drain(..moves.len() - MAX_EVIDENCE_COMMITS);
    }
    let fmt = |v: u64| match measure {
        Measure::Listen => human_ms(v),
        Measure::Memory => crate::human_bytes(v),
    };
    let recent: Vec<u64> = s.recent.iter().map(|x| *x as u64).collect();
    let base = s.baseline as u64;
    let now_v = perf::median(&s.recent).unwrap_or(s.baseline) as u64;
    let place = ws
        .file_name()
        .map(|n| format!(" in {}", n.to_string_lossy()))
        .unwrap_or_default();
    let title = match measure {
        Measure::Listen => format!(
            "`{key}`{place} takes {} to start listening, {:.1}× its usual {}",
            fmt(now_v),
            s.ratio,
            fmt(base)
        ),
        Measure::Memory => format!(
            "`{key}`{place} settles at {} of memory, {:.1}× its usual {}",
            fmt(now_v),
            s.ratio,
            fmt(base)
        ),
    };
    let changed = match (&from_head, &to_head) {
        (Some(a), Some(b)) if commits::same_commit(a, b) => {
            "HEAD did not move between the last normal run and the first slow one, so the cause is outside the code: dependencies, caches, the toolchain, the environment or a busy machine.".to_string()
        }
        (Some(a), Some(b)) if moves.is_empty() => format!(
            "HEAD moved from {} to {} between the last normal run and the first slow one; Mulchie recorded none of the moves in between.",
            short(a),
            short(b)
        ),
        (Some(a), Some(b)) => {
            let who: Vec<String> = moves
                .iter()
                .filter_map(|m| m.agent.clone().or_else(|| m.session_id.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let by = if who.is_empty() {
                String::new()
            } else {
                format!(" while {} worked there", who.join(", "))
            };
            format!(
                "HEAD moved {} time(s) from {} to {}{by}: see evidence.devserver.commits.",
                moves.len() + usize::from(truncated),
                short(a),
                short(b)
            )
        }
        _ => "The workspace's HEAD is not known for those runs.".to_string(),
    };
    let how = match measure {
        Measure::Listen => format!(
            "The last {} runs each took more than {}× the median of the {} before them ({}) to start listening, and more than {} longer. The time is read from the watch run that first saw the port, so it is only as exact as the watch interval (up to {} here).",
            recent.len(),
            cfg.slower_ratio,
            s.baseline_runs,
            fmt(base),
            human_ms(t.min_increase as u64),
            human_ms(uncertainty as u64)
        ),
        Measure::Memory => format!(
            "The last {} runs each settled (median over minutes 2 to 5) at more than {}× the median of the {} before them ({}), and more than {} above it.",
            recent.len(),
            cfg.slower_ratio,
            s.baseline_runs,
            fmt(base),
            crate::human_bytes(MIN_HEAVIER_BYTES as u64)
        ),
    };
    let severity = if s.ratio >= 2.0 {
        Severity::Medium
    } else {
        Severity::Low
    };
    let kind = measure.kind();
    Some(Finding {
        id: finding_id(kind, ws, key),
        kind,
        category: FindingCategory::Performance,
        severity,
        title,
        why: format!("{how} {changed} Mulchie only reports this; it changes nothing."),
        evidence: Evidence {
            cmd: Some(key.to_string()),
            cwd: Some(latest.workspace.clone()),
            workspace: Some(ws.to_path_buf()),
            detail: Some(format!(
                "baseline {} over {} runs; recent runs {}",
                fmt(base),
                s.baseline_runs,
                recent
                    .iter()
                    .map(|m| fmt(*m))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            devserver: Some(Box::new(DevServerEvidence {
                command: key.to_string(),
                measure: measure.name().to_string(),
                baseline: base,
                baseline_runs: s.baseline_runs,
                recent,
                ratio: s.ratio,
                uncertainty_ms: (measure == Measure::Listen).then_some(uncertainty as u64),
                from_head,
                to_head,
                commits: moves,
                commits_truncated: truncated,
            })),
            ..Evidence::default()
        },
        suggested: vec![Suggestion::call(
            "Show this workspace's dev servers",
            ApiRequest::get(route(Some(ws))),
            true,
        )],
        first_seen_unix: slow.at_unix.min(now),
        last_seen_unix: now,
        dismissed: false,
        dismissed_until: None,
    })
}

fn short(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}

#[cfg(test)]
#[path = "devservers_tests.rs"]
mod tests;
