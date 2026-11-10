//! Session limits: when a session stays over a cap for `sustain_secs`, a
//! breach is raised once, its action runs once (through the safety gate, by
//! the engine), and it clears when the session is back under the cap.
//!
//! [`Limits::evaluate`] is pure and runs on the sample loop. It decides; the
//! engine acts on the events it returns, on its own worker thread.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::config::{Budget, LimitAction, LimitsConfig};
use crate::session::{Session, SessionStatus};

/// What a cap measures.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CapKind {
    CpuCores,
    MemoryMb,
    Processes,
    WriteMbPerHour,
    RuntimeHours,
}

impl CapKind {
    pub const ALL: [CapKind; 5] = [
        CapKind::CpuCores,
        CapKind::MemoryMb,
        CapKind::Processes,
        CapKind::WriteMbPerHour,
        CapKind::RuntimeHours,
    ];

    pub fn code(self) -> &'static str {
        match self {
            CapKind::CpuCores => "cpu_cores",
            CapKind::MemoryMb => "memory_mb",
            CapKind::Processes => "processes",
            CapKind::WriteMbPerHour => "write_mb_per_hour",
            CapKind::RuntimeHours => "runtime_hours",
        }
    }

    /// The figure in words, for titles.
    pub fn words(self) -> &'static str {
        match self {
            CapKind::CpuCores => "CPU cores",
            CapKind::MemoryMb => "MB of memory",
            CapKind::Processes => "processes",
            CapKind::WriteMbPerHour => "MB written per hour",
            CapKind::RuntimeHours => "hours of runtime",
        }
    }

    /// The session's current value; `None` when it is not known (the write
    /// rate on a platform that does not report writes).
    pub fn value(self, s: &Session) -> Option<f64> {
        let u = &s.usage;
        match self {
            CapKind::CpuCores => Some(u.cpu_cores),
            CapKind::MemoryMb => Some(u.memory_mb),
            CapKind::Processes => Some(u.processes as f64),
            CapKind::WriteMbPerHour => u.write_mb_per_hour,
            CapKind::RuntimeHours => Some(u.runtime_secs as f64 / 3600.0),
        }
    }

    fn cap(self, b: &Budget) -> Option<&crate::config::Cap> {
        match self {
            CapKind::CpuCores => b.cpu_cores.as_ref(),
            CapKind::MemoryMb => b.memory_mb.as_ref(),
            CapKind::Processes => b.processes.as_ref(),
            CapKind::WriteMbPerHour => b.write_mb_per_hour.as_ref(),
            CapKind::RuntimeHours => b.runtime_hours.as_ref(),
        }
    }
}

/// Where a breach's action stands.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    /// Queued for the limits worker.
    Pending,
    /// Carried out for every target.
    Done,
    /// Carried out for some targets; the rest were skipped (see detail).
    Partial,
    /// Nothing was done (every target skipped, or no targets).
    Skipped,
    /// The action is `notify`.
    NotifyOnly,
    /// Not enforced here: the session was registered by a hook, or
    /// automation is off. Only noted and notified.
    NotEnforced,
}

/// One breach of one cap by one session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Breach {
    pub id: String,
    pub session_id: String,
    pub agent: String,
    pub cap: CapKind,
    pub max: f64,
    /// The value when the breach was raised.
    pub value: f64,
    /// The highest value while it lasted.
    pub peak: f64,
    pub action: LimitAction,
    pub sustain_secs: u64,
    /// After clearing, paused processes resume this much later.
    pub resume_after_secs: u64,
    /// When the value first went over the cap.
    pub over_since_unix: u64,
    /// When the breach was raised (over for `sustain_secs`).
    pub breached_unix: u64,
    pub action_state: ActionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_detail: Option<String>,
    /// When the session went back under the cap (or ended).
    pub cleared_unix: Option<u64>,
    /// Priority restored or processes resumed after clearing.
    pub restored: bool,
    /// Handed to the agent through the hook (exactly once).
    pub delivered: bool,
}

impl Breach {
    pub fn active(&self) -> bool {
        self.cleared_unix.is_none()
    }

    /// One sentence for a person or an agent.
    pub fn sentence(&self) -> String {
        let what = match self.action {
            LimitAction::Notify => "Mulchie noted it".to_string(),
            LimitAction::Deprioritize => "Mulchie lowers the session's priority".to_string(),
            LimitAction::Pause => "Mulchie pauses the session's processes".to_string(),
            LimitAction::Stop => "Mulchie stops the session's processes".to_string(),
        };
        let done = match self.action_state {
            ActionState::NotEnforced => " (not enforced here)",
            ActionState::Skipped => " (nothing could be done)",
            _ => "",
        };
        format!(
            "Session {} ({}) has used {:.1} {} for over {}s, above its limit of {}; {what}{done}.",
            self.session_id,
            self.agent,
            self.value,
            self.cap.words(),
            self.sustain_secs,
            fmt_num(self.max)
        )
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// What the engine must do after an evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitEvent {
    Breached {
        breach_id: String,
    },
    Cleared {
        breach_id: String,
    },
    /// A paused breach has been clear for `resume_after_secs`.
    ResumeDue {
        breach_id: String,
    },
}

/// A session using at least `hog_share_pct` of the machine's CPU or memory
/// for `hog_sustain_secs` while health is strained or critical (with or
/// without budgets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hog {
    pub session_id: String,
    pub agent: String,
    /// `cpu` or `memory`.
    pub resource: String,
    /// Percent of the machine.
    pub share_pct: f64,
    pub since_unix: u64,
}

/// The machine, for the hog share.
#[derive(Debug, Clone, Copy)]
pub struct Machine {
    pub cores: usize,
    pub mem_mb: f64,
    /// Health is strained or critical: only then is a hog reported.
    pub strained: bool,
}

#[derive(Debug, Default, Clone)]
struct CapState {
    over_since: Option<u64>,
    active: Option<String>,
}

/// Cleared breaches kept for state and the API.
pub const CLEARED_KEPT: usize = 100;

#[derive(Debug, Default)]
pub struct Limits {
    caps: HashMap<(String, CapKind), CapState>,
    breaches: BTreeMap<String, Breach>,
    hog_since: HashMap<(String, &'static str), u64>,
    hogs: Vec<Hog>,
    resume_requested: Vec<String>,
}

impl Limits {
    pub fn breaches(&self) -> impl Iterator<Item = &Breach> {
        self.breaches.values()
    }

    pub fn get(&self, id: &str) -> Option<&Breach> {
        self.breaches.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Breach> {
        self.breaches.get_mut(id)
    }

    pub fn hogs(&self) -> &[Hog] {
        &self.hogs
    }

    /// Breaches of `session` not yet handed to its agent, now marked
    /// delivered. Each breach is returned exactly once.
    pub fn take_undelivered(&mut self, session: &str) -> Vec<Breach> {
        let mut out = Vec::new();
        for b in self.breaches.values_mut() {
            if b.session_id == session && !b.delivered {
                b.delivered = true;
                out.push(b.clone());
            }
        }
        out
    }

    /// Check every session against its budget. `budget_for` gives a
    /// session's budget (per-workspace overrides, else the default).
    pub fn evaluate<'a>(
        &mut self,
        now: u64,
        sessions: impl Iterator<Item = &'a Session>,
        cfg: &LimitsConfig,
        machine: Machine,
    ) -> Vec<LimitEvent> {
        let mut events = Vec::new();
        let mut touched: Vec<(String, CapKind)> = Vec::new();
        let mut hogs_now: Vec<Hog> = Vec::new();
        let mut hog_seen: Vec<(String, &'static str)> = Vec::new();
        for s in sessions {
            if s.status != SessionStatus::Running {
                continue;
            }
            if machine.strained {
                let cores = machine.cores.max(1) as f64;
                let checks: [(&'static str, f64); 2] = [
                    ("cpu", s.usage.cpu_cores * 100.0 / cores),
                    (
                        "memory",
                        if machine.mem_mb > 0.0 {
                            s.usage.memory_mb * 100.0 / machine.mem_mb
                        } else {
                            0.0
                        },
                    ),
                ];
                for (res, share) in checks {
                    let key = (s.id.clone(), res);
                    if share >= cfg.hog_share_pct as f64 {
                        let since = *self.hog_since.entry(key.clone()).or_insert(now);
                        hog_seen.push(key);
                        if now.saturating_sub(since) >= cfg.hog_sustain_secs {
                            hogs_now.push(Hog {
                                session_id: s.id.clone(),
                                agent: s.agent.clone(),
                                resource: res.to_string(),
                                share_pct: share,
                                since_unix: since,
                            });
                        }
                    }
                }
            }
            let Some(budget) = cfg.budget_for(s.workspace.as_deref()) else {
                continue;
            };
            for kind in CapKind::ALL {
                let Some(cap) = kind.cap(budget) else {
                    continue;
                };
                let key = (s.id.clone(), kind);
                touched.push(key.clone());
                let st = self.caps.entry(key).or_default();
                let value = kind.value(s);
                let over = value.is_some_and(|v| v > cap.max);
                if over {
                    let v = value.unwrap_or(0.0);
                    let since = *st.over_since.get_or_insert(now);
                    if let Some(id) = &st.active {
                        if let Some(b) = self.breaches.get_mut(id) {
                            b.peak = b.peak.max(v);
                        }
                    } else if now.saturating_sub(since) >= cap.sustain_secs {
                        let id = format!("b-{}-{}-{now}", s.id, kind.code());
                        self.breaches.insert(
                            id.clone(),
                            Breach {
                                id: id.clone(),
                                session_id: s.id.clone(),
                                agent: s.agent.clone(),
                                cap: kind,
                                max: cap.max,
                                value: v,
                                peak: v,
                                action: cap.action,
                                sustain_secs: cap.sustain_secs,
                                resume_after_secs: budget.resume_after_secs,
                                over_since_unix: since,
                                breached_unix: now,
                                action_state: ActionState::Pending,
                                action_detail: None,
                                cleared_unix: None,
                                restored: false,
                                delivered: false,
                            },
                        );
                        st.active = Some(id.clone());
                        events.push(LimitEvent::Breached { breach_id: id });
                    }
                } else {
                    st.over_since = None;
                    if let Some(id) = st.active.take() {
                        if let Some(b) = self.breaches.get_mut(&id) {
                            b.cleared_unix = Some(now);
                        }
                        events.push(LimitEvent::Cleared { breach_id: id });
                    }
                }
            }
        }
        // Caps not checked this time clear their breaches: the session
        // ended or left the list, or its budget or cap was removed.
        let mut cleared = Vec::new();
        self.caps.retain(|key, st| {
            if touched.contains(key) {
                return true;
            }
            if let Some(id) = st.active.take() {
                cleared.push(id);
            }
            false
        });
        for id in cleared {
            if let Some(b) = self.breaches.get_mut(&id) {
                b.cleared_unix = Some(now);
            }
            events.push(LimitEvent::Cleared { breach_id: id });
        }
        self.hog_since.retain(|k, _| hog_seen.contains(k));
        self.hogs = hogs_now;

        // Resume paused processes once a breach has been clear long enough.
        for b in self.breaches.values() {
            let paused = b.action == LimitAction::Pause
                && matches!(b.action_state, ActionState::Done | ActionState::Partial);
            let due = b
                .cleared_unix
                .is_some_and(|c| now >= c.saturating_add(b.resume_after_secs));
            if paused && due && !b.restored && !self.resume_requested.contains(&b.id) {
                self.resume_requested.push(b.id.clone());
                events.push(LimitEvent::ResumeDue {
                    breach_id: b.id.clone(),
                });
            }
        }
        self.prune();
        events
    }

    /// Forget all but the newest [`CLEARED_KEPT`] cleared breaches that need
    /// nothing more.
    fn prune(&mut self) {
        let mut done: Vec<(u64, String)> = self
            .breaches
            .values()
            .filter(|b| {
                b.cleared_unix.is_some()
                    && (b.restored
                        || !(b.action == LimitAction::Pause
                            && matches!(b.action_state, ActionState::Done | ActionState::Partial)))
            })
            .map(|b| (b.cleared_unix.unwrap_or(0), b.id.clone()))
            .collect();
        if done.len() > CLEARED_KEPT {
            done.sort();
            for (_, id) in done.iter().take(done.len() - CLEARED_KEPT) {
                self.breaches.remove(id);
                self.resume_requested.retain(|r| r != id);
            }
        }
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
