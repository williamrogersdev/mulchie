//! The memory guard: when the machine stays low on memory, the biggest
//! closable unit is closed (SIGTERM, then SIGKILL), one at a time, until
//! memory is back. A unit is what an agent session started, or, with
//! `scope: all`, any other app, workspace or program of the user.
//!
//! [`MemGuard::evaluate`] is pure and runs on the sample loop. It decides;
//! the engine closes the unit on the limits worker, through the safety gate,
//! which checks every process again at that moment.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crate::config::{MemoryAction, MemoryGuardConfig};
use crate::limits::ActionState;

/// Closings kept for state and the API.
pub const CLOSINGS_KEPT: usize = 50;
/// A unit of which nothing could be closed is left alone this long.
pub const REFUSED_BACKOFF_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// What one agent session started.
    Session,
    Workspace,
    App,
    Process,
}

/// Something the guard may close as a whole.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// `session:<id>` or a consumer group key.
    pub key: String,
    pub name: String,
    pub kind: UnitKind,
    pub session_id: Option<String>,
    pub workspace: Option<PathBuf>,
    /// Resident memory of `procs` together.
    pub mem_mb: f64,
    /// (pid, start time) of every process that would be signalled.
    pub procs: Vec<(u32, u64)>,
}

/// Why a unit was picked.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The machine stayed low on memory; this was the biggest unit.
    MemoryLow,
    /// The unit itself stayed over `unit_max_mb`.
    OverCap,
}

/// The unit the guard would close now (or will, after the cooldown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub key: String,
    pub name: String,
    pub kind: UnitKind,
    pub mem_mb: f64,
    pub processes: usize,
    pub trigger: Trigger,
}

/// One unit the guard closed, or tried to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Closing {
    pub id: String,
    pub at_unix: u64,
    pub trigger: Trigger,
    /// Percent of memory available when it was decided.
    pub free_pct: f32,
    pub pressure: Option<String>,
    pub key: String,
    pub name: String,
    pub kind: UnitKind,
    pub session_id: Option<String>,
    pub workspace: Option<PathBuf>,
    pub mem_mb: f64,
    pub processes: usize,
    pub state: ActionState,
    pub detail: Option<String>,
    #[serde(skip)]
    pub targets: Vec<(u32, u64)>,
}

impl Closing {
    /// One sentence for a person.
    pub fn sentence(&self) -> String {
        let why = match self.trigger {
            Trigger::MemoryLow => format!("memory was {:.0}% free", self.free_pct),
            Trigger::OverCap => "it was over the memory guard's cap".to_string(),
        };
        format!(
            "Mulchie closed {} ({} process(es), {}) because {why}",
            self.name,
            self.processes,
            fmt_mb(self.mem_mb)
        )
    }
}

/// `3.2 GB` or `640 MB`.
pub fn fmt_mb(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

/// Memory as one sample saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub free_pct: f32,
    /// The kernel's level: `normal`, `warning`, `critical`.
    pub pressure: Option<String>,
}

#[derive(Debug, Default)]
pub struct MemGuard {
    low_since: Option<u64>,
    /// Memory has been low for `sustain_secs`.
    low_due: bool,
    free_pct: f32,
    pressure: Option<String>,
    /// Unit key → when it went over `unit_max_mb`.
    over: HashMap<String, u64>,
    /// Unit key → not picked again before this time.
    backoff: HashMap<String, u64>,
    last_closed: Option<u64>,
    candidate: Option<Candidate>,
    closings: VecDeque<Closing>,
    /// The key last announced in notify mode, so one episode notifies once.
    announced: Option<String>,
    seq: u64,
}

impl MemGuard {
    pub fn low(&self) -> bool {
        self.low_due
    }

    pub fn low_since(&self) -> Option<u64> {
        self.low_since
    }

    pub fn free_pct(&self) -> f32 {
        self.free_pct
    }

    pub fn pressure(&self) -> Option<&str> {
        self.pressure.as_deref()
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        self.candidate.as_ref()
    }

    /// Newest first.
    pub fn closings(&self) -> impl Iterator<Item = &Closing> {
        self.closings.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Closing> {
        self.closings.iter().find(|c| c.id == id)
    }

    /// The candidate, the first time it is seen in an episode (for a
    /// notification in notify mode).
    pub fn take_announcement(&mut self) -> Option<Candidate> {
        let c = self.candidate.clone()?;
        if self.announced.as_deref() == Some(c.key.as_str()) {
            return None;
        }
        self.announced = Some(c.key.clone());
        Some(c)
    }

    /// What the gate did for a closing. A unit of which nothing was closed
    /// is left alone for [`REFUSED_BACKOFF_SECS`].
    pub fn set_result(&mut self, id: &str, state: ActionState, detail: String, now: u64) {
        let Some(c) = self.closings.iter_mut().find(|c| c.id == id) else {
            return;
        };
        c.state = state;
        c.detail = Some(detail);
        if matches!(state, ActionState::Skipped | ActionState::NotEnforced) {
            self.backoff
                .insert(c.key.clone(), now.saturating_add(REFUSED_BACKOFF_SECS));
        }
    }

    /// Add to a closing's detail (the SIGKILL after the grace period).
    pub fn add_detail(&mut self, id: &str, note: &str) {
        if let Some(c) = self.closings.iter_mut().find(|c| c.id == id) {
            let d = c.detail.take().unwrap_or_default();
            c.detail = Some(if d.is_empty() {
                note.to_string()
            } else {
                format!("{d}; {note}")
            });
        }
    }

    /// Look at one sample. Returns the id of a new closing the engine must
    /// carry out, when `cfg.action` is `stop`, `can_act` is true and the
    /// cooldown has passed.
    pub fn evaluate(
        &mut self,
        now: u64,
        mem: &Memory,
        units: &[Unit],
        cfg: &MemoryGuardConfig,
        can_act: bool,
    ) -> Option<String> {
        self.free_pct = mem.free_pct;
        self.pressure = mem.pressure.clone();
        if cfg.action == MemoryAction::Off {
            self.low_since = None;
            self.low_due = false;
            self.over.clear();
            self.candidate = None;
            self.announced = None;
            return None;
        }
        let low = mem.free_pct < cfg.free_pct
            || (cfg.on_pressure && mem.pressure.as_deref() == Some("critical"));
        if low {
            self.low_since.get_or_insert(now);
        } else {
            self.low_since = None;
        }
        let sustained = |since: u64| now.saturating_sub(since) >= cfg.sustain_secs;
        self.low_due = self.low_since.is_some_and(sustained);

        let cap = cfg.unit_max_mb as f64;
        let is_over = |u: &Unit| cap > 0.0 && u.mem_mb > cap;
        self.over
            .retain(|k, _| units.iter().any(|u| &u.key == k && is_over(u)));
        for u in units.iter().filter(|u| is_over(u)) {
            self.over.entry(u.key.clone()).or_insert(now);
        }
        self.backoff.retain(|_, until| *until > now);

        let open = |u: &&Unit| !u.procs.is_empty() && !self.backoff.contains_key(&u.key);
        let biggest = |a: &&Unit, b: &&Unit| a.mem_mb.total_cmp(&b.mem_mb);
        let for_low = self
            .low_due
            .then(|| {
                units
                    .iter()
                    .filter(open)
                    .filter(|u| u.mem_mb >= cfg.min_mb as f64)
                    .max_by(biggest)
            })
            .flatten()
            .map(|u| (u, Trigger::MemoryLow));
        let pick = for_low.or_else(|| {
            units
                .iter()
                .filter(open)
                .filter(|u| self.over.get(&u.key).copied().is_some_and(sustained))
                .max_by(biggest)
                .map(|u| (u, Trigger::OverCap))
        });
        self.candidate = pick.map(|(u, trigger)| Candidate {
            key: u.key.clone(),
            name: u.name.clone(),
            kind: u.kind,
            mem_mb: u.mem_mb,
            processes: u.procs.len(),
            trigger,
        });
        let Some((u, trigger)) = pick else {
            self.announced = None;
            return None;
        };
        if cfg.action != MemoryAction::Stop || !can_act {
            return None;
        }
        if self
            .last_closed
            .is_some_and(|t| now < t.saturating_add(cfg.cooldown_secs))
        {
            return None;
        }
        self.seq += 1;
        let id = format!("m-{now}-{}", self.seq);
        self.closings.push_front(Closing {
            id: id.clone(),
            at_unix: now,
            trigger,
            free_pct: mem.free_pct,
            pressure: mem.pressure.clone(),
            key: u.key.clone(),
            name: u.name.clone(),
            kind: u.kind,
            session_id: u.session_id.clone(),
            workspace: u.workspace.clone(),
            mem_mb: u.mem_mb,
            processes: u.procs.len(),
            state: ActionState::Pending,
            detail: None,
            targets: u.procs.clone(),
        });
        self.closings.truncate(CLOSINGS_KEPT);
        self.last_closed = Some(now);
        // Its processes take a moment to exit: not picked again meanwhile.
        self.backoff
            .insert(u.key.clone(), now.saturating_add(cfg.cooldown_secs));
        self.over.remove(&u.key);
        Some(id)
    }
}

#[cfg(test)]
#[path = "memguard_tests.rs"]
mod tests;
