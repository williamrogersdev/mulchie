//! Sessions, limits and the activity record, wired into the engine.
//!
//! The sample loop observes sessions and evaluates limits in memory
//! ([`Engine::observe_sessions`]): no disk, no subprocess, no network. Every
//! action a breach asks for is queued for the limits worker thread
//! (`mulchie-limits`) and runs through the safety gate as [`Request`]s that
//! carry the start time the sample saw, so a pid reused in between is
//! skipped, not signalled. Processes Mulchie paused are written to
//! `<config dir>/sessions/paused.json`; shutdown resumes them, and a start
//! after a crash resumes them (or, without automation, reports them).

use super::*;
use crate::config::{create_private_dir, write_atomic, LimitAction};
use crate::consumers::Severity;
use crate::limits::{ActionState, Breach, Hog, LimitEvent, Limits, Machine};
use crate::record::{Record, RecordLimits, SWEEP_SECS};
use crate::session::{RootSource, Session, SessionEvent, SessionProc, SessionStatus, Tracker};
use crate::watch::{ApiRequest, Evidence, FindingCategory, FindingKind, Suggestion};

/// Sessions listed in [`State`] (running first, then the latest ended).
pub const STATE_SESSIONS_KEPT: usize = 50;
/// The file of processes Mulchie paused, under `<config dir>/sessions/`.
pub const PAUSED_FILE: &str = "paused.json";
/// The limits worker wakes at least this often for scheduled jobs.
const WORKER_POLL: Duration = Duration::from_secs(1);
/// The longest a read of the activity record waits for queued events.
pub const READ_FLUSH: Duration = Duration::from_millis(250);

/// A process Mulchie paused for a limit and has not resumed yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PausedProc {
    pub pid: u32,
    pub start_time: u64,
    pub session_id: String,
    pub breach_id: String,
    pub since_unix: u64,
    /// Recorded before SIGSTOP is sent; false once it was. A crash in
    /// between leaves the entry, so the next engine resumes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pausing: bool,
}

/// Work for the limits worker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LimitJob {
    /// Carry out a new breach's action.
    Act { breach_id: String },
    /// SIGKILL what is left of a `stop` after its grace period.
    Kill {
        breach_id: String,
        targets: Vec<(u32, u64)>,
        at: u64,
    },
    /// Put back the priority a `deprioritize` lowered.
    Restore { breach_id: String },
    /// SIGCONT the processes a `pause` stopped.
    Resume { breach_id: String },
    /// SIGCONT these paused processes now: sessions were turned off, or the
    /// budget that paused them no longer pauses.
    ResumeEntries {
        entries: Vec<PausedProc>,
        why: &'static str,
    },
    /// Write `paused.json`.
    Persist,
    /// Carry out a memory guard closing (SIGTERM).
    GuardClose { id: String },
    /// SIGKILL what is left of a memory guard closing after its grace period.
    GuardKill {
        id: String,
        targets: Vec<(u32, u64)>,
        at: u64,
    },
}

/// Paused processes whose pause no longer applies and that no queued job
/// resumes yet: every one when sessions are off; otherwise those whose breach
/// is gone or whose budget no longer has that cap with action `pause`.
fn paused_without_a_pause(sup: &Supervision, cfg: &Config) -> (Vec<PausedProc>, &'static str) {
    let queued: HashSet<(u32, u64)> = sup
        .jobs
        .iter()
        .filter_map(|j| match j {
            LimitJob::ResumeEntries { entries, .. } => Some(entries),
            _ => None,
        })
        .flatten()
        .map(|p| (p.pid, p.start_time))
        .collect();
    let fresh = |p: &&PausedProc| !queued.contains(&(p.pid, p.start_time));
    if !cfg.sessions.enabled {
        let v = sup.paused.iter().filter(fresh).cloned().collect();
        return (v, "sessions are turned off");
    }
    let still_pauses = |p: &PausedProc| {
        sup.limits.get(&p.breach_id).is_some_and(|b| {
            let ws = sup
                .tracker
                .get(&b.session_id)
                .and_then(|s| s.workspace.clone());
            cfg.limits.budget_for(ws.as_deref()).is_some_and(|bud| {
                bud.caps()
                    .iter()
                    .any(|(n, c)| *n == b.cap.code() && c.action == LimitAction::Pause)
            })
        })
    };
    let v = sup
        .paused
        .iter()
        .filter(fresh)
        .filter(|p| !still_pauses(p))
        .cloned()
        .collect();
    (
        v,
        "the limit that paused them was removed or no longer pauses",
    )
}

impl LimitJob {
    fn due(&self, now: u64) -> bool {
        match self {
            LimitJob::Kill { at, .. } | LimitJob::GuardKill { at, .. } => *at <= now,
            _ => true,
        }
    }
}

/// Sessions, limits and the actions in flight.
#[derive(Default)]
pub(crate) struct Supervision {
    pub(crate) tracker: Tracker,
    pub(crate) limits: Limits,
    /// The memory guard and its current finding (see `engine_memguard.rs`).
    pub(crate) guard: crate::memguard::MemGuard,
    pub(crate) guard_findings: Vec<Finding>,
    pub(crate) paused: Vec<PausedProc>,
    /// Found in `paused.json` at start and not resumed by this engine
    /// (automation is off here); reported in state.
    pub(crate) paused_left: Vec<PausedProc>,
    pub(crate) jobs: VecDeque<LimitJob>,
    /// breach → (pid, start time, nice before Mulchie lowered it).
    lowered: HashMap<String, Vec<(u32, u64, i32)>>,
    record_limits: Option<RecordLimits>,
    last_sweep: u64,
    /// Why `paused.json` could not be read at start; shown as a finding.
    pub(crate) paused_file_error: Option<String>,
}

/// One session in [`State`]: the figures, not the history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionBrief {
    pub id: String,
    pub agent: String,
    pub source: RootSource,
    pub status: SessionStatus,
    pub root_pid: u32,
    pub workspace: Option<PathBuf>,
    pub client: Option<String>,
    pub started_unix: u64,
    pub ended_unix: Option<u64>,
    /// As on [`Session`].
    #[serde(default)]
    pub last_active_unix: u64,
    pub cpu_cores: f64,
    pub memory_mb: f64,
    pub processes: u32,
    pub write_mb_per_hour: Option<f64>,
    pub runtime_secs: u64,
    pub build_slots: u32,
    /// As on [`Session`].
    pub enforced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_enforced_reason: Option<String>,
}

impl SessionBrief {
    fn of(s: &Session) -> Self {
        Self {
            enforced: s.enforced,
            not_enforced_reason: s.not_enforced_reason.clone(),
            id: s.id.clone(),
            agent: s.agent.clone(),
            source: s.source,
            status: s.status,
            root_pid: s.root_pid,
            workspace: s.workspace.clone(),
            client: s.client.clone(),
            started_unix: s.started_unix,
            ended_unix: s.ended_unix,
            last_active_unix: s.last_active_unix,
            cpu_cores: s.usage.cpu_cores,
            memory_mb: s.usage.memory_mb,
            processes: s.usage.processes,
            write_mb_per_hour: s.usage.write_mb_per_hour,
            runtime_secs: s.usage.runtime_secs,
            build_slots: s.usage.build_slots,
        }
    }
}

/// `state.sessions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionsState {
    /// `sessions.enabled`.
    pub enabled: bool,
    /// Bytes written per process are reported on this platform; without it
    /// `bytes_written` and `write_mb_per_hour` are `null` and a
    /// `write_mb_per_hour` cap never fires.
    pub write_bytes_available: bool,
    pub running: usize,
    pub ended: usize,
    /// Running sessions, then the most recently ended, at most 50.
    pub list: Vec<SessionBrief>,
    /// Active breaches, and cleared ones whose paused processes are not
    /// resumed yet.
    pub breaches: Vec<Breach>,
    pub breaches_active: usize,
    /// Breaches not yet handed to their agent through `POST /v1/hook`.
    pub breaches_undelivered: usize,
    pub hogs: Vec<Hog>,
    /// Processes this engine paused and has not resumed.
    pub paused_by_mulchie: Vec<PausedProc>,
    /// Processes an earlier engine paused that this one did not resume
    /// (automation is off here). Resume them with a `resume` process plan.
    pub paused_left: Vec<PausedProc>,
    /// Why the activity record could not be written, while that is so.
    pub record_error: Option<String>,
    /// No budget is configured: limits only report `session_hog` findings.
    pub limits_report_only: bool,
}

/// `GET /v1/sessions/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    pub session: Session,
    /// Live processes of the session as of the last sample.
    pub processes: Vec<SessionMember>,
    /// Breaches of this session that Mulchie still keeps.
    pub breaches: Vec<Breach>,
    pub paused_by_mulchie: Vec<PausedProc>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionMember {
    pub pid: u32,
    pub ppid: u32,
    pub start_time: u64,
    pub name: String,
    /// The command line, redacted.
    pub cmd: String,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub stopped: bool,
    /// The session's root (the agent itself).
    pub root: bool,
}

/// `GET /v1/limits/status`.
#[derive(Debug, Clone, Serialize)]
pub struct LimitsStatus {
    pub report_only: bool,
    pub config: crate::config::LimitsConfig,
    /// Every breach Mulchie keeps, newest first (active and recently cleared).
    pub breaches: Vec<Breach>,
    pub hogs: Vec<Hog>,
    pub paused_by_mulchie: Vec<PausedProc>,
    pub paused_left: Vec<PausedProc>,
    /// Limit actions run only on the engine that automates.
    pub enforcing: bool,
    /// The memory guard: whether it acts, whether memory is low, and what it
    /// closed.
    pub memory_guard: super::memguard_glue::MemoryGuardStatus,
    /// A7: why not, when `enforcing` is false: `not_lock_holder` (another
    /// engine holds the lock) or `config_error` (the config file did not
    /// load).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_enforcing_reason: Option<String>,
}

fn paused_path(paths: &Paths) -> PathBuf {
    paths.config_dir.join("sessions").join(PAUSED_FILE)
}

/// `paused.json`, or why it could not be read. A missing file is empty.
pub(crate) fn load_paused(paths: &Paths) -> Result<Vec<PausedProc>, String> {
    match std::fs::read(paused_path(paths)) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{PAUSED_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{PAUSED_FILE}: {e}")),
    }
}

fn save_paused(paths: &Paths, paused: &[PausedProc]) -> std::io::Result<()> {
    let p = paused_path(paths);
    if paused.is_empty() && !p.exists() {
        return Ok(());
    }
    if let Some(dir) = p.parent() {
        create_private_dir(dir)?;
    }
    let data = serde_json::to_vec_pretty(paused).unwrap_or_else(|_| b"[]".to_vec());
    write_atomic(&p, &data, 0o600)
}

/// `pid 12: start_time_mismatch, pid 13: process_gone` (at most four).
pub(super) fn skipped_words(results: &[ActionResult]) -> String {
    let skipped: Vec<String> = results
        .iter()
        .filter_map(|r| match &r.outcome {
            Outcome::Skipped { reason, .. } => Some(format!("{}: {}", r.target, reason.code())),
            _ => None,
        })
        .collect();
    let mut s = skipped
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if skipped.len() > 4 {
        s.push_str(&format!(" and {} more", skipped.len() - 4));
    }
    s
}

pub(super) fn state_of(done: usize, total: usize) -> ActionState {
    if total > 0 && done == total {
        ActionState::Done
    } else if done > 0 {
        ActionState::Partial
    } else {
        ActionState::Skipped
    }
}

impl Engine {
    pub(crate) fn record_writer(&self) -> &Record {
        self.record.get_or_init(|| {
            let cfg = self.config();
            Record::new(
                self.paths.config_dir.join("sessions"),
                RecordLimits::from_config(&cfg.sessions),
            )
        })
    }

    /// Records are written by the engine that holds the lock only.
    pub(crate) fn records_on(&self, cfg: &Config) -> bool {
        cfg.sessions.enabled && self.automation_enabled.load(Ordering::SeqCst)
    }

    /// Wait until everything queued for the activity record is written.
    pub fn flush_record(&self) {
        if let Some(r) = self.record.get() {
            r.flush();
        }
    }

    // ------------------------------------------------------- sample loop

    /// Update sessions and limits from one sample. In memory only; actions
    /// are queued for the limits worker ([`Engine::run_limit_jobs`]).
    pub(crate) fn observe_sessions(
        &self,
        now: u64,
        procs: &[SessionProc],
        machine: Machine,
        cfg: &Config,
    ) {
        let on = cfg.sessions.enabled;
        // Every own process of the sample, also with sessions off: paused
        // processes are pruned against it.
        let sampled = procs;
        let procs: &[SessionProc] = if on { procs } else { &[] };
        let slots: Vec<u32> = {
            let st = lock(&self.state);
            st.slots.active.iter().filter_map(|l| l.pid).collect()
        };
        let automation = self.automation_allowed();
        let records = self.records_on(cfg);
        let retain = cfg.sessions.retain_days.saturating_mul(86_400);
        let mut notes: Vec<(String, Option<String>, bool)> = Vec::new();
        let mut wake = false;
        let (findings, rec_events, sweep, limits_changed) = {
            let mut sup = lock(&self.sup);
            let sup = &mut *sup;
            let events = sup.tracker.observe(now, procs, &slots, retain);
            // Paused processes that exited need no resume. Only a real
            // process list prunes: an empty one says nothing.
            if !sampled.is_empty() {
                let alive: HashSet<(u32, u64)> =
                    sampled.iter().map(|p| (p.pid, p.start_time)).collect();
                let before = sup.paused.len();
                sup.paused
                    .retain(|p| alive.contains(&(p.pid, p.start_time)));
                if sup.paused.len() != before {
                    sup.jobs.push_back(LimitJob::Persist);
                    wake = true;
                }
            }
            let lev = sup
                .limits
                .evaluate(now, sup.tracker.sessions(), &cfg.limits, machine);
            // Paused processes whose pause no longer applies are resumed now
            // (they stay listed until the worker has resumed them).
            let (unpause, why) = paused_without_a_pause(sup, cfg);
            if !unpause.is_empty() {
                sup.jobs.push_back(LimitJob::ResumeEntries {
                    entries: unpause,
                    why,
                });
                wake = true;
            }
            let mut rec: Vec<(String, &'static str, Value)> = Vec::new();
            for e in lev {
                match e {
                    LimitEvent::Breached { breach_id } => {
                        let hook = {
                            let b = sup.limits.get(&breach_id);
                            b.and_then(|b| sup.tracker.get(&b.session_id))
                                .is_some_and(|s| s.source == RootSource::Hook)
                        };
                        let Some(b) = sup.limits.get_mut(&breach_id) else {
                            continue;
                        };
                        b.action_state = if b.action == LimitAction::Notify {
                            ActionState::NotifyOnly
                        } else if hook {
                            b.action_detail = Some(
                                "the session was registered by a hook call; Mulchie only reports it"
                                    .into(),
                            );
                            ActionState::NotEnforced
                        } else if !automation {
                            b.action_detail = Some(
                                "automation is off on this engine (another engine holds the lock, or the config did not load)"
                                    .into(),
                            );
                            ActionState::NotEnforced
                        } else {
                            sup.jobs.push_back(LimitJob::Act {
                                breach_id: breach_id.clone(),
                            });
                            wake = true;
                            ActionState::Pending
                        };
                        let ws = sup
                            .tracker
                            .get(&b.session_id)
                            .and_then(|s| s.workspace.as_ref())
                            .map(|w| w.display().to_string());
                        notes.push((b.sentence(), ws, true));
                        rec.push((
                            b.session_id.clone(),
                            "limit_breach",
                            json!({
                                "breach_id": b.id, "cap": b.cap, "max": b.max, "value": b.value,
                                "action": b.action, "action_state": b.action_state,
                            }),
                        ));
                    }
                    LimitEvent::Cleared { breach_id } => {
                        let Some(b) = sup.limits.get(&breach_id) else {
                            continue;
                        };
                        if b.action == LimitAction::Deprioritize
                            && matches!(b.action_state, ActionState::Done | ActionState::Partial)
                        {
                            sup.jobs.push_back(LimitJob::Restore {
                                breach_id: breach_id.clone(),
                            });
                            wake = true;
                        }
                        notes.push((
                            format!(
                                "Session {} ({}) is back under its {} limit",
                                b.session_id,
                                b.agent,
                                b.cap.words()
                            ),
                            None,
                            false,
                        ));
                        rec.push((
                            b.session_id.clone(),
                            "limit_cleared",
                            json!({"breach_id": b.id, "cap": b.cap}),
                        ));
                    }
                    LimitEvent::ResumeDue { breach_id } => {
                        sup.jobs.push_back(LimitJob::Resume { breach_id });
                        wake = true;
                    }
                }
            }
            for e in &events {
                match e {
                    SessionEvent::Started { id } => {
                        if let Some(s) = sup.tracker.get(id) {
                            rec.push((
                                id.clone(),
                                "session_start",
                                json!({
                                    "agent": s.agent, "source": s.source,
                                    "root_pid": s.root_pid, "root_start_time": s.root_start_time,
                                    "command": s.command, "workspace": s.workspace,
                                    "cwd": s.cwd, "parent": s.parent,
                                }),
                            ));
                        }
                    }
                    SessionEvent::Ended { id } => {
                        if let Some(s) = sup.tracker.get(id) {
                            rec.push((
                                id.clone(),
                                "session_end",
                                json!({
                                    "runtime_secs": s.usage.runtime_secs,
                                    "peak": s.peak,
                                    "bytes_written": s.usage.bytes_written,
                                }),
                            ));
                        }
                    }
                    SessionEvent::ProcessStarted { .. } => {}
                }
            }
            let findings = Self::limit_category_findings(sup, &self.paths, now);
            let rl = RecordLimits::from_config(&cfg.sessions);
            let limits_changed = sup.record_limits.replace(rl) != Some(rl);
            let sweep = records && now >= sup.last_sweep.saturating_add(SWEEP_SECS);
            let sweep = sweep.then(|| {
                sup.last_sweep = now;
                sup.tracker
                    .sessions()
                    .filter(|s| s.status == SessionStatus::Running)
                    .map(|s| s.id.clone())
                    .collect::<Vec<String>>()
            });
            (findings, (rec, events), sweep, limits_changed)
        };
        if wake {
            self.limit_wake.notify_all();
        }
        if records {
            let r = self.record_writer();
            if limits_changed {
                r.set_limits(RecordLimits::from_config(&cfg.sessions));
            }
            let (rec, events) = rec_events;
            for (sid, ty, fields) in rec {
                r.event(&sid, now, ty, fields);
            }
            for e in events {
                match e {
                    SessionEvent::ProcessStarted { id, pid, name, cmd } => {
                        r.process_start(&id, now, pid, &name, &cmd)
                    }
                    SessionEvent::Ended { id } => r.close(&id, now),
                    SessionEvent::Started { .. } => {}
                }
            }
            if let Some(live) = sweep {
                r.sweep(now, live);
            }
        }
        for (text, ws, alert) in notes {
            if alert && self.notify.load(Ordering::SeqCst) {
                crate::notify::notify("Mulchie", &text);
            }
            self.log("limit", text, 0, ws, "limits");
        }
        self.replace_findings(FindingCategory::Limit, findings, now);
    }

    /// Every finding of the `limit` category: breaches, hogs, an unreadable
    /// paused list and the memory guard's.
    fn limit_category_findings(sup: &Supervision, paths: &Paths, now: u64) -> Vec<Finding> {
        let mut findings = limit_findings(&sup.limits, &sup.tracker, now);
        if let Some(e) = &sup.paused_file_error {
            findings.push(paused_file_finding(paths, e, now));
        }
        findings.extend(sup.guard_findings.iter().cloned());
        findings
    }

    /// Store the `limit` findings again (after the memory guard changed its own).
    pub(crate) fn publish_limit_findings(&self, now: u64) {
        let findings = Self::limit_category_findings(&lock(&self.sup), &self.paths, now);
        self.replace_findings(FindingCategory::Limit, findings, now);
    }

    /// Replace the stored findings of one category (limits, updates) with
    /// `raw`, finalized like the watch's, and journal what changed.
    pub(crate) fn replace_findings(&self, cat: FindingCategory, raw: Vec<Finding>, now: u64) {
        let watch_cfg = lock(&self.state).config.watch.clone();
        let changes = {
            let mut st = lock(&self.state);
            let prev: Vec<Finding> = st
                .findings
                .iter()
                .filter(|f| f.category == cat)
                .cloned()
                .collect();
            if raw.is_empty() && prev.is_empty() {
                return;
            }
            let (fresh, changes) = watch::finalize(raw, &prev, &watch_cfg, now);
            st.findings.retain(|f| f.category != cat);
            st.findings.extend(fresh);
            sort_findings(&mut st.findings);
            st.finding_counts = FindingCounts::of(&st.findings);
            changes
        };
        if !changes.appeared.is_empty() || !changes.gone.is_empty() {
            self.journal_findings(&changes, now);
        }
    }

    // ------------------------------------------------------ limits worker

    /// Start the limits worker and resume what a crashed engine left paused.
    pub(crate) fn start_limits_worker(&self, threads: &mut Vec<JoinHandle<()>>) {
        self.recover_paused();
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-limits".into())
            .spawn(move || {
                while !me.is_shut_down() {
                    let r = catch_unwind(AssertUnwindSafe(|| me.run_limit_jobs(now_unix())));
                    if r.is_err() {
                        me.log(
                            "error",
                            "limits worker failed on a job; continuing".into(),
                            0,
                            None,
                            "limits",
                        );
                    }
                    let sup = lock(&me.sup);
                    let now = now_unix();
                    if !sup.jobs.iter().any(|j| j.due(now)) && !me.is_shut_down() {
                        let _ = me.limit_wake.wait_timeout(sup, WORKER_POLL);
                    }
                }
            })
        {
            threads.push(h);
        }
    }

    /// Run every limit job due at `now`. Tests call this directly.
    pub fn run_limit_jobs(&self, now: u64) {
        loop {
            let job = {
                let mut sup = lock(&self.sup);
                let Some(i) = sup.jobs.iter().position(|j| j.due(now)) else {
                    return;
                };
                sup.jobs.remove(i)
            };
            let Some(job) = job else { return };
            match job {
                LimitJob::Act { breach_id } => {
                    self.act(&breach_id, now);
                    self.record_act(&breach_id, now);
                }
                LimitJob::Kill {
                    breach_id, targets, ..
                } => self.kill_survivors(&breach_id, &targets, now),
                LimitJob::Restore { breach_id } => self.restore_breach(&breach_id, now),
                LimitJob::Resume { breach_id } => self.resume_breach(&breach_id, "limits"),
                LimitJob::ResumeEntries { entries, why } => {
                    self.log(
                        "limit",
                        format!("Resuming {} paused process(es): {why}", entries.len()),
                        0,
                        None,
                        "limits",
                    );
                    self.resume_entries(&entries, "limits");
                }
                LimitJob::Persist => self.persist_paused(),
                LimitJob::GuardClose { id } => self.guard_close(&id, now),
                LimitJob::GuardKill { id, targets, .. } => self.guard_kill(&id, &targets, now),
            }
        }
    }

    fn persist_paused(&self) {
        let paused = lock(&self.sup).paused.clone();
        if let Err(e) = save_paused(&self.paths, &paused) {
            self.log(
                "error",
                format!("Could not save the list of paused processes: {e}"),
                0,
                None,
                "limits",
            );
        }
    }

    /// A1: a limit action in the session's activity record, as a
    /// `limit_action` event: `breach_id`, `action` (the cap's action),
    /// `outcome` (paused, stopped, killed, deprioritized, resumed, restored,
    /// restore_failed or refused), `state` and `detail`, and `reason` when
    /// refused. `cap`, `action` and `state` are null when the breach is gone
    /// (a resume after sessions or limits were turned off). Written only
    /// where records are (the lock holder).
    fn record_limit_action(
        &self,
        session_id: &str,
        breach_id: &str,
        b: Option<&Breach>,
        now: u64,
        outcome: &str,
        detail: &str,
    ) {
        let cfg = self.config();
        if !self.records_on(&cfg) {
            return;
        }
        let mut ev = json!({
            "breach_id": breach_id,
            "cap": b.map(|b| b.cap),
            "action": b.map(|b| b.action),
            "outcome": outcome,
            "state": b.map(|b| b.action_state),
            // Mulchie's own words (counts, pids, gate reasons): no user
            // text, so nothing to redact.
            "detail": detail,
        });
        if outcome == "refused" {
            ev["reason"] = ev["detail"].clone();
        }
        self.record_writer()
            .event(session_id, now, "limit_action", ev);
    }

    /// After `act`: what it did (or why it did nothing) into the record.
    fn record_act(&self, breach_id: &str, now: u64) {
        let Some(b) = lock(&self.sup).limits.get(breach_id).cloned() else {
            return;
        };
        let outcome = match (b.action_state, b.action) {
            (ActionState::Pending | ActionState::NotifyOnly, _) | (_, LimitAction::Notify) => {
                return
            }
            (ActionState::Done | ActionState::Partial, LimitAction::Pause) => "paused",
            (ActionState::Done | ActionState::Partial, LimitAction::Stop) => "stopped",
            (ActionState::Done | ActionState::Partial, LimitAction::Deprioritize) => {
                "deprioritized"
            }
            (ActionState::Skipped | ActionState::NotEnforced, _) => "refused",
        };
        let detail = b.action_detail.clone().unwrap_or_default();
        self.record_limit_action(&b.session_id, &b.id, Some(&b), now, outcome, &detail);
    }

    fn set_action(&self, breach_id: &str, state: ActionState, detail: Option<String>) {
        if let Some(b) = lock(&self.sup).limits.get_mut(breach_id) {
            b.action_state = state;
            if detail.is_some() {
                b.action_detail = detail;
            }
        }
    }

    /// Carry out a new breach's action through the gate.
    fn act(&self, breach_id: &str, now: u64) {
        let (b, session, members) = {
            let sup = lock(&self.sup);
            let Some(b) = sup.limits.get(breach_id).cloned() else {
                return;
            };
            let s = sup.tracker.get(&b.session_id).cloned();
            let m = sup.tracker.members(&b.session_id);
            (b, s, m)
        };
        if b.action_state != ActionState::Pending {
            return;
        }
        let Some(session) = session else {
            self.set_action(
                breach_id,
                ActionState::Skipped,
                Some("the session is gone".into()),
            );
            return;
        };
        if !b.active() {
            self.set_action(
                breach_id,
                ActionState::Skipped,
                Some("the session was back under the cap before the action ran".into()),
            );
            return;
        }
        if !self.automation_allowed() {
            self.set_action(
                breach_id,
                ActionState::NotEnforced,
                Some("automation is off on this engine".into()),
            );
            return;
        }
        let cfg = self.config();
        let budget = cfg
            .limits
            .budget_for(session.workspace.as_deref())
            .cloned()
            .unwrap_or_default();
        let root = (session.root_pid, session.root_start_time);
        let targets: Vec<(u32, u64)> = members
            .into_iter()
            .filter(|m| budget.include_agent || *m != root)
            .collect();
        if targets.is_empty() {
            self.set_action(
                breach_id,
                ActionState::Skipped,
                Some(if budget.include_agent {
                    "the session has no processes left".into()
                } else {
                    "the session has no processes besides the agent, which is spared (include_agent is off)".into()
                }),
            );
            return;
        }
        let source = "limits";
        let sig = |s: Signal| -> Vec<Request> {
            targets
                .iter()
                .map(|(pid, st)| Request::Signal {
                    pid: *pid,
                    signal: s,
                    expected_start_time: *st,
                })
                .collect()
        };
        let total = targets.len();
        let (state, detail, verb) = match b.action {
            LimitAction::Notify => return,
            LimitAction::Pause => {
                // Shutdown takes this lock before it resumes everything, so
                // a pause either finishes first (and is resumed) or never
                // starts.
                let _pausing = lock(&self.pause_lock);
                if self.is_shut_down() {
                    self.set_action(
                        breach_id,
                        ActionState::Skipped,
                        Some("the engine is shutting down".into()),
                    );
                    return;
                }
                // Recorded (and saved) before SIGSTOP: a crash in between
                // still leaves them to resume.
                {
                    let mut sup = lock(&self.sup);
                    for (pid, st) in &targets {
                        sup.paused.push(PausedProc {
                            pid: *pid,
                            start_time: *st,
                            session_id: b.session_id.clone(),
                            breach_id: b.id.clone(),
                            since_unix: now,
                            pausing: true,
                        });
                    }
                }
                self.persist_paused();
                let results = self.execute(&sig(Signal::Stop), source);
                let mut done = 0;
                {
                    let mut sup = lock(&self.sup);
                    for (r, (pid, st)) in results.iter().zip(&targets) {
                        let Some(i) = sup.paused.iter().position(|p| {
                            p.pausing && p.pid == *pid && p.start_time == *st && p.breach_id == b.id
                        }) else {
                            continue;
                        };
                        if r.is_done() {
                            done += 1;
                            sup.paused[i].pausing = false;
                        } else {
                            sup.paused.remove(i);
                        }
                    }
                }
                self.persist_paused();
                (
                    state_of(done, total),
                    self.detail(done, total, &results),
                    "paused",
                )
            }
            LimitAction::Stop => {
                let results = self.execute(&sig(Signal::Term), source);
                let sent: Vec<(u32, u64)> = results
                    .iter()
                    .zip(&targets)
                    .filter(|(r, _)| r.is_done())
                    .map(|(_, t)| *t)
                    .collect();
                let done = sent.len();
                if !sent.is_empty() {
                    lock(&self.sup).jobs.push_back(LimitJob::Kill {
                        breach_id: b.id.clone(),
                        targets: sent,
                        at: now.saturating_add(budget.stop_grace_secs),
                    });
                    self.limit_wake.notify_all();
                }
                (
                    state_of(done, total),
                    self.detail(done, total, &results),
                    "sent SIGTERM to",
                )
            }
            LimitAction::Deprioritize => {
                let nice = cfg.build_nice.max(10);
                let before: Vec<Option<i32>> = targets.iter().map(|(p, _)| nice_of(*p)).collect();
                let reqs: Vec<Request> = targets
                    .iter()
                    .map(|(pid, st)| Request::Renice {
                        pid: *pid,
                        nice,
                        expected_start_time: *st,
                    })
                    .collect();
                let results = self.execute(&reqs, source);
                let mut done = 0;
                let mut lowered = Vec::new();
                for ((r, (pid, st)), orig) in results.iter().zip(&targets).zip(before) {
                    if r.is_done() {
                        done += 1;
                        if let Some(o) = orig.filter(|o| *o < nice) {
                            lowered.push((*pid, *st, o));
                        }
                    }
                }
                lock(&self.sup).lowered.insert(b.id.clone(), lowered);
                (
                    state_of(done, total),
                    self.detail(done, total, &results),
                    "lowered the priority of",
                )
            }
        };
        self.set_action(breach_id, state, Some(detail.clone()));
        self.log(
            "limit",
            format!("Session {} ({}): {verb} {detail}", b.session_id, b.agent),
            0,
            session.workspace.as_ref().map(|w| w.display().to_string()),
            source,
        );
    }

    /// `3 of 4 processes (skipped pid 9: start_time_mismatch)`
    fn detail(&self, done: usize, total: usize, results: &[ActionResult]) -> String {
        let mut s = format!("{done} of {total} process(es)");
        let sk = skipped_words(results);
        if !sk.is_empty() {
            s.push_str(&format!(" (skipped {sk})"));
        }
        s
    }

    /// SIGKILL the processes of a `stop` still running after the grace
    /// period (same pid and start time; the gate checks again).
    fn kill_survivors(&self, breach_id: &str, targets: &[(u32, u64)], now: u64) {
        let alive: Vec<(u32, u64)> = targets
            .iter()
            .copied()
            .filter(|(p, st)| self.procs.get(*p).is_some_and(|f| f.start_time == *st))
            .collect();
        let mut note = format!(
            "{} of {} exited after SIGTERM",
            targets.len() - alive.len(),
            targets.len()
        );
        if !alive.is_empty() {
            let reqs: Vec<Request> = alive
                .iter()
                .map(|(pid, st)| Request::Signal {
                    pid: *pid,
                    signal: Signal::Kill,
                    expected_start_time: *st,
                })
                .collect();
            let results = self.execute(&reqs, "limits");
            let killed = results.iter().filter(|r| r.is_done()).count();
            note.push_str(&format!("; SIGKILL to {killed} of {}", alive.len()));
            let sk = skipped_words(&results);
            if !sk.is_empty() {
                note.push_str(&format!(" (skipped {sk})"));
            }
        }
        let b = {
            let mut sup = lock(&self.sup);
            sup.limits.get_mut(breach_id).map(|b| {
                let d = b.action_detail.take().unwrap_or_default();
                b.action_detail = Some(if d.is_empty() {
                    note.clone()
                } else {
                    format!("{d}; {note}")
                });
                b.clone()
            })
        };
        if let Some(b) = b {
            self.record_limit_action(&b.session_id, &b.id, Some(&b), now, "killed", &note);
        }
    }

    /// Put back the priorities a `deprioritize` lowered. Raising priority
    /// usually needs privilege, so this often fails; the breach says so.
    fn restore_breach(&self, breach_id: &str, now: u64) {
        let lowered = lock(&self.sup)
            .lowered
            .remove(breach_id)
            .unwrap_or_default();
        let (mut ok, mut failed) = (0usize, 0usize);
        for (pid, st, orig) in &lowered {
            if !self.same_own_process(*pid, *st) {
                continue;
            }
            if set_nice(*pid, *orig).is_ok() {
                ok += 1;
            } else {
                failed += 1;
            }
        }
        if ok + failed > 0 {
            let mut text =
                format!("Restored the priority of {ok} process(es) after limit breach {breach_id}");
            if failed > 0 {
                text.push_str(&format!(
                    "; {failed} keep the lower priority (raising priority needs privilege)"
                ));
            }
            self.log("limit", text, 0, None, "limits");
        }
        let note = if failed == 0 {
            format!("priority restored for {ok} process(es)")
        } else {
            format!(
                "priority restored for {ok}, not for {failed} process(es): raising priority needs privilege, so they keep the lower priority until they exit"
            )
        };
        let b = {
            let mut sup = lock(&self.sup);
            sup.limits.get_mut(breach_id).map(|b| {
                b.restored = failed == 0;
                let d = b.action_detail.take().unwrap_or_default();
                b.action_detail = Some(if d.is_empty() {
                    note.clone()
                } else {
                    format!("{d}; {note}")
                });
                b.clone()
            })
        };
        if let Some(b) = b {
            let outcome = if failed == 0 {
                "restored"
            } else {
                "restore_failed"
            };
            self.record_limit_action(&b.session_id, &b.id, Some(&b), now, outcome, &note);
        }
    }

    /// SIGCONT every process paused for `breach_id`.
    fn resume_breach(&self, breach_id: &str, source: &str) {
        let entries: Vec<PausedProc> = lock(&self.sup)
            .paused
            .iter()
            .filter(|p| p.breach_id == breach_id)
            .cloned()
            .collect();
        let n = self.resume_entries(&entries, source);
        let mut sup = lock(&self.sup);
        if let Some(b) = sup.limits.get_mut(breach_id) {
            b.restored = true;
            let note = format!("resumed {n} of {} process(es)", entries.len());
            let d = b.action_detail.take().unwrap_or_default();
            b.action_detail = Some(if d.is_empty() {
                note
            } else {
                format!("{d}; {note}")
            });
        }
    }

    /// SIGCONT these paused processes through the gate and forget them
    /// (whatever the outcome: a process that exited or whose pid was reused
    /// needs nothing more). Returns how many were resumed.
    fn resume_entries(&self, entries: &[PausedProc], source: &str) -> usize {
        if entries.is_empty() {
            return 0;
        }
        let reqs: Vec<Request> = entries
            .iter()
            .map(|p| Request::Signal {
                pid: p.pid,
                signal: Signal::Cont,
                expected_start_time: p.start_time,
            })
            .collect();
        let results = self.execute(&reqs, source);
        let resumed = results.iter().filter(|r| r.is_done()).count();
        {
            let keys: HashSet<(u32, u64)> = entries.iter().map(|p| (p.pid, p.start_time)).collect();
            let mut sup = lock(&self.sup);
            sup.paused
                .retain(|p| !keys.contains(&(p.pid, p.start_time)));
            sup.paused_left
                .retain(|p| !keys.contains(&(p.pid, p.start_time)));
        }
        self.persist_paused();
        let failed = skipped_words(&results);
        let mut text = format!("Resumed {resumed} of {} paused process(es)", entries.len());
        if !failed.is_empty() {
            text.push_str(&format!(" (skipped {failed})"));
        }
        self.log("limit", text, 0, None, source);
        // Into each breach's session record: how many of its processes.
        let mut per: BTreeMap<(&str, &str), (usize, usize)> = BTreeMap::new();
        for (p, r) in entries.iter().zip(&results) {
            let c = per
                .entry((p.session_id.as_str(), p.breach_id.as_str()))
                .or_default();
            c.1 += 1;
            if r.is_done() {
                c.0 += 1;
            }
        }
        let now = crate::monitor::now_unix();
        for ((session_id, breach_id), (done, total)) in per {
            let b = lock(&self.sup).limits.get(breach_id).cloned();
            let d = format!("resumed {done} of {total} process(es) ({source})");
            self.record_limit_action(session_id, breach_id, b.as_ref(), now, "resumed", &d);
        }
        resumed
    }

    /// After any executed batch: paused processes that were continued,
    /// stopped or killed by request need no resume from Mulchie.
    pub(crate) fn note_signals(&self, requests: &[Request], results: &[ActionResult]) {
        let ended: Vec<(u32, u64)> = requests
            .iter()
            .zip(results)
            .filter(|(_, r)| r.is_done())
            .filter_map(|(q, _)| match q {
                Request::Signal {
                    pid,
                    signal: Signal::Cont | Signal::Kill,
                    expected_start_time,
                } => Some((*pid, *expected_start_time)),
                _ => None,
            })
            .collect();
        if ended.is_empty() {
            return;
        }
        // Nothing holds `sup` across `execute`, so this cannot deadlock.
        let changed = {
            let mut sup = lock(&self.sup);
            let before = sup.paused.len() + sup.paused_left.len();
            sup.paused
                .retain(|p| !ended.contains(&(p.pid, p.start_time)));
            sup.paused_left
                .retain(|p| !ended.contains(&(p.pid, p.start_time)));
            let changed = sup.paused.len() + sup.paused_left.len() != before;
            if changed {
                sup.jobs.push_back(LimitJob::Persist);
            }
            changed
        };
        if changed {
            self.limit_wake.notify_all();
        }
    }

    /// On start: processes an earlier engine paused and never resumed (it
    /// crashed). Resumed now when this engine automates; otherwise listed in
    /// `state.sessions.paused_left`.
    pub(crate) fn recover_paused(&self) {
        let found = match load_paused(&self.paths) {
            Ok(v) => v,
            Err(e) => {
                self.log(
                    "error",
                    format!("Could not read the list of paused processes ({e}); check for stopped processes"),
                    0,
                    None,
                    "limits",
                );
                lock(&self.sup).paused_file_error = Some(e);
                return;
            }
        };
        if found.is_empty() {
            return;
        }
        if self.automation_allowed() {
            lock(&self.sup).paused_left = found.clone();
            self.resume_entries(&found, "startup");
        } else {
            lock(&self.sup).paused_left = found;
        }
    }

    /// On shutdown: resume everything this engine paused.
    pub(crate) fn resume_all_paused(&self, source: &str) {
        let entries = lock(&self.sup).paused.clone();
        self.resume_entries(&entries, source);
    }

    // ------------------------------------------------------------ reads

    /// `state.sessions`.
    pub(crate) fn sessions_state(&self, cfg: &Config) -> SessionsState {
        let write = lock(&self.state).snapshot.write_bytes_available;
        let record_error = self.record.get().and_then(|r| r.error());
        let automating = self.automation_allowed();
        let sup = lock(&self.sup);
        let mut list: Vec<&Session> = sup.tracker.sessions().collect();
        let running = list
            .iter()
            .filter(|s| s.status == SessionStatus::Running)
            .count();
        list.sort_by(|a, b| {
            (a.status == SessionStatus::Ended)
                .cmp(&(b.status == SessionStatus::Ended))
                .then(b.ended_unix.cmp(&a.ended_unix))
                .then(b.started_unix.cmp(&a.started_unix))
        });
        let ended = list.len() - running;
        let breaches: Vec<Breach> = sup
            .limits
            .breaches()
            .filter(|b| {
                b.active()
                    || (b.action == LimitAction::Pause
                        && !b.restored
                        && matches!(b.action_state, ActionState::Done | ActionState::Partial))
            })
            .cloned()
            .collect();
        SessionsState {
            enabled: cfg.sessions.enabled,
            write_bytes_available: write,
            running,
            ended,
            list: list
                .into_iter()
                .take(STATE_SESSIONS_KEPT)
                .map(|s| SessionBrief::of(&Self::with_enforcement(s.clone(), cfg, automating)))
                .collect(),
            breaches_active: breaches.iter().filter(|b| b.active()).count(),
            breaches_undelivered: sup.limits.breaches().filter(|b| !b.delivered).count(),
            breaches,
            hogs: sup.limits.hogs().to_vec(),
            paused_by_mulchie: sup.paused.clone(),
            paused_left: sup.paused_left.clone(),
            record_error,
            limits_report_only: cfg.limits.report_only(),
        }
    }

    /// (running sessions, active breaches) for `/v1/health`.
    pub(crate) fn session_counts(&self) -> (usize, usize) {
        let sup = lock(&self.sup);
        (
            sup.tracker
                .sessions()
                .filter(|s| s.status == SessionStatus::Running)
                .count(),
            sup.limits.breaches().filter(|b| b.active()).count(),
        )
    }

    /// A4: whether a breach of `s` would be acted on here, and why not.
    /// `automating` is [`Engine::automation_allowed`], read before `sup` is
    /// locked (it takes `state`, which comes first).
    pub(crate) fn with_enforcement(mut s: Session, cfg: &Config, automating: bool) -> Session {
        let reason = if s.status == SessionStatus::Ended {
            Some("ended")
        } else if s.source == RootSource::Hook {
            Some("hook_registered")
        } else if !automating {
            Some("not_automating")
        } else {
            match cfg.limits.budget_for(s.workspace.as_deref()) {
                None => Some("no_budget"),
                Some(b) => {
                    let caps = [
                        &b.cpu_cores,
                        &b.memory_mb,
                        &b.processes,
                        &b.write_mb_per_hour,
                        &b.runtime_hours,
                    ];
                    let acts = caps
                        .iter()
                        .filter_map(|c| c.as_ref())
                        .any(|c| c.action != LimitAction::Notify);
                    (!acts).then_some("notify_only")
                }
            }
        };
        s.enforced = reason.is_none();
        s.not_enforced_reason = reason.map(String::from);
        s
    }

    /// Every session Mulchie keeps, running first.
    pub fn sessions(&self) -> Vec<Session> {
        let cfg = self.config();
        let automating = self.automation_allowed();
        let sup = lock(&self.sup);
        let mut v: Vec<Session> = sup
            .tracker
            .sessions()
            .map(|s| Self::with_enforcement(s.clone(), &cfg, automating))
            .collect();
        v.sort_by(|a, b| {
            (a.status == SessionStatus::Ended)
                .cmp(&(b.status == SessionStatus::Ended))
                .then(b.started_unix.cmp(&a.started_unix))
        });
        v
    }

    pub fn session(&self, id: &str) -> Option<SessionDetail> {
        let procs: Vec<SessionProc> = lock(&self.state).snapshot.session_procs.clone();
        let cfg = self.config();
        let automating = self.automation_allowed();
        let sup = lock(&self.sup);
        let s = Self::with_enforcement(sup.tracker.get(id)?.clone(), &cfg, automating);
        let members: HashSet<(u32, u64)> = sup.tracker.members(id).into_iter().collect();
        let processes = procs
            .iter()
            .filter(|p| members.contains(&(p.pid, p.start_time)))
            .map(|p| SessionMember {
                pid: p.pid,
                ppid: p.ppid,
                start_time: p.start_time,
                name: p.name.clone(),
                cmd: p.cmd.clone(),
                cpu_pct: p.cpu_pct,
                mem_bytes: p.mem_bytes,
                stopped: p.stopped,
                root: p.pid == s.root_pid && p.start_time == s.root_start_time,
            })
            .collect();
        let breaches = sup
            .limits
            .breaches()
            .filter(|b| b.session_id == id)
            .cloned()
            .collect();
        let paused = sup
            .paused
            .iter()
            .filter(|p| p.session_id == id)
            .cloned()
            .collect();
        Some(SessionDetail {
            session: s,
            processes,
            breaches,
            paused_by_mulchie: paused,
        })
    }

    /// Events of a session's activity record with `seq` above `since`.
    pub fn session_events(
        &self,
        id: &str,
        since: u64,
        limit: usize,
    ) -> std::io::Result<Vec<Value>> {
        Ok(self
            .session_events_page(id, crate::record::Query::Since { since, limit })?
            .events)
    }

    /// A page of a session's activity record. Events still queued are waited
    /// for at most [`READ_FLUSH`]; the read seeks through the record's index.
    pub fn session_events_page(
        &self,
        id: &str,
        q: crate::record::Query,
    ) -> std::io::Result<crate::record::Page> {
        match self.record.get() {
            Some(r) => {
                r.flush_pending(READ_FLUSH);
                r.page(id, q)
            }
            None => crate::record::EventIndex::default().read(
                &self.paths.config_dir.join("sessions"),
                id,
                q,
            ),
        }
    }

    pub fn limits_status(&self) -> LimitsStatus {
        let cfg = self.config();
        // Read before `sup` (they take `state`, which comes first).
        let reason = if !self.automation_enabled.load(Ordering::SeqCst) {
            Some("not_lock_holder")
        } else if self.config_error().is_some() {
            Some("config_error")
        } else {
            None
        };
        let memory_guard = self.memory_guard_status();
        let sup = lock(&self.sup);
        let mut breaches: Vec<Breach> = sup.limits.breaches().cloned().collect();
        breaches.sort_by_key(|b| std::cmp::Reverse(b.breached_unix));
        LimitsStatus {
            report_only: cfg.limits.report_only(),
            config: cfg.limits.clone(),
            breaches,
            hogs: sup.limits.hogs().to_vec(),
            paused_by_mulchie: sup.paused.clone(),
            paused_left: sup.paused_left.clone(),
            enforcing: reason.is_none(),
            memory_guard,
            not_enforcing_reason: reason.map(String::from),
        }
    }

    /// Members of a running session as [`ProcSample`]s for a process plan;
    /// the root only with `include_agent`.
    pub(crate) fn session_members(
        &self,
        id: &str,
        include_agent: bool,
    ) -> Result<(Session, Vec<ProcSample>), (&'static str, String)> {
        let (session, members) = {
            let sup = lock(&self.sup);
            let Some(s) = sup.tracker.get(id).cloned() else {
                return Err(("unknown_session", format!("no session {id}")));
            };
            (s, sup.tracker.members(id))
        };
        if session.status != SessionStatus::Running {
            return Err(("unknown_session", format!("session {id} has ended")));
        }
        let root = (session.root_pid, session.root_start_time);
        let want: HashSet<(u32, u64)> = members
            .into_iter()
            .filter(|m| include_agent || *m != root)
            .collect();
        let samples: Vec<ProcSample> = lock(&self.state)
            .snapshot
            .members
            .values()
            .flatten()
            .filter(|p| want.contains(&(p.pid, p.start_time)))
            .cloned()
            .collect();
        Ok((session, samples))
    }
}

/// Findings for active breaches and hogs.
fn limit_findings(limits: &Limits, tracker: &Tracker, now: u64) -> Vec<Finding> {
    let mut out = Vec::new();
    let plan = |sid: &str| {
        Suggestion::call(
            "Plan an action on the session's processes",
            ApiRequest::post(
                "/v1/process/plan",
                json!({"session": sid, "action": "pause"}),
            ),
            true,
        )
    };
    let show = |sid: &str| {
        Suggestion::call(
            "Show the session",
            ApiRequest::get(format!("/v1/sessions/{sid}")),
            true,
        )
    };
    let evidence = |sid: &str, detail: String| {
        let s = tracker.get(sid);
        Evidence {
            pid: s.map(|s| s.root_pid),
            start_time: s.map(|s| s.root_start_time),
            name: s.map(|s| s.agent.clone()),
            cmd: s.map(|s| s.command.clone()),
            cwd: s.and_then(|s| s.cwd.clone()),
            workspace: s.and_then(|s| s.workspace.clone()),
            age_secs: s.map(|s| s.usage.runtime_secs),
            detail: Some(detail),
            ..Evidence::default()
        }
    };
    for b in limits.breaches().filter(|b| b.active()) {
        let state = serde_json::to_value(b.action_state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        let mut detail = format!(
            "breach {}; action {}: {state}",
            b.id,
            serde_json::to_value(b.action)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default()
        );
        if let Some(d) = &b.action_detail {
            detail.push_str(&format!(" ({d})"));
        }
        out.push(Finding {
            id: format!("limit_exceeded:{}:{}", b.session_id, b.cap.code()),
            kind: FindingKind::LimitExceeded,
            category: FindingCategory::Limit,
            severity: if matches!(b.action, LimitAction::Stop | LimitAction::Pause) {
                Severity::High
            } else {
                Severity::Medium
            },
            title: b.sentence(),
            why: "A session over its budget takes CPU, memory or disk from every other agent and build on this machine.".into(),
            evidence: evidence(&b.session_id, detail),
            suggested: vec![show(&b.session_id), plan(&b.session_id)],
            first_seen_unix: b.breached_unix,
            last_seen_unix: now,
            dismissed: false,
            dismissed_until: None,
        });
    }
    for h in limits.hogs() {
        out.push(Finding {
            id: format!("session_hog:{}:{}", h.session_id, h.resource),
            kind: FindingKind::SessionHog,
            category: FindingCategory::Limit,
            severity: Severity::Medium,
            title: format!(
                "Session {} ({}) uses {:.0}% of the machine's {} while the machine is strained",
                h.session_id, h.agent, h.share_pct, h.resource
            ),
            why: "One session holding most of the machine slows every other agent and build. Mulchie only reports this; set limits to act on it.".into(),
            evidence: evidence(&h.session_id, format!("since {}", h.since_unix)),
            suggested: vec![show(&h.session_id), plan(&h.session_id)],
            first_seen_unix: h.since_unix,
            last_seen_unix: now,
            dismissed: false,
            dismissed_until: None,
        });
    }
    out
}

/// `paused.json` could not be read at start: processes an earlier engine
/// paused may still be stopped.
fn paused_file_finding(paths: &Paths, error: &str, now: u64) -> Finding {
    let file = paused_path(paths);
    Finding {
        id: format!("paused_list_unreadable:{}", file.display()),
        kind: FindingKind::PausedListUnreadable,
        category: FindingCategory::Limit,
        severity: Severity::High,
        title: format!(
            "Mulchie could not read its list of paused processes ({})",
            file.display()
        ),
        why: "Processes an earlier Mulchie paused for a limit may still be stopped, and this engine cannot tell which. Look for stopped processes (state T in ps) and continue them, then fix or remove the file.".into(),
        evidence: Evidence {
            path: Some(file),
            detail: Some(error.to_string()),
            ..Evidence::default()
        },
        suggested: vec![Suggestion::call(
            "List stopped processes",
            ApiRequest::get("/v1/state"),
            true,
        )],
        first_seen_unix: now,
        last_seen_unix: now,
        dismissed: false,
        dismissed_until: None,
    }
}

/// Most severe first, then kind and id (as [`watch::finalize`] sorts).
pub(crate) fn sort_findings(v: &mut [Finding]) {
    v.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(a.kind.cmp(&b.kind))
            .then(a.id.cmp(&b.id))
    });
}

#[cfg(test)]
#[path = "engine_sessions_tests.rs"]
mod tests;
