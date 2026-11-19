//! Commit attribution ([`crate::commits`]), build timings
//! ([`crate::timings`]) and their `build_slower` findings, through the
//! engine.
//!
//! Hook calls and `POST /v1/release` only check and queue: no git and no
//! disk on those paths. The `mulchie-perf` worker, started only by the
//! engine that holds the lock, reads `HEAD` with git, appends to
//! `commits.jsonl` and `timings.jsonl`, and stores the `performance`
//! findings. Nothing here signals, deletes or changes anything else.

use super::*;
use crate::commits::{self, CommitEntry};
use crate::config::PerfConfig;
use crate::git;
use crate::session::SessionStatus;
use crate::timings::{self, Timing, TimingReport};

/// How long the worker sleeps when nothing is queued.
const PERF_POLL: Duration = Duration::from_secs(5);
/// Jobs queued at most; past it new ones are dropped and counted.
const MAX_JOBS: usize = 256;
/// Workspaces whose `HEAD` one periodic check reads, at most.
const MAX_POLLED: usize = 32;
/// A session that ended or was idle less than this long ago still counts as
/// working in its workspace.
const RECENT_SECS: u64 = 15 * 60;
/// A workspace with a timing this recent is checked too.
const RECENT_TIMING_SECS: u64 = 3600;
/// One `git rev-parse` may take this long.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// A session as attribution names it: id and agent.
type SessionRef = (String, String);

pub(crate) enum PerfJob {
    /// Read `HEAD` of a workspace now; `session` is the one a hook named.
    Head {
        workspace: PathBuf,
        session: Option<SessionRef>,
    },
    Timing(Box<PendingTiming>),
}

pub(crate) struct PendingTiming {
    at_unix: u64,
    report: TimingReport,
    label: String,
    session: Option<SessionRef>,
}

#[derive(Default)]
pub(crate) struct PerfState {
    /// Read from disk on first use, then kept up to date (the newest
    /// [`commits::KEPT`] and [`timings::KEPT`]).
    commits: Option<Vec<CommitEntry>>,
    timings: Option<Vec<Timing>>,
    /// The last `HEAD` seen per workspace.
    heads: HashMap<PathBuf, String>,
    jobs: VecDeque<PerfJob>,
    dropped: u64,
    last_poll: u64,
    /// The settings the stored findings were built with; `None`: rebuild.
    published: Option<PerfConfig>,
    /// A timing came in since the findings were built.
    dirty: bool,
    /// The `build_slower` findings last built, stored together with the
    /// endpoint findings (one category).
    build_findings: Vec<Finding>,
}

impl PerfState {
    fn load(&mut self, paths: &Paths) {
        if self.commits.is_none() {
            let mut c: Vec<CommitEntry> = commits::file(paths).read_all();
            c.drain(..c.len().saturating_sub(commits::KEPT));
            self.heads = commits::latest_heads(&c);
            self.commits = Some(c);
        }
        if self.timings.is_none() {
            let mut t: Vec<Timing> = timings::file(paths).read_all();
            t.drain(..t.len().saturating_sub(timings::KEPT));
            self.timings = Some(t);
        }
    }

    pub(super) fn commits(&self) -> &[CommitEntry] {
        self.commits.as_deref().unwrap_or_default()
    }

    fn timings(&self) -> &[Timing] {
        self.timings.as_deref().unwrap_or_default()
    }
}

#[derive(Default)]
pub(crate) struct Perf {
    state: Mutex<PerfState>,
    wake: Condvar,
}

impl Perf {
    pub(crate) fn wake_all(&self) {
        self.wake.notify_all();
    }
}

/// `GET /v1/commits`.
#[derive(Debug, Clone, Serialize)]
pub struct CommitList {
    /// Newest first.
    pub commits: Vec<CommitEntry>,
    /// How many matched before `limit`.
    pub total: usize,
}

/// `GET /v1/timings`.
#[derive(Debug, Clone, Serialize)]
pub struct TimingList {
    /// Newest first.
    pub timings: Vec<Timing>,
    /// How many matched before `limit`.
    pub total: usize,
}

/// Whether `p` is `root` or inside it, comparing as given and with
/// symbolic links resolved.
fn under(p: &Path, root: &Path) -> bool {
    if p.starts_with(root) {
        return true;
    }
    match (std::fs::canonicalize(p), std::fs::canonicalize(root)) {
        (Ok(a), Ok(b)) => a.starts_with(b),
        _ => false,
    }
}

/// A workspace as recorded: symbolic links resolved when it exists.
fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The checkout holding `dir`, symbolic links resolved.
pub(super) fn checkout_root(dir: &Path) -> Option<PathBuf> {
    let d = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    super::hook::checkout_of(&d).0
}

impl Engine {
    /// This engine holds the engine lock now: only it writes the files.
    pub(super) fn keeps_perf(&self) -> bool {
        self.automation_enabled.load(Ordering::SeqCst)
            && lock::running_engine(&self.paths).is_some_and(|h| h.pid == std::process::id())
    }

    pub(super) fn with_perf<R>(&self, f: impl FnOnce(&mut PerfState) -> R) -> R {
        let mut g = lock(&self.perf.state);
        g.load(&self.paths);
        f(&mut g)
    }

    fn queue_perf(&self, job: PerfJob) {
        let mut g = lock(&self.perf.state);
        if g.jobs.len() >= MAX_JOBS {
            g.dropped += 1;
            return;
        }
        g.jobs.push_back(job);
        drop(g);
        self.perf.wake.notify_all();
    }

    /// The session `pid` (or an ancestor) belongs to, from the last sample.
    fn session_of_pid(&self, pid: u32) -> Option<SessionRef> {
        let sup = lock(&self.sup);
        let id = sup.tracker.session_of_ancestry(pid)?;
        let agent = sup.tracker.get(&id).map(|s| s.agent.clone())?;
        Some((id, agent))
    }

    /// The session working in `ws` that did something last: running, or
    /// ended or idle less than [`RECENT_SECS`] ago.
    fn session_in(&self, ws: &Path, now: u64) -> Option<SessionRef> {
        let cands: Vec<(u64, String, String, Vec<PathBuf>)> = {
            let sup = lock(&self.sup);
            sup.tracker
                .sessions()
                .filter(|s| {
                    s.status == SessionStatus::Running
                        || now.saturating_sub(s.last_active_unix.max(s.ended_unix.unwrap_or(0)))
                            < RECENT_SECS
                })
                .map(|s| {
                    let paths = s.workspace.iter().chain(s.cwd.iter()).cloned().collect();
                    (s.last_active_unix, s.id.clone(), s.agent.clone(), paths)
                })
                .collect()
        };
        cands
            .into_iter()
            .filter(|(_, _, _, ps)| ps.iter().any(|p| under(p, ws)))
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, id, agent, _)| (id, agent))
    }

    /// Workspaces to read `HEAD` of: where sessions work or recently
    /// worked, where a timing came from in the last hour, and where apps
    /// sending traces run.
    fn active_workspaces(&self, now: u64) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = {
            let sup = lock(&self.sup);
            sup.tracker
                .sessions()
                .filter(|s| {
                    s.status == SessionStatus::Running
                        || now.saturating_sub(s.last_active_unix) < RECENT_SECS
                })
                .filter_map(|s| s.workspace.clone().or_else(|| s.cwd.clone()))
                .collect()
        };
        let mut out: Vec<PathBuf> = Vec::new();
        for d in dirs {
            if out.len() >= MAX_POLLED {
                break;
            }
            if d == self.paths.home {
                continue;
            }
            if let Some(r) = checkout_root(&d) {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
        let recent: Vec<PathBuf> = self.with_perf(|p| {
            p.timings()
                .iter()
                .rev()
                .take_while(|t| now.saturating_sub(t.at_unix) < RECENT_TIMING_SECS)
                .filter_map(|t| t.workspace.clone())
                .collect()
        });
        for r in recent.into_iter().chain(self.otel_workspaces()) {
            if out.len() >= MAX_POLLED {
                break;
            }
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }

    /// Read `HEAD` of `ws` and record it when it moved. `hint` is the
    /// session a hook named. The first sighting of a workspace is recorded
    /// with no previous head and no session.
    pub(super) fn check_head(
        &self,
        ws: &Path,
        hint: Option<SessionRef>,
        via: &str,
        now: u64,
    ) -> Option<git::Head> {
        let h = git::head(&self.paths, ws, HEAD_TIMEOUT).ok()?;
        let prev = self.with_perf(|p| p.heads.get(ws).cloned());
        if prev.as_deref() == Some(h.sha.as_str()) {
            return Some(h);
        }
        let session = match prev {
            None => None,
            Some(_) => hint.or_else(|| self.session_in(ws, now)),
        };
        let e = CommitEntry {
            at_unix: now,
            workspace: ws.to_path_buf(),
            branch: h.branch.clone(),
            head: h.sha.clone(),
            previous_head: prev,
            session_id: session.as_ref().map(|s| s.0.clone()),
            agent: session.map(|s| s.1),
            via: via.to_string(),
        };
        if self.keeps_perf() {
            // A line that cannot be written costs only history.
            let _ = commits::file(&self.paths).append(&e);
        }
        self.with_perf(|p| {
            p.heads.insert(ws.to_path_buf(), h.sha.clone());
            let c = p.commits.get_or_insert_with(Vec::new);
            c.push(e);
            if c.len() > commits::KEPT {
                c.remove(0);
            }
        });
        Some(h)
    }

    fn record_timing(&self, t: PendingTiming, now: u64) {
        let cwd = t.report.cwd.clone();
        let ws = cwd
            .as_deref()
            .map(|c| checkout_root(c).unwrap_or_else(|| real(c)));
        let session = t
            .session
            .or_else(|| ws.as_deref().and_then(|w| self.session_in(w, now)));
        let head = ws
            .as_deref()
            .and_then(|w| self.check_head(w, session.clone(), "timing", now));
        let command = t
            .report
            .command
            .as_deref()
            .map(timings::clean_command)
            .unwrap_or_default();
        let entry = Timing {
            at_unix: t.at_unix,
            duration_ms: t.report.duration_ms.unwrap_or_default(),
            exit_code: t.report.exit_code,
            key: crate::issues::normalize_command(&command),
            command,
            label: t.label,
            cwd,
            workspace: ws,
            branch: head.as_ref().and_then(|h| h.branch.clone()),
            head: head.map(|h| h.sha),
            session_id: session.as_ref().map(|s| s.0.clone()),
            agent: session.map(|s| s.1),
        };
        if self.keeps_perf() {
            let _ = timings::file(&self.paths).append(&entry);
        }
        self.with_perf(|p| {
            let v = p.timings.get_or_insert_with(Vec::new);
            v.push(entry);
            if v.len() > timings::KEPT {
                v.remove(0);
            }
            p.dirty = true;
        });
    }

    /// The label of the lease or waiter `ticket` and the session of its
    /// process, before it is released.
    pub(crate) fn lease_holder(&self, ticket: u64) -> Option<(String, Option<SessionRef>)> {
        let (label, pid) = {
            let st = lock(&self.state);
            st.slots
                .active
                .iter()
                .find(|l| l.id == ticket)
                .map(|l| (l.label.clone(), l.pid))
                .or_else(|| {
                    st.slots
                        .waiting
                        .iter()
                        .find(|w| w.id == ticket)
                        .map(|w| (w.label.clone(), w.pid))
                })?
        };
        Some((label, pid.and_then(|p| self.session_of_pid(p))))
    }

    /// Queue a timing reported with the release of a slot held as
    /// `holder` (from [`Engine::lease_holder`]). Checks the report only; the
    /// worker records it.
    pub fn queue_timing(
        &self,
        report: TimingReport,
        holder: (String, Option<(String, String)>),
    ) -> Result<(), String> {
        report.check()?;
        self.queue_perf(PerfJob::Timing(Box::new(PendingTiming {
            at_unix: now_unix(),
            report,
            label: holder.0,
            session: holder.1,
        })));
        Ok(())
    }

    /// A hook reported `cmd` run in `cwd` by `session`: when it is a git
    /// command that moves `HEAD`, have the worker read `HEAD` now. A few
    /// `stat`s; no git here.
    pub(crate) fn note_shell_command(&self, cwd: &Path, cmd: &str, session: Option<&str>) {
        if !commits::moves_head(cmd) {
            return;
        }
        let Some(ws) = checkout_root(cwd) else {
            return;
        };
        let session = session.and_then(|id| {
            lock(&self.sup)
                .tracker
                .get(id)
                .map(|s| (id.to_string(), s.agent.clone()))
        });
        self.queue_perf(PerfJob::Head {
            workspace: ws,
            session,
        });
    }

    /// One step of the worker: run the queued jobs, read `HEAD` of the
    /// active workspaces when due, start or stop the trace receiver, assess
    /// endpoints when due, and store the `performance` findings again when
    /// timings, endpoints or settings changed. Tests call it directly.
    pub(crate) fn perf_tick(&self, now: u64) {
        while let Some(job) = self.with_perf(|p| p.jobs.pop_front()) {
            match job {
                PerfJob::Head { workspace, session } => {
                    self.check_head(&workspace, session, "hook", now);
                }
                PerfJob::Timing(t) => self.record_timing(*t, now),
            }
        }
        let cfg = self.config().perf;
        let due = self.with_perf(|p| {
            let due = now >= p.last_poll + cfg.head_check_secs || now < p.last_poll;
            if due {
                p.last_poll = now;
            }
            due
        });
        if due {
            for ws in self.active_workspaces(now) {
                self.check_head(&ws, None, "poll", now);
            }
        }
        let rebuild = self.with_perf(|p| {
            let r = p.dirty || p.published.as_ref() != Some(&cfg);
            p.dirty = false;
            p.published = Some(cfg.clone());
            r
        });
        if rebuild {
            let found = self.with_perf(|p| timings::findings(p.timings(), p.commits(), &cfg, now));
            self.with_perf(|p| p.build_findings = found);
        }
        let had_endpoints = !self.otel_findings().is_empty();
        self.otel_reconcile(now);
        let assessed = self.otel_tick(now).is_some();
        let cleared = had_endpoints && self.otel_findings().is_empty();
        if rebuild || assessed || cleared {
            let mut all = self.with_perf(|p| p.build_findings.clone());
            all.extend(self.otel_findings());
            self.replace_findings(watch::FindingCategory::Performance, all, now);
        }
    }

    /// On the lock holder: start the `mulchie-perf` worker.
    pub(crate) fn start_perf(&self, threads: &mut Vec<JoinHandle<()>>) {
        if !self.automation_enabled.load(Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-perf".into())
            .spawn(move || {
                while !me.is_shut_down() {
                    let r = catch_unwind(AssertUnwindSafe(|| me.perf_tick(now_unix())));
                    if r.is_err() {
                        me.log(
                            "error",
                            "timing and commit worker failed; continuing".into(),
                            0,
                            None,
                            "perf",
                        );
                    }
                    let g = lock(&me.perf.state);
                    if g.jobs.is_empty() && !me.is_shut_down() {
                        let _ = me.perf.wake.wait_timeout(g, PERF_POLL);
                    }
                }
            })
        {
            threads.push(h);
        }
    }

    // ------------------------------------------------------------ reads

    /// Recorded moves of `HEAD`, newest first: in `workspace` when given,
    /// at or after `since`, at most `limit`.
    pub fn commits(
        &self,
        workspace: Option<&Path>,
        since: Option<u64>,
        limit: usize,
    ) -> CommitList {
        let workspace = workspace.map(real);
        let workspace = workspace.as_deref();
        self.with_perf(|p| {
            let m: Vec<&CommitEntry> = p
                .commits()
                .iter()
                .rev()
                .filter(|e| workspace.is_none_or(|w| e.workspace == w))
                .filter(|e| since.is_none_or(|s| e.at_unix >= s))
                .collect();
            CommitList {
                total: m.len(),
                commits: m.into_iter().take(limit).cloned().collect(),
            }
        })
    }

    /// The recorded moves of `HEAD` in `workspace` from `from_sha` to
    /// `to_sha`, oldest first ([`commits::between`]).
    pub fn commits_between(
        &self,
        workspace: &Path,
        from_sha: &str,
        to_sha: &str,
    ) -> Vec<CommitEntry> {
        let ws = real(workspace);
        self.with_perf(|p| commits::between(p.commits(), &ws, from_sha, to_sha))
    }

    /// Who was working in `workspace` when `HEAD` first moved to `sha`
    /// ([`commits::attribute`]).
    pub fn attribute(&self, workspace: &Path, sha: &str) -> Option<CommitEntry> {
        let ws = real(workspace);
        self.with_perf(|p| commits::attribute(p.commits(), &ws, sha).cloned())
    }

    /// Recorded timings, newest first: in `workspace` when given, of
    /// commands whose grouping key contains `command` normalized, at most
    /// `limit`.
    pub fn timings(
        &self,
        workspace: Option<&Path>,
        command: Option<&str>,
        limit: usize,
    ) -> TimingList {
        let want = command.map(crate::issues::normalize_command);
        let workspace = workspace.map(real);
        let workspace = workspace.as_deref();
        self.with_perf(|p| {
            let m: Vec<&Timing> = p
                .timings()
                .iter()
                .rev()
                .filter(|t| workspace.is_none_or(|w| t.workspace.as_deref() == Some(w)))
                .filter(|t| want.as_deref().is_none_or(|c| t.key.contains(c)))
                .collect();
            TimingList {
                total: m.len(),
                timings: m.into_iter().take(limit).cloned().collect(),
            }
        })
    }
}

#[cfg(test)]
#[path = "engine_perf_tests.rs"]
mod tests;
