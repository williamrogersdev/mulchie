//! Watch: read-only detectors that turn facts about running processes and
//! Mulchie's own setup into findings — things that should probably not be
//! running (left over, runaway, exposed, oddly placed) and settings that are
//! wrong.
//!
//! Detectors never act. A finding suggests ready-to-plan [`Request`]s; acting
//! goes through the safety gate like everything else. This is not malware
//! detection: there are no signatures, hashes, network lookups or uploads.
//! Findings are observations ("is listening on all interfaces"), not verdicts.
//!
//! Inputs come in a [`WatchInput`] so every detector runs against fake tables
//! in tests. Probes that cost something (listening sockets, code signatures,
//! quarantine flags) sit behind [`WatchProbe`] and run on the scan loop only,
//! bounded in count and time.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::WatchConfig;
use crate::consumers::{outer_bundle, ProcSample, Severity};
use crate::monitor::ProcKind;
use crate::process::ProcFacts;
use crate::safety::{Request, Signal};

// ------------------------------------------------------------------ findings

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Reparented to pid 1 (its parent exited) and still running from a
    /// workspace or scan root.
    OrphanedProcess,
    /// Its working directory no longer exists (a deleted worktree).
    MissingWorkingDirectory,
    /// Its executable was deleted or replaced after it started.
    ExecutableChanged,
    /// An agent or build process older than `stale_process_hours`, idle.
    StaleProcess,
    /// Stopped (SIGSTOP) for longer than `stopped_hours`.
    StoppedProcess,
    /// Near a full core or more for `runaway_minutes`.
    RunawayCpu,
    /// Memory grew steadily by `memory_growth_mb` over `runaway_minutes`.
    GrowingMemory,
    /// Listening on all interfaces or a non-loopback address.
    ExposedListener,
    /// Running from a temporary or downloads directory.
    TempExecutable,
    /// Running from a hidden directory inside a workspace.
    HiddenDirExecutable,
    /// Uses the name of a system process but runs from elsewhere.
    SystemNameMismatch,
    /// An app bundle that is not signed, or only ad-hoc signed (macOS).
    UnsignedExecutable,
    /// A loose executable that still carries the download quarantine flag.
    QuarantinedExecutable,
    /// Mulchie's config directory or a token file is readable by others.
    InsecurePermissions,
    /// Mulchie's API is listening beyond loopback.
    ApiExposed,
    /// Another engine holds the engine lock; this one is not automating.
    EngineLockNotHeld,
    /// The config file was not used (parse or validation error).
    ConfigNotLoaded,
    /// An agent session stayed over a cap of its budget (`limits`).
    LimitExceeded,
    /// An agent session holds most of the machine while health is poor.
    SessionHog,
    /// A newer Mulchie release is out.
    MulchieUpdate,
    /// A newer version of an installed agent tool is out.
    AgentUpdate,
    /// Homebrew packages are outdated (local data only).
    HomebrewOutdated,
    /// A package in a workspace lockfile has a known vulnerability (OSV.dev).
    Advisory,
    /// `paused.json` could not be read at start: processes paused by an
    /// earlier engine may still be stopped.
    PausedListUnreadable,
}

impl FindingKind {
    pub fn code(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default()
    }

    /// Which family the kind belongs to.
    pub fn category(self) -> FindingCategory {
        use FindingKind::*;
        match self {
            ExecutableChanged
            | ExposedListener
            | TempExecutable
            | HiddenDirExecutable
            | SystemNameMismatch
            | UnsignedExecutable
            | QuarantinedExecutable => FindingCategory::Security,
            OrphanedProcess
            | MissingWorkingDirectory
            | StaleProcess
            | StoppedProcess
            | RunawayCpu
            | GrowingMemory => FindingCategory::Hygiene,
            InsecurePermissions | ApiExposed | EngineLockNotHeld | ConfigNotLoaded => {
                FindingCategory::Posture
            }
            LimitExceeded | SessionHog | PausedListUnreadable => FindingCategory::Limit,
            MulchieUpdate | AgentUpdate | HomebrewOutdated | Advisory => FindingCategory::Update,
        }
    }
}

/// The family of a finding: what kind of attention it asks for.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// Something about a process may be unsafe (exposed, unsigned, odd place).
    Security,
    /// Left over, runaway or stuck work.
    #[default]
    Hygiene,
    /// Mulchie's own setup.
    Posture,
    /// An agent session over its budget or hogging the machine.
    Limit,
    /// Updates and advisories.
    Update,
}

/// macOS code signature of an executable, as `codesign` reports it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SigStatus {
    Apple,
    AppStore,
    DeveloperId,
    /// Apple Development certificate (a developer's own build).
    Development,
    /// Signed by someone else (an enterprise certificate, …).
    OtherSigned,
    AdHoc,
    Unsigned,
    /// `codesign` could not tell (timeout, not a Mach-O, …).
    Unknown,
}

impl SigStatus {
    pub fn weak(self) -> bool {
        matches!(self, SigStatus::AdHoc | SigStatus::Unsigned)
    }
}

/// A listening TCP socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Listener {
    pub pid: u32,
    /// The bound address as shown (`*`, `0.0.0.0`, `::`, `127.0.0.1`, …).
    pub addr: String,
    pub port: u16,
}

impl Listener {
    pub fn loopback(&self) -> bool {
        let a = self.addr.trim_start_matches('[').trim_end_matches(']');
        a.starts_with("127.") || a == "::1" || a == "localhost" || a == "::ffff:127.0.0.1"
    }

    pub fn shown(&self) -> String {
        if self.addr.contains(':') && !self.addr.starts_with('[') {
            format!("[{}]:{}", self.addr, self.port)
        } else {
            format!("{}:{}", self.addr, self.port)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ParentInfo {
    pub pid: u32,
    pub name: String,
}

/// What a finding is based on. Fields that do not apply are left out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Evidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<PathBuf>,
    /// The command line, secrets redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<SigStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    /// A file or directory (posture findings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Anything else, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Something the caller can do about a finding. `request` goes to a plan
/// route (`POST /v1/process/plan` or straight to the gate through
/// `POST /v1/process/execute` after review); `api` names another route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Suggestion {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Request>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Whether it can be undone (SIGSTOP can, SIGTERM cannot).
    pub reversible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    /// Stable while the cause lasts: `<kind>:<pid>:<start_time>` for a
    /// process, `<kind>:<path or name>` otherwise.
    pub id: String,
    pub kind: FindingKind,
    /// Always `kind.category()`; set when findings are finalized.
    #[serde(default)]
    pub category: FindingCategory,
    pub severity: Severity,
    /// One sentence, an observation.
    pub title: String,
    /// Why it may matter, in plain words.
    pub why: String,
    pub evidence: Evidence,
    pub suggested: Vec<Suggestion>,
    pub first_seen_unix: u64,
    pub last_seen_unix: u64,
    pub dismissed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_until: Option<u64>,
}

/// Open findings by severity (dismissed ones counted apart).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FindingCounts {
    pub info: usize,
    pub low: usize,
    pub medium: usize,
    pub high: usize,
    pub dismissed: usize,
}

impl FindingCounts {
    pub fn of(findings: &[Finding]) -> Self {
        let mut c = Self::default();
        for f in findings {
            if f.dismissed {
                c.dismissed += 1;
                continue;
            }
            match f.severity {
                Severity::Info => c.info += 1,
                Severity::Low => c.low += 1,
                Severity::Medium => c.medium += 1,
                Severity::High => c.high += 1,
            }
        }
        c
    }
}

// ------------------------------------------------------------------- tracker

const TRACK_MINUTES: usize = 120;

#[derive(Debug, Clone, Default)]
struct Minute {
    minute: u64,
    cpu_sum: f64,
    n: u32,
    mem_max: u64,
}

#[derive(Debug, Clone, Default)]
struct Track {
    minutes: VecDeque<Minute>,
    stopped_since: Option<u64>,
}

/// Per-process history from the sample loop: per-minute CPU averages and
/// peak memory for up to two hours, and since when a process is stopped.
/// Cheap: one map update per process per sample.
#[derive(Debug, Default)]
pub struct Tracker {
    procs: HashMap<(u32, u64), Track>,
}

/// What the tracker knows about one process over a window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrackView {
    /// Whole minutes with samples in the window.
    pub minutes: usize,
    pub avg_cpu: f64,
    pub mem_first: u64,
    pub mem_last: u64,
    /// Share of minute-to-minute steps in which memory did not fall.
    pub mem_rising_share: f64,
    pub stopped_secs: Option<u64>,
}

impl Tracker {
    /// One sample of every process; processes not in it are forgotten.
    pub fn observe<'a>(&mut self, now: u64, procs: impl IntoIterator<Item = &'a ProcSample>) {
        let minute = now / 60;
        let mut seen = HashSet::with_capacity(self.procs.len());
        for p in procs {
            let key = (p.pid, p.start_time);
            seen.insert(key);
            let t = self.procs.entry(key).or_default();
            match t.minutes.back_mut() {
                Some(m) if m.minute == minute => {
                    m.cpu_sum += p.cpu_pct as f64;
                    m.n += 1;
                    m.mem_max = m.mem_max.max(p.mem_bytes);
                }
                _ => {
                    t.minutes.push_back(Minute {
                        minute,
                        cpu_sum: p.cpu_pct as f64,
                        n: 1,
                        mem_max: p.mem_bytes,
                    });
                    while t.minutes.len() > TRACK_MINUTES {
                        t.minutes.pop_front();
                    }
                }
            }
            if p.stopped {
                t.stopped_since.get_or_insert(now);
            } else {
                t.stopped_since = None;
            }
        }
        self.procs.retain(|k, _| seen.contains(k));
    }

    /// The last `window_mins` minutes of `(pid, start_time)`.
    pub fn view(&self, pid: u32, start_time: u64, now: u64, window_mins: u64) -> Option<TrackView> {
        let t = self.procs.get(&(pid, start_time))?;
        let from = (now / 60).saturating_sub(window_mins.saturating_sub(1));
        let ms: Vec<&Minute> = t.minutes.iter().filter(|m| m.minute >= from).collect();
        let stopped_secs = t.stopped_since.map(|s| now.saturating_sub(s));
        if ms.is_empty() {
            return Some(TrackView {
                stopped_secs,
                ..Default::default()
            });
        }
        let avg_cpu = ms
            .iter()
            .map(|m| m.cpu_sum / m.n.max(1) as f64)
            .sum::<f64>()
            / ms.len() as f64;
        let steps = ms.len().saturating_sub(1);
        let rising = ms
            .windows(2)
            .filter(|w| w[1].mem_max >= w[0].mem_max)
            .count();
        Some(TrackView {
            minutes: ms.len(),
            avg_cpu,
            mem_first: ms[0].mem_max,
            mem_last: ms[ms.len() - 1].mem_max,
            mem_rising_share: if steps == 0 {
                0.0
            } else {
                rising as f64 / steps as f64
            },
            stopped_secs,
        })
    }

    pub fn len(&self) -> usize {
        self.procs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// Tests: pretend a process was stopped since `at`.
    #[cfg(test)]
    pub(crate) fn set_stopped_since(&mut self, pid: u32, start_time: u64, at: u64) {
        self.procs
            .entry((pid, start_time))
            .or_default()
            .stopped_since = Some(at);
    }
}

// --------------------------------------------------------------------- probes

/// The costly reads, behind a trait so tests use fakes. Each runs on the scan
/// loop only.
pub trait WatchProbe: Send + Sync {
    /// Listening TCP sockets of the user's processes; `None` when unavailable.
    fn listeners(&self) -> Option<Vec<Listener>>;
    /// The code signature of an executable (macOS); `None` elsewhere.
    fn signature(&self, exe: &Path) -> Option<SigStatus>;
    /// Whether an executable carries the download quarantine flag (macOS).
    fn quarantined(&self, exe: &Path) -> bool;
}

/// No probes: listeners unknown, nothing signed or quarantined.
pub struct NoProbe;

impl WatchProbe for NoProbe {
    fn listeners(&self) -> Option<Vec<Listener>> {
        None
    }
    fn signature(&self, _: &Path) -> Option<SigStatus> {
        None
    }
    fn quarantined(&self, _: &Path) -> bool {
        false
    }
}

/// The real probes: `lsof` (macOS) or `/proc/net/tcp*` (Linux) for listeners,
/// `codesign` and the `com.apple.quarantine` attribute on macOS.
pub struct SystemProbe;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CODESIGN_TIMEOUT: Duration = Duration::from_secs(2);

impl WatchProbe for SystemProbe {
    fn listeners(&self) -> Option<Vec<Listener>> {
        #[cfg(target_os = "linux")]
        {
            return linux_listeners();
        }
        #[allow(unreachable_code)]
        {
            let mut cmd = std::process::Command::new("lsof");
            cmd.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"]);
            let out = crate::git::run_with_timeout(cmd, PROBE_TIMEOUT).ok()?;
            lsof_result(out.success, &out.stdout)
        }
    }

    fn signature(&self, exe: &Path) -> Option<SigStatus> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let mut cmd = std::process::Command::new("/usr/bin/codesign");
        cmd.args(["-dv", "--verbose=2"]).arg(exe);
        codesign_result(crate::git::run_with_timeout(cmd, CODESIGN_TIMEOUT).map(|o| o.stderr))
    }

    fn quarantined(&self, exe: &Path) -> bool {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let Ok(p) = std::ffi::CString::new(exe.as_os_str().as_bytes()) else {
                return false;
            };
            let name = c"com.apple.quarantine";
            let r =
                unsafe { libc::getxattr(p.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
            return r >= 0;
        }
        #[allow(unreachable_code)]
        {
            let _ = exe;
            false
        }
    }
}

/// Listeners from an `lsof` run. Only a successful run counts: lsof also
/// exits 1 when nothing listens, but a failure looks the same, so a
/// non-zero exit is "unknown" (`None`) and earlier findings are kept.
pub fn lsof_result(success: bool, stdout: &[u8]) -> Option<Vec<Listener>> {
    success.then(|| parse_lsof(&String::from_utf8_lossy(stdout)))
}

/// The status from a `codesign` run. A timeout or spawn error is `None`,
/// never [`SigStatus::Unknown`]: it is not cached and is tried again.
pub fn codesign_result<E>(stderr: Result<String, E>) -> Option<SigStatus> {
    stderr.ok().map(|e| parse_codesign(&e))
}

/// While listeners are unknown, keep the listener findings of the last run:
/// API exposure, and exposed listeners of processes still running (same pid
/// and start time).
pub fn carry_listener_findings(
    raw: &mut Vec<Finding>,
    prev: &[Finding],
    alive: &dyn Fn(u32, u64) -> bool,
) {
    for f in prev {
        let keep = match f.kind {
            FindingKind::ApiExposed => true,
            FindingKind::ExposedListener => match (f.evidence.pid, f.evidence.start_time) {
                (Some(p), Some(s)) => alive(p, s),
                _ => false,
            },
            _ => false,
        };
        if keep && !raw.iter().any(|r| r.id == f.id) {
            raw.push(f.clone());
        }
    }
}

/// `lsof -F pn` output: `p<pid>` lines, then `n<addr>:<port>` lines.
pub fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    let mut pid = 0u32;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix('n') {
            let Some((addr, port)) = n.rsplit_once(':') else {
                continue;
            };
            let Ok(port) = port.parse() else { continue };
            let addr = addr
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            if pid > 0 {
                out.push(Listener { pid, addr, port });
            }
        }
    }
    out.sort_by(|a, b| (a.pid, a.port, &a.addr).cmp(&(b.pid, b.port, &b.addr)));
    out.dedup();
    out
}

/// `codesign -dv --verbose=2` stderr → a status.
pub fn parse_codesign(stderr: &str) -> SigStatus {
    if stderr.contains("not signed at all") {
        return SigStatus::Unsigned;
    }
    if stderr.contains("Signature=adhoc") {
        return SigStatus::AdHoc;
    }
    let auth = |s: &str| {
        stderr
            .lines()
            .any(|l| l.starts_with("Authority=") && l.contains(s))
    };
    if auth("Software Signing") {
        SigStatus::Apple
    } else if auth("Apple Mac OS Application Signing") {
        SigStatus::AppStore
    } else if auth("Developer ID Application") {
        SigStatus::DeveloperId
    } else if auth("Apple Development") {
        SigStatus::Development
    } else if stderr.lines().any(|l| l.starts_with("Authority=")) {
        SigStatus::OtherSigned
    } else {
        SigStatus::Unknown
    }
}

/// `/proc/net/tcp` or `tcp6` text → (address, port, inode) of LISTEN sockets.
pub fn parse_proc_net_tcp(text: &str, v6: bool) -> Vec<(String, u16, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 || cols[3] != "0A" {
            continue;
        }
        let Some((a, p)) = cols[1].split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(p, 16) else {
            continue;
        };
        let Ok(inode) = cols[9].parse() else { continue };
        let addr = if v6 { hex_v6(a) } else { hex_v4(a) };
        if let Some(addr) = addr {
            out.push((addr, port, inode));
        }
    }
    out
}

fn hex_v4(a: &str) -> Option<String> {
    let n = u32::from_str_radix(a, 16).ok()?;
    Some(std::net::Ipv4Addr::from(n.to_le_bytes()).to_string())
}

fn hex_v6(a: &str) -> Option<String> {
    if a.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.chunks_mut(4).enumerate() {
        let w = u32::from_str_radix(&a[i * 8..i * 8 + 8], 16).ok()?;
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    let ip = std::net::Ipv6Addr::from(bytes);
    Some(match ip.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => ip.to_string(),
    })
}

#[cfg(target_os = "linux")]
fn linux_listeners() -> Option<Vec<Listener>> {
    let mut socks: HashMap<u64, (String, u16)> = HashMap::new();
    for (f, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(t) = std::fs::read_to_string(f) {
            for (a, p, i) in parse_proc_net_tcp(&t, v6) {
                socks.insert(i, (a, p));
            }
        }
    }
    if socks.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut budget = 50_000usize;
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            budget = budget.checked_sub(1)?;
            let Ok(t) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let t = t.to_string_lossy();
            if let Some(i) = t
                .strip_prefix("socket:[")
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|r| r.parse::<u64>().ok())
            {
                if let Some((a, p)) = socks.get(&i) {
                    out.push(Listener {
                        pid,
                        addr: a.clone(),
                        port: *p,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| (a.pid, a.port, &a.addr).cmp(&(b.pid, b.port, &b.addr)));
    out.dedup();
    Some(out)
}

/// Signature results remembered by (path, mtime, size) so each executable is
/// checked once.
#[derive(Debug, Default)]
pub struct SigCache {
    map: HashMap<PathBuf, (i64, u64, SigStatus)>,
}

impl SigCache {
    pub fn get(&self, exe: &Path) -> Option<SigStatus> {
        let (mt, sz) = mtime_size(exe)?;
        self.map
            .get(exe)
            .filter(|(m, s, _)| *m == mt && *s == sz)
            .map(|e| e.2)
    }

    pub fn put(&mut self, exe: &Path, st: SigStatus) {
        if let Some((mt, sz)) = mtime_size(exe) {
            if self.map.len() > 5000 {
                self.map.clear();
            }
            self.map.insert(exe.to_path_buf(), (mt, sz, st));
        }
    }
}

/// A yes/no fact about a file (the quarantine flag), kept while the file's
/// status-change time and size stay the same (setting or removing an
/// extended attribute changes the ctime).
#[derive(Debug, Default)]
pub struct FlagCache {
    map: HashMap<PathBuf, (i64, u64, bool)>,
}

impl FlagCache {
    pub fn get(&self, p: &Path) -> Option<bool> {
        let (ct, sz) = ctime_size(p)?;
        self.map
            .get(p)
            .filter(|(c, s, _)| *c == ct && *s == sz)
            .map(|e| e.2)
    }

    pub fn put(&mut self, p: &Path, v: bool) {
        if let Some((ct, sz)) = ctime_size(p) {
            if self.map.len() > 5000 {
                self.map.clear();
            }
            self.map.insert(p.to_path_buf(), (ct, sz, v));
        }
    }
}

fn ctime_size(p: &Path) -> Option<(i64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.ctime(), m.len()))
}

fn mtime_size(p: &Path) -> Option<(i64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.mtime(), m.len()))
}

// ---------------------------------------------------------------------- input

/// Mulchie's own setup, read by the engine.
#[derive(Debug, Clone, Default)]
pub struct Posture {
    pub config_dir: PathBuf,
    /// Permission bits of the config directory, when it exists.
    pub config_dir_mode: Option<u32>,
    /// Token files and their permission bits.
    pub token_modes: Vec<(PathBuf, u32)>,
    /// The engine's API port, when it serves one.
    pub api_port: Option<u16>,
    /// Another engine holds the lock (its pid).
    pub lock_held_by_other: Option<u32>,
    pub config_error: Option<String>,
}

pub struct WatchInput<'a> {
    pub now: u64,
    pub cfg: &'a WatchConfig,
    /// Every process, read on the scan loop (exe, cwd, argv).
    pub procs: &'a [ProcFacts],
    /// The latest sample by pid (kind, CPU, memory, workspace).
    pub samples: &'a HashMap<u32, ProcSample>,
    pub tracker: &'a Tracker,
    pub listeners: Option<&'a [Listener]>,
    pub signatures: &'a HashMap<PathBuf, SigStatus>,
    pub quarantined: &'a HashSet<PathBuf>,
    pub uid: u32,
    pub self_pid: u32,
    pub home: &'a Path,
    /// Canonical scan roots.
    pub roots: &'a [PathBuf],
    /// Temporary and download directories, canonical where possible.
    pub temp_dirs: &'a [PathBuf],
    pub posture: &'a Posture,
    /// Processes the service manager started (launchd or systemd jobs; see
    /// [`started_by_service_manager`]): never orphans.
    pub service_started: &'a HashSet<u32>,
    /// Pids of `systemd --user`, which adopts orphans of the user session.
    pub subreapers: &'a [u32],
}

/// Whether the service manager started `pid` rather than a shell that has
/// since exited. A launchd or systemd job runs in launchd's session or its
/// own (session id 1 or its pid); an orphan from a terminal keeps the
/// session of the shell that started it. On Linux a `*.service` cgroup also
/// counts. When the session cannot be read the answer is yes (no finding).
pub fn started_by_service_manager(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Ok(cg) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
            let service = cg.lines().any(|l| {
                l.rsplit('/')
                    .next()
                    .is_some_and(|c| c.trim().ends_with(".service"))
            });
            if service {
                return true;
            }
        }
    }
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return true;
    };
    let sid = unsafe { libc::getsid(p) };
    sid < 0 || sid == 1 || sid == p
}

/// Whether a path is gone: only "not found" counts. A permission error or
/// any other failure is unknown (`None`), which raises no finding.
fn path_gone(p: &Path) -> Option<bool> {
    match std::fs::symlink_metadata(p) {
        Ok(_) => Some(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
        Err(_) => None,
    }
}

/// Temporary and downloads directories for `home`.
pub fn temp_dirs(home: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = vec![
        "/tmp".into(),
        "/private/tmp".into(),
        "/var/tmp".into(),
        "/private/var/tmp".into(),
        "/var/folders".into(),
        "/private/var/folders".into(),
        "/dev/shm".into(),
        home.join("Downloads"),
    ];
    if let Some(t) = std::env::var_os("TMPDIR") {
        v.push(PathBuf::from(t));
    }
    let canon: Vec<PathBuf> = v.iter().filter_map(|p| p.canonicalize().ok()).collect();
    v.extend(canon);
    v.sort();
    v.dedup();
    v
}

// ----------------------------------------------------------------- detectors

/// Well-known system process names (macOS and Linux) and where they live.
const SYSTEM_NAMES: &[&str] = &[
    "launchd",
    "kernel_task",
    "WindowServer",
    "loginwindow",
    "Finder",
    "Dock",
    "SystemUIServer",
    "mds",
    "mds_stores",
    "mdworker",
    "coreaudiod",
    "syslogd",
    "cfprefsd",
    "trustd",
    "securityd",
    "opendirectoryd",
    "systemd",
    "sshd",
    "cron",
    "crond",
    "dbus-daemon",
    "kworker",
    "kthreadd",
    "rsyslogd",
    "polkitd",
    "NetworkManager",
];

const SYSTEM_DIRS: &[&str] = &[
    "/System/",
    "/usr/libexec/",
    "/usr/sbin/",
    "/usr/bin/",
    "/sbin/",
    "/bin/",
    "/usr/lib/",
    "/lib/",
    "/usr/local/sbin/",
];

/// Hidden directory names that hold ordinary tools in a workspace.
const ORDINARY_HIDDEN: &[&str] = &[
    ".venv",
    ".bin",
    ".build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cargo",
    ".rustup",
    ".nvm",
    ".bun",
    ".deno",
    ".local",
    ".pyenv",
    ".cache",
    ".claude",
    ".pnpm",
    ".yarn",
    ".gradle",
    ".m2",
    ".tox",
    ".nox",
    ".direnv",
    ".devenv",
    ".mise",
    ".asdf",
    ".volta",
    ".expo",
    ".angular",
    ".parcel-cache",
    ".vercel",
    ".netlify",
    ".wrangler",
    ".swiftpm",
];

/// Parents that run programs from temporary directories as a matter of course
/// (`go run`, test runners, Xcode previews).
const TEMP_RUNNERS: &[&str] = &[
    "go",
    "cargo",
    "node",
    "python",
    "python3",
    "xcodebuild",
    "swift",
    "dotnet",
    "java",
    "bun",
    "deno",
    "npx",
    "pnpm",
    "yarn",
    "make",
    "bazel",
];

fn exe_base(p: &ProcFacts) -> String {
    p.exe
        .as_deref()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The exe path as given, without Linux's ` (deleted)` suffix, and whether
/// that suffix was there.
fn exe_path(p: &ProcFacts) -> Option<(PathBuf, bool)> {
    let e = p.exe.as_deref()?;
    let s = e.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(t) => Some((PathBuf::from(t), true)),
        None => Some((e.to_path_buf(), false)),
    }
}

struct Det<'a> {
    i: &'a WatchInput<'a>,
    by_pid: HashMap<u32, &'a ProcFacts>,
    by_exe_listen: HashMap<u32, Vec<&'a Listener>>,
    out: Vec<Finding>,
}

fn under(p: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| p.starts_with(d))
}

fn fmt_age(s: u64) -> String {
    if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 172_800 {
        format!("{} h", s / 3600)
    } else {
        format!("{} days", s / 86_400)
    }
}

impl<'a> Det<'a> {
    fn evidence(&self, p: &ProcFacts) -> Evidence {
        let s = self.i.samples.get(&p.pid);
        Evidence {
            pid: Some(p.pid),
            start_time: Some(p.start_time),
            name: Some(p.name.clone()),
            exe: p.exe.clone(),
            cmd: (!p.cmd.is_empty()).then(|| crate::redact::redact_args(&p.cmd).join(" ")),
            cwd: p.cwd.clone(),
            workspace: s.and_then(|s| s.workspace.as_ref().map(|w| w.0.clone())),
            age_secs: (p.start_time > 0).then(|| self.i.now.saturating_sub(p.start_time)),
            parent: self.by_pid.get(&p.ppid).map(|pp| ParentInfo {
                pid: pp.pid,
                name: pp.name.clone(),
            }),
            listen: self.by_exe_listen.get(&p.pid).map(|v| {
                let mut l: Vec<String> = v.iter().map(|l| l.shown()).collect();
                l.sort();
                l.dedup();
                l
            }),
            signature: p
                .exe
                .as_deref()
                .and_then(|e| self.i.signatures.get(e).copied()),
            quarantined: p
                .exe
                .as_deref()
                .filter(|e| self.i.quarantined.contains(*e))
                .map(|_| true),
            cpu_pct: s.map(|s| s.cpu_pct),
            mem_bytes: s.map(|s| s.mem_bytes),
            ..Default::default()
        }
    }

    fn push_proc(
        &mut self,
        p: &ProcFacts,
        kind: FindingKind,
        severity: Severity,
        title: String,
        why: &str,
        suggested: Vec<Suggestion>,
    ) {
        let evidence = self.evidence(p);
        self.out.push(Finding {
            id: format!("{}:{}:{}", kind.code(), p.pid, p.start_time),
            category: kind.category(),
            kind,
            severity,
            title,
            why: why.into(),
            evidence,
            suggested,
            first_seen_unix: self.i.now,
            last_seen_unix: self.i.now,
            dismissed: false,
            dismissed_until: None,
        });
    }

    fn push_other(
        &mut self,
        id: &str,
        kind: FindingKind,
        severity: Severity,
        title: String,
        why: &str,
        evidence: Evidence,
    ) {
        self.out.push(Finding {
            id: format!("{}:{id}", kind.code()),
            category: kind.category(),
            kind,
            severity,
            title,
            why: why.into(),
            evidence,
            suggested: Vec::new(),
            first_seen_unix: self.i.now,
            last_seen_unix: self.i.now,
            dismissed: false,
            dismissed_until: None,
        });
    }

    fn kind_of(&self, p: &ProcFacts) -> ProcKind {
        self.i
            .samples
            .get(&p.pid)
            .map(|s| s.kind)
            .unwrap_or(ProcKind::Other)
    }

    fn in_workspace_or_root(&self, p: &ProcFacts) -> bool {
        self.i
            .samples
            .get(&p.pid)
            .is_some_and(|s| s.workspace.is_some())
            || p.cwd.as_deref().is_some_and(|c| under(c, self.i.roots))
    }
}

fn quit(p: &ProcFacts) -> Suggestion {
    Suggestion {
        label: format!("Ask {} to quit (SIGTERM)", p.name),
        request: Some(Request::Signal {
            pid: p.pid,
            signal: Signal::Term,
            expected_start_time: p.start_time,
        }),
        api: None,
        reversible: false,
    }
}

fn pause(p: &ProcFacts) -> Suggestion {
    Suggestion {
        label: format!("Pause {} (SIGSTOP; resume later)", p.name),
        request: Some(Request::Signal {
            pid: p.pid,
            signal: Signal::Stop,
            expected_start_time: p.start_time,
        }),
        api: None,
        reversible: true,
    }
}

fn resume(p: &ProcFacts) -> Suggestion {
    Suggestion {
        label: format!("Resume {} (SIGCONT)", p.name),
        request: Some(Request::Signal {
            pid: p.pid,
            signal: Signal::Cont,
            expected_start_time: p.start_time,
        }),
        api: None,
        reversible: true,
    }
}

fn lower(p: &ProcFacts) -> Suggestion {
    Suggestion {
        label: format!("Lower the priority of {} (nice 10)", p.name),
        request: Some(Request::Renice {
            pid: p.pid,
            nice: 10,
            expected_start_time: p.start_time,
        }),
        api: None,
        // Raising priority again needs privilege.
        reversible: false,
    }
}

fn dismiss(id_hint: &str) -> Suggestion {
    Suggestion {
        label: "Dismiss this finding for 30 days".into(),
        request: None,
        api: Some(format!(
            "POST /v1/findings/dismiss {{\"id\": \"{id_hint}\"}}"
        )),
        reversible: true,
    }
}

/// Run every detector. The result is raw: allow-lists and dismissals are
/// applied by [`finalize`].
pub fn detect(i: &WatchInput) -> Vec<Finding> {
    let mut d = Det {
        i,
        by_pid: i.procs.iter().map(|p| (p.pid, p)).collect(),
        by_exe_listen: HashMap::new(),
        out: Vec::new(),
    };
    if let Some(ls) = i.listeners {
        for l in ls {
            d.by_exe_listen.entry(l.pid).or_default().push(l);
        }
    }
    let own: Vec<&ProcFacts> = i
        .procs
        .iter()
        .filter(|p| p.uid == Some(i.uid) && p.pid != i.self_pid && p.pid > 1)
        .collect();
    for p in &own {
        detect_leftovers(&mut d, p);
        detect_activity(&mut d, p);
        detect_origin(&mut d, p);
        detect_exposure(&mut d, p);
    }
    detect_posture(&mut d);
    // One finding per id (a process can only produce each kind once anyway).
    let mut seen = HashSet::new();
    d.out.retain(|f| seen.insert(f.id.clone()));
    d.out
}

fn detect_leftovers(d: &mut Det, p: &ProcFacts) {
    let i = d.i;
    // Orphaned: parent exited (reparented to launchd/init, or on Linux to the
    // `systemd --user` subreaper), not a service the service manager started,
    // still working in a workspace or scan root, and a kind people start by
    // hand.
    let kind = d.kind_of(p);
    if (p.ppid == 1 || i.subreapers.contains(&p.ppid))
        && !i.service_started.contains(&p.pid)
        && d.in_workspace_or_root(p)
        && matches!(kind, ProcKind::Agent | ProcKind::Build | ProcKind::Runtime)
    {
        let age = i.now.saturating_sub(p.start_time);
        let sev = if age > i.cfg.stale_process_hours * 3600 {
            Severity::Medium
        } else {
            Severity::Low
        };
        d.push_proc(
            p,
            FindingKind::OrphanedProcess,
            sev,
            format!("{} (pid {}) is still running from a workspace after its parent exited", p.name, p.pid),
            "Its terminal or agent is gone, so nothing is watching it; it may be a dev server or build left behind that keeps using CPU, memory or a port.",
            vec![quit(p), pause(p)],
        );
    }
    // Working directory gone.
    if let Some(cwd) = p.cwd.as_deref() {
        let s = cwd.to_string_lossy();
        let deleted =
            s.ends_with(" (deleted)") || (cwd.is_absolute() && path_gone(cwd) == Some(true));
        if deleted && (under(cwd, i.roots) || cwd.starts_with(i.home)) {
            d.push_proc(
                p,
                FindingKind::MissingWorkingDirectory,
                Severity::Medium,
                format!("{} (pid {}) is working in a directory that no longer exists", p.name, p.pid),
                "Its working directory (often a removed worktree) was deleted while it ran; it is almost certainly left over and may hold disk space until it exits.",
                vec![quit(p)],
            );
        }
    }
    // Executable deleted or replaced after start.
    if let Some((exe, deleted_mark)) = exe_path(p) {
        let state = if deleted_mark {
            Some(true)
        } else if exe.is_absolute() {
            path_gone(&exe)
        } else {
            Some(false)
        };
        let gone = state == Some(true);
        let replaced = state == Some(false)
            && p.start_time > 0
            && std::fs::metadata(&exe)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|t| t.as_secs() > p.start_time + 60);
        if gone || replaced {
            // Only deletion is certain; a newer mtime says the file was
            // modified, not that the running code differs.
            let (how, why) = if gone {
                (
                    "a file that was deleted",
                    "The file it was started from is no longer on disk (an update, rebuild or uninstall); restart it to run the current version.",
                )
            } else {
                (
                    "a file that was modified",
                    "The file it was started from was modified after the process started (usually an update or rebuild), so it may be running an older version; restart it to run the current one.",
                )
            };
            let sev = if under(&exe, i.temp_dirs) {
                Severity::Medium
            } else {
                Severity::Low
            };
            d.push_proc(
                p,
                FindingKind::ExecutableChanged,
                sev,
                format!(
                    "The program {} (pid {}) runs from {how} after it started",
                    p.name, p.pid
                ),
                why,
                vec![quit(p)],
            );
        }
    }
    // Stale agent or build process: old and idle.
    if matches!(kind, ProcKind::Agent | ProcKind::Build) && p.start_time > 0 {
        let age = i.now.saturating_sub(p.start_time);
        if age > i.cfg.stale_process_hours * 3600 {
            if let Some(v) = i
                .tracker
                .view(p.pid, p.start_time, i.now, TRACK_MINUTES as u64)
            {
                if v.minutes >= 30 && v.avg_cpu < i.cfg.stale_idle_cpu_pct as f64 {
                    d.push_proc(
                        p,
                        FindingKind::StaleProcess,
                        Severity::Low,
                        format!(
                            "{} (pid {}) has been running for {} and was idle for the last {} min",
                            p.name,
                            p.pid,
                            fmt_age(age),
                            v.minutes
                        ),
                        "An agent or build process this old that does nothing is usually forgotten; it still holds memory and possibly a build lock.",
                        vec![quit(p)],
                    );
                }
            }
        }
    }
    // Stopped for a long time.
    if let Some(v) = i.tracker.view(p.pid, p.start_time, i.now, 1) {
        if let Some(s) = v.stopped_secs {
            if s > i.cfg.stopped_hours * 3600 {
                d.push_proc(
                    p,
                    FindingKind::StoppedProcess,
                    Severity::Low,
                    format!("{} (pid {}) has been paused for {}", p.name, p.pid, fmt_age(s)),
                    "A stopped process keeps its memory and any locks or ports; it was probably paused and forgotten.",
                    vec![resume(p), quit(p)],
                );
            }
        }
    }
}

fn detect_activity(d: &mut Det, p: &ProcFacts) {
    let i = d.i;
    let w = i.cfg.runaway_minutes;
    let Some(v) = i.tracker.view(p.pid, p.start_time, i.now, w) else {
        return;
    };
    // Need nearly the whole window.
    if (v.minutes as u64) + 2 < w {
        return;
    }
    if v.avg_cpu >= i.cfg.runaway_cpu_pct as f64 {
        d.push_proc(
            p,
            FindingKind::RunawayCpu,
            Severity::Medium,
            format!(
                "{} (pid {}) has used {:.0}% of a core on average for {} min",
                p.name, p.pid, v.avg_cpu, v.minutes
            ),
            "Constant full-core use for this long is either a long job or a process stuck in a loop; it slows everything else.",
            vec![lower(p), pause(p), quit(p)],
        );
    }
    let grew = v.mem_last.saturating_sub(v.mem_first);
    if grew >= i.cfg.memory_growth_mb * 1024 * 1024 && v.mem_rising_share >= 0.8 {
        d.push_proc(
            p,
            FindingKind::GrowingMemory,
            Severity::Medium,
            format!(
                "{} (pid {}) grew by {} of memory in {} min and keeps growing",
                p.name,
                p.pid,
                crate::human_bytes(grew),
                v.minutes
            ),
            "Memory that rises steadily without falling back is the shape of a leak; left alone it pushes the machine into swap.",
            vec![quit(p)],
        );
    }
}

fn detect_origin(d: &mut Det, p: &ProcFacts) {
    let i = d.i;
    let Some((exe, _)) = exe_path(p) else { return };
    if i.cfg.allow_exes.iter().any(|a| a == &exe) {
        return;
    }
    let sig = i.signatures.get(&exe).copied();
    let quarantined = i.quarantined.contains(&exe);
    let parent_runner = d.by_pid.get(&p.ppid).is_some_and(|pp| {
        TEMP_RUNNERS.contains(&exe_base(pp).as_str()) || TEMP_RUNNERS.contains(&pp.name.as_str())
    });
    let exposed = d
        .by_exe_listen
        .get(&p.pid)
        .is_some_and(|v| v.iter().any(|l| !l.loopback()));
    // Temporary or downloads directory.
    if under(&exe, i.temp_dirs) {
        let sev = if exposed || quarantined || sig.is_some_and(SigStatus::weak) && !parent_runner {
            Severity::High
        } else if parent_runner {
            Severity::Low
        } else {
            Severity::Medium
        };
        d.push_proc(
            p,
            FindingKind::TempExecutable,
            sev,
            format!("{} (pid {}) runs from a temporary or downloads folder", p.name, p.pid),
            "Programs installed on purpose live in an Applications or bin folder; something running from a temporary or Downloads folder was started from a download or by another program, so check that you know what it is.",
            vec![pause(p), quit(p)],
        );
    }
    // Hidden directory inside a workspace or scan root.
    if under(&exe, i.roots) {
        let root = i
            .roots
            .iter()
            .find(|r| exe.starts_with(r))
            .cloned()
            .unwrap_or_default();
        let rel = exe.strip_prefix(&root).unwrap_or(&exe);
        let hidden = rel
            .parent()
            .into_iter()
            .flat_map(|p| p.components())
            .find(|c| {
                let s = c.as_os_str().to_string_lossy();
                s.starts_with('.') && s.len() > 1 && !ORDINARY_HIDDEN.contains(&s.as_ref())
            });
        if let Some(h) = hidden {
            d.push_proc(
                p,
                FindingKind::HiddenDirExecutable,
                Severity::Medium,
                format!(
                    "{} (pid {}) runs from a hidden folder ({}) inside your code",
                    p.name,
                    p.pid,
                    h.as_os_str().to_string_lossy()
                ),
                "Build tools keep their output in ordinary folders; a program started from a hidden folder in a project is unusual and worth a look.",
                vec![pause(p), quit(p)],
            );
        }
    }
    // A system name from somewhere else.
    let base = exe_base(p);
    let claims = SYSTEM_NAMES
        .iter()
        .find(|n| p.name == **n || base == **n)
        .copied();
    if let Some(n) = claims {
        let s = exe.to_string_lossy();
        if !SYSTEM_DIRS.iter().any(|d| s.starts_with(d)) {
            d.push_proc(
                p,
                FindingKind::SystemNameMismatch,
                Severity::High,
                format!("A process named {n} (pid {}) runs from {}", p.pid, exe.display()),
                "This name belongs to an operating-system program, which runs from a system folder; a copy elsewhere, running as you, is unusual and is a pattern malware uses to blend in.",
                vec![pause(p), quit(p)],
            );
        }
    }
    // Unsigned or ad-hoc app bundles (not workspace build output).
    if let (Some(st), Some(bundle)) = (sig, outer_bundle(&exe)) {
        let built_here = under(&exe, i.roots)
            || i.samples.get(&p.pid).is_some_and(|s| s.workspace.is_some())
            || bundle.components().any(|c| c.as_os_str() == "DerivedData");
        if st.weak() && !built_here {
            d.push_proc(
                p,
                FindingKind::UnsignedExecutable,
                if quarantined { Severity::Medium } else { Severity::Low },
                format!(
                    "The app {} (pid {}) is {}",
                    p.name,
                    p.pid,
                    if st == SigStatus::Unsigned { "not code-signed" } else { "only ad-hoc signed" }
                ),
                "Apps from known developers carry a Developer ID or App Store signature that ties them to their maker; this one does not, so macOS cannot say who built it.",
                vec![quit(p)],
            );
        }
    }
    // A loose (non-bundle) executable still quarantined.
    if quarantined && outer_bundle(&exe).is_none() {
        d.push_proc(
            p,
            FindingKind::QuarantinedExecutable,
            if sig.is_some_and(SigStatus::weak) { Severity::Medium } else { Severity::Low },
            format!("{} (pid {}) was downloaded and is running", p.name, p.pid),
            "macOS marks downloaded files with a quarantine flag; a command-line program with that flag running is something fetched from the internet and started, so make sure you know where it came from.",
            vec![quit(p)],
        );
    }
}

fn detect_exposure(d: &mut Det, p: &ProcFacts) {
    let i = d.i;
    let Some(ls) = d.by_exe_listen.get(&p.pid) else {
        return;
    };
    let open: Vec<String> = {
        let mut v: Vec<String> = ls
            .iter()
            .filter(|l| !l.loopback())
            .map(|l| l.shown())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if open.is_empty() {
        return;
    }
    // Mulchie's own API is reported by the posture check.
    if i.posture
        .api_port
        .is_some_and(|port| ls.iter().any(|l| l.port == port))
        && p.name.to_ascii_lowercase().starts_with("mulchie")
    {
        return;
    }
    let ws = d.in_workspace_or_root(p);
    let app = p.exe.as_deref().and_then(outer_bundle).is_some();
    let sev = if ws {
        Severity::Medium
    } else if app {
        Severity::Info
    } else {
        Severity::Low
    };
    d.push_proc(
        p,
        FindingKind::ExposedListener,
        sev,
        format!(
            "{} (pid {}) is listening on {} beyond this machine",
            p.name,
            p.pid,
            open.join(", ")
        ),
        "A server bound to all interfaces (0.0.0.0 or ::) can be reached by anyone on the same network, such as café or office Wi-Fi; a development server usually only needs 127.0.0.1.",
        vec![quit(p)],
    );
}

fn detect_posture(d: &mut Det) {
    let i = d.i;
    let pos = i.posture;
    if let Some(m) = pos.config_dir_mode {
        if m & 0o077 != 0 {
            d.push_other(
                &pos.config_dir.display().to_string(),
                FindingKind::InsecurePermissions,
                Severity::High,
                format!("Mulchie's settings folder can be read by other users (mode {:o})", m & 0o777),
                "It holds the API tokens; other accounts on this Mac could read them and control Mulchie. Run chmod 700 on it.",
                Evidence {
                    path: Some(pos.config_dir.clone()),
                    mode: Some(format!("{:o}", m & 0o777)),
                    ..Default::default()
                },
            );
        }
    }
    for (path, m) in &pos.token_modes {
        if m & 0o077 != 0 {
            d.push_other(
                &path.display().to_string(),
                FindingKind::InsecurePermissions,
                Severity::High,
                format!("A Mulchie token file can be read by other users (mode {:o})", m & 0o777),
                "Anyone who can read the token can use Mulchie's API as you. Run chmod 600 on it (Mulchie refuses to start its API until then).",
                Evidence {
                    path: Some(path.clone()),
                    mode: Some(format!("{:o}", m & 0o777)),
                    ..Default::default()
                },
            );
        }
    }
    if let (Some(port), Some(ls)) = (pos.api_port, i.listeners) {
        let open: Vec<String> = ls
            .iter()
            .filter(|l| l.port == port && !l.loopback())
            .map(|l| l.shown())
            .collect();
        if !open.is_empty() {
            d.push_other(
                &port.to_string(),
                FindingKind::ApiExposed,
                Severity::High,
                format!("Mulchie's API port {port} is listening beyond this machine"),
                "Mulchie's API should only listen on 127.0.0.1; another program may have taken its port, or it is exposed to the network.",
                Evidence {
                    listen: Some(open),
                    ..Default::default()
                },
            );
        }
    }
    if let Some(pid) = pos.lock_held_by_other {
        d.push_other(
            "engine",
            FindingKind::EngineLockNotHeld,
            Severity::Medium,
            format!("Another Mulchie engine (pid {pid}) is in charge"),
            "Only the engine holding the lock reclaims, sweeps and throttles; this one only reports. That is expected when the app and `mulchie serve` both run.",
            Evidence {
                pid: Some(pid),
                ..Default::default()
            },
        );
    }
    if let Some(e) = &pos.config_error {
        d.push_other(
            "config",
            FindingKind::ConfigNotLoaded,
            Severity::Medium,
            "Mulchie's settings file was not used".into(),
            "Mulchie is running on safe defaults with every automatic action off until the file is fixed.",
            Evidence {
                detail: Some(e.clone()),
                ..Default::default()
            },
        );
    }
}

// ------------------------------------------------------------------- finalize

/// What changed since the last run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub appeared: Vec<Finding>,
    pub gone: Vec<Finding>,
}

/// Apply the allow-lists and dismissals, keep `first_seen_unix` from the
/// previous run, and say what appeared and what went away. Allow-listed
/// findings are dropped; dismissed ones are kept with `dismissed: true`.
pub fn finalize(
    raw: Vec<Finding>,
    prev: &[Finding],
    cfg: &WatchConfig,
    now: u64,
) -> (Vec<Finding>, Changes) {
    let prev_by: HashMap<&str, &Finding> = prev.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut out: Vec<Finding> = Vec::with_capacity(raw.len());
    for mut f in raw {
        f.category = f.kind.category();
        if cfg.allow_ids.iter().any(|a| a == &f.id) {
            continue;
        }
        if f.evidence
            .exe
            .as_ref()
            .is_some_and(|e| cfg.allow_exes.iter().any(|a| a == e))
        {
            continue;
        }
        if let Some(d) = cfg.dismissed.iter().find(|d| d.id == f.id) {
            if d.until_unix.is_none_or(|u| u > now) {
                f.dismissed = true;
                f.dismissed_until = d.until_unix;
            }
        }
        if let Some(p) = prev_by.get(f.id.as_str()) {
            f.first_seen_unix = p.first_seen_unix;
        }
        if !f.suggested.iter().any(|s| s.api.is_some()) {
            f.suggested.push(dismiss(&f.id));
        }
        out.push(f);
    }
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.kind.cmp(&b.kind))
            .then(a.id.cmp(&b.id))
    });
    let now_ids: HashSet<&str> = out.iter().map(|f| f.id.as_str()).collect();
    let prev_ids: HashSet<&str> = prev.iter().map(|f| f.id.as_str()).collect();
    let changes = Changes {
        appeared: out
            .iter()
            .filter(|f| !prev_ids.contains(f.id.as_str()))
            .cloned()
            .collect(),
        gone: prev
            .iter()
            .filter(|f| !now_ids.contains(f.id.as_str()))
            .cloned()
            .collect(),
    };
    (out, changes)
}

/// Dismissals that have run out, removed.
pub fn prune_dismissed(cfg: &mut WatchConfig, now: u64) -> bool {
    let before = cfg.dismissed.len();
    cfg.dismissed
        .retain(|d| d.until_unix.is_none_or(|u| u > now));
    before != cfg.dismissed.len()
}

/// Filter for `GET /v1/findings?severity=&kind=`: `severity` is a minimum.
pub fn filter(
    findings: &[Finding],
    min_severity: Option<Severity>,
    kind: Option<FindingKind>,
    include_dismissed: bool,
) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| min_severity.is_none_or(|s| f.severity >= s))
        .filter(|f| kind.is_none_or(|k| f.kind == k))
        .filter(|f| include_dismissed || !f.dismissed)
        .collect()
}

/// Which executables to check for signatures and quarantine this run: app
/// bundles and executables in temporary, downloads or hidden folders, not
/// already known, at most `max`.
pub fn signature_candidates(
    procs: &[ProcFacts],
    uid: u32,
    known: &dyn Fn(&Path) -> bool,
    temp: &[PathBuf],
    max: usize,
) -> Vec<PathBuf> {
    let mut out: BTreeMap<u8, Vec<PathBuf>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for p in procs.iter().filter(|p| p.uid == Some(uid)) {
        let Some((exe, gone)) = exe_path(p) else {
            continue;
        };
        if gone || !seen.insert(exe.clone()) || known(&exe) {
            continue;
        }
        let s = exe.to_string_lossy();
        if s.starts_with("/System/")
            || s.starts_with("/usr/")
            || s.starts_with("/bin/")
            || s.starts_with("/sbin/")
        {
            continue;
        }
        let rank = if under(&exe, temp) {
            0
        } else if exe.components().any(|c| {
            let c = c.as_os_str().to_string_lossy();
            c.starts_with('.') && c.len() > 1
        }) {
            1
        } else if outer_bundle(&exe).is_some() {
            2
        } else {
            continue;
        };
        out.entry(rank).or_default().push(exe);
    }
    out.into_values().flatten().take(max).collect()
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
