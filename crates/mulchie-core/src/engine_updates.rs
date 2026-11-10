//! Update checks on their own thread (`mulchie-updates`), started only by
//! the engine that holds the lock. The first automatic check runs no sooner
//! than five minutes after start, then every `updates.interval_hours` (sooner
//! after errors, with backoff); `POST /v1/updates/check` asks for one now.
//! Results are cached in `<config dir>/updates.json`, so a restart shows the
//! last findings without asking the network again. `updates.check: off`
//! makes no request and runs no command.

use super::*;
use crate::config::UpdateCheck;
use crate::updates::{self, Cache, Http, Report, Runner};
use crate::watch::FindingCategory;

/// The updates worker wakes at least this often.
const UPDATES_POLL: Duration = Duration::from_secs(30);

pub(crate) struct UpdateState {
    cache: Cache,
    checking: bool,
    manual: bool,
    pub(crate) started_unix: u64,
}

pub(crate) struct Updates {
    http: Arc<dyn Http>,
    runner: Arc<dyn Runner>,
    pub(crate) state: Mutex<UpdateState>,
    pub(crate) wake: Condvar,
}

impl Updates {
    pub(crate) fn new(http: Arc<dyn Http>, runner: Arc<dyn Runner>) -> Self {
        Self {
            http,
            runner,
            state: Mutex::new(UpdateState {
                cache: Cache::default(),
                checking: false,
                manual: false,
                started_unix: now_unix(),
            }),
            wake: Condvar::new(),
        }
    }
}

/// `GET /v1/updates`, and compact in `state.updates`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UpdatesStatus {
    /// `updates.check` is on.
    pub enabled: bool,
    pub checking: bool,
    pub checked_unix: Option<u64>,
    /// No automatic check before this.
    pub next_check_unix: Option<u64>,
    /// Consecutive checks with errors.
    pub failures: u32,
    /// Open findings of category `update`.
    pub available: usize,
    /// Why sources failed in the last check.
    pub errors: Vec<String>,
    /// The last check in full (omitted in state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
}

impl Engine {
    /// Use these for update checks (tests use fakes).
    pub fn with_update_sources(mut self, http: Arc<dyn Http>, runner: Arc<dyn Runner>) -> Self {
        self.updates = Arc::new(Updates::new(http, runner));
        self
    }

    /// On the lock holder: show the cached findings and start the worker.
    pub(crate) fn start_updates(&self, threads: &mut Vec<JoinHandle<()>>) {
        if !self.automation_enabled.load(Ordering::SeqCst) {
            return;
        }
        let cache = updates::load_cache(&self.paths.config_dir);
        let cached = cache.report.clone();
        {
            let mut st = lock(&self.updates.state);
            st.cache = cache;
            st.started_unix = now_unix();
        }
        if self.config().updates.check == UpdateCheck::On {
            if let Some(r) = cached {
                self.replace_findings(FindingCategory::Update, updates::findings(&r), now_unix());
            }
        }
        let me = self.clone();
        if let Ok(h) = std::thread::Builder::new()
            .name("mulchie-updates".into())
            .spawn(move || {
                while !me.is_shut_down() {
                    let r = catch_unwind(AssertUnwindSafe(|| me.update_tick(now_unix())));
                    if r.is_err() {
                        me.log(
                            "error",
                            "update check failed; retrying later".into(),
                            0,
                            None,
                            "updates",
                        );
                        let mut st = lock(&me.updates.state);
                        st.checking = false;
                        st.cache.next_unix = now_unix() + updates::RETRY_BASE_SECS;
                    }
                    let st = lock(&me.updates.state);
                    if !st.manual && !me.is_shut_down() {
                        let _ = me.updates.wake.wait_timeout(st, UPDATES_POLL);
                    }
                }
            })
        {
            threads.push(h);
        }
    }

    /// One worker step at `now`: check when due.
    pub(crate) fn update_tick(&self, now: u64) {
        let on = self.config().updates.check == UpdateCheck::On;
        let due = {
            let st = lock(&self.updates.state);
            st.manual
                || (now >= st.started_unix + updates::FIRST_CHECK_DELAY_SECS
                    && now >= st.cache.next_unix)
        };
        if !on {
            lock(&self.updates.state).manual = false;
            self.replace_findings(FindingCategory::Update, Vec::new(), now);
            return;
        }
        if due {
            self.run_update_check(now);
        }
    }

    /// Check now (on the calling thread) and store the findings.
    pub fn run_update_check(&self, now: u64) -> Report {
        let cfg = self.config();
        if cfg.updates.check == UpdateCheck::Off {
            lock(&self.updates.state).manual = false;
            self.replace_findings(FindingCategory::Update, Vec::new(), now);
            return Report {
                checked_unix: now,
                ..Report::default()
            };
        }
        let prev = {
            let mut st = lock(&self.updates.state);
            st.checking = true;
            st.manual = false;
            st.cache.report.clone()
        };
        let workspaces = self.update_workspaces();
        let exe = std::env::current_exe().ok();
        let input = updates::CheckInput {
            cfg: &cfg.updates,
            home: &self.paths.home,
            exe: exe.as_deref(),
            version: env!("CARGO_PKG_VERSION"),
            workspaces: &workspaces,
            brew_cache: Some(updates::brew_api_cache(&self.paths.home)),
            now,
        };
        let report = updates::check(
            &*self.updates.http,
            &*self.updates.runner,
            &input,
            prev.as_ref(),
        );
        let cache = {
            let mut st = lock(&self.updates.state);
            st.checking = false;
            st.cache.failures = if report.errors.is_empty() {
                0
            } else {
                st.cache.failures.saturating_add(1)
            };
            st.cache.next_unix = updates::next_check(
                now,
                cfg.updates.interval_hours.saturating_mul(3600),
                st.cache.failures,
            );
            st.cache.report = Some(report.clone());
            st.cache.clone()
        };
        if let Err(e) = updates::save_cache(&self.paths.config_dir, &cache) {
            self.log(
                "error",
                format!("Could not save update check results: {e}"),
                0,
                None,
                "updates",
            );
        }
        self.replace_findings(FindingCategory::Update, updates::findings(&report), now);
        report
    }

    /// Workspaces in use: those in the last sample and those of sessions.
    fn update_workspaces(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = lock(&self.state)
            .snapshot
            .workspaces
            .iter()
            .filter(|w| w.exists)
            .map(|w| w.path.clone())
            .collect();
        v.extend(
            lock(&self.sup)
                .tracker
                .sessions()
                .filter_map(|s| s.workspace.clone()),
        );
        v.sort();
        v.dedup();
        v.truncate(updates::MAX_WORKSPACES);
        v
    }

    /// Ask the worker for a check now. Refused when checks are off.
    pub fn request_update_check(&self) -> Result<(), (&'static str, String)> {
        if self.config().updates.check == UpdateCheck::Off {
            return Err((
                "updates_off",
                "update checks are off (updates.check); nothing is checked".into(),
            ));
        }
        lock(&self.updates.state).manual = true;
        self.updates.wake.notify_all();
        Ok(())
    }

    /// Where update checks stand; `full` adds the whole last report.
    pub fn updates_status(&self, full: bool) -> UpdatesStatus {
        let enabled = self.config().updates.check == UpdateCheck::On;
        let available = self.updates_available();
        let st = lock(&self.updates.state);
        let r = st.cache.report.as_ref();
        UpdatesStatus {
            enabled,
            checking: st.checking,
            checked_unix: r.map(|r| r.checked_unix),
            next_check_unix: enabled.then(|| {
                st.cache
                    .next_unix
                    .max(st.started_unix + updates::FIRST_CHECK_DELAY_SECS)
            }),
            failures: st.cache.failures,
            available,
            errors: r.map(|r| r.errors.clone()).unwrap_or_default(),
            report: if full { r.cloned() } else { None },
        }
    }

    /// Open update findings.
    pub(crate) fn updates_available(&self) -> usize {
        lock(&self.state)
            .findings
            .iter()
            .filter(|f| f.category == FindingCategory::Update && !f.dismissed)
            .count()
    }
}

#[cfg(test)]
#[path = "engine_updates_tests.rs"]
mod tests;
