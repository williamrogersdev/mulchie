//! The memory guard wired into the engine.
//!
//! The sample loop builds the closable units from the sample and lets
//! [`MemGuard::evaluate`] decide, in memory. A closing is queued for the
//! limits worker and runs through the safety gate as `Signal` requests that
//! carry the start time the sample saw: SIGTERM, then SIGKILL after
//! `memory_guard.stop_grace_secs` to what is left. Only the engine that
//! automates closes anything; any other engine reports.

use super::sessions::LimitJob;
use super::*;
use crate::config::{MemoryAction, MemoryGuardConfig, MemoryScope};
use crate::consumers::Severity;
use crate::limits::ActionState;
use crate::memguard::{fmt_mb, Candidate, Closing, MemGuard, Memory, Trigger, Unit, UnitKind};
use crate::session::{RootSource, SessionStatus, Tracker};
use crate::watch::{ApiRequest, Evidence, FindingCategory, FindingKind, Suggestion};

/// `state.memory_guard` and `memory_guard` in `GET /v1/limits/status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MemoryGuardStatus {
    pub action: MemoryAction,
    pub scope: MemoryScope,
    /// This engine closes things: the action is `stop` and it automates.
    pub enforcing: bool,
    /// Memory has been low for `sustain_secs`.
    pub low: bool,
    pub low_since_unix: Option<u64>,
    pub free_pct: f32,
    /// Swap in use, as a percent of physical memory.
    #[serde(default)]
    pub swap_pct: f32,
    pub pressure: Option<String>,
    /// What would be closed now, if anything qualifies.
    pub would_close: Option<Candidate>,
    /// What the guard closed or tried to, newest first (at most 50, since
    /// this engine started).
    pub closings: Vec<Closing>,
}

fn status_of(g: &MemGuard, cfg: &MemoryGuardConfig, can_act: bool) -> MemoryGuardStatus {
    MemoryGuardStatus {
        action: cfg.action,
        scope: cfg.scope,
        enforcing: cfg.action == MemoryAction::Stop && can_act,
        low: g.low(),
        low_since_unix: g.low_since(),
        free_pct: g.free_pct(),
        swap_pct: g.swap_pct(),
        pressure: g.pressure().map(String::from),
        would_close: g.candidate().cloned(),
        closings: g.closings().cloned().collect(),
    }
}

/// `pid` and its ancestors among `parent` (pid → parent pid).
fn chain(parent: &HashMap<u32, u32>, pid: u32, into: &mut HashSet<u32>) {
    let mut cur = pid;
    while cur > 1 && into.insert(cur) && into.len() < 4096 {
        match parent.get(&cur) {
            Some(p) => cur = *p,
            None => break,
        }
    }
}

/// The units the guard may close, from one sample.
///
/// - One unit per running session Mulchie detected itself: what the agent
///   started, and the agent too with `include_agent`.
/// - With `scope: all`, every other consumer group whose members are all the
///   user's own, none protected, none named in `never`, and none an ancestor
///   of an agent session or of this engine. Session processes are left out
///   of a group (their session's unit holds them).
///
/// This picks sensibly; the safety gate decides again for every process when
/// the closing runs.
pub(crate) fn build_units(
    tracker: &Tracker,
    snap: &Snapshot,
    cfg: &MemoryGuardConfig,
    sessions_on: bool,
    self_pid: u32,
) -> Vec<Unit> {
    let mem: HashMap<(u32, u64), u64> = snap
        .session_procs
        .iter()
        .map(|p| ((p.pid, p.start_time), p.mem_bytes))
        .collect();
    let name_of: HashMap<u32, &str> = snap
        .session_procs
        .iter()
        .map(|p| (p.pid, p.name.as_str()))
        .collect();
    let never = |n: &str| cfg.never.iter().any(|x| x == n);
    let mb = |b: u64| b as f64 / 1_048_576.0;
    let mut units = Vec::new();
    let mut in_session: HashSet<u32> = HashSet::new();
    let mut roots: Vec<u32> = Vec::new();
    if sessions_on {
        for s in tracker
            .sessions()
            .filter(|s| s.status == SessionStatus::Running)
        {
            let members = tracker.members(&s.id);
            in_session.extend(members.iter().map(|m| m.0));
            roots.push(s.root_pid);
            // A session only a hook registered is reported, never acted on.
            if s.source != RootSource::Detected {
                continue;
            }
            let root = (s.root_pid, s.root_start_time);
            let procs: Vec<(u32, u64)> = members
                .into_iter()
                .filter(|m| cfg.include_agent || *m != root)
                .filter(|m| m.0 != self_pid)
                .filter(|m| !name_of.get(&m.0).is_some_and(|n| never(n)))
                .collect();
            if procs.is_empty() {
                continue;
            }
            let bytes: u64 = procs.iter().filter_map(|m| mem.get(m)).sum();
            let place = s
                .workspace
                .as_ref()
                .and_then(|w| w.file_name())
                .map(|n| format!(" in {}", n.to_string_lossy()))
                .unwrap_or_default();
            units.push(Unit {
                key: format!("session:{}", s.id),
                name: format!("what the {} session{place} started", s.agent),
                kind: UnitKind::Session,
                session_id: Some(s.id.clone()),
                workspace: s.workspace.clone(),
                mem_mb: mb(bytes),
                procs,
            });
        }
    }
    if cfg.scope != MemoryScope::All {
        return units;
    }
    let parent: HashMap<u32, u32> = snap
        .members
        .values()
        .flatten()
        .map(|p| (p.pid, p.ppid))
        .collect();
    // Whoever hosts an agent session (its terminal, editor or app) or this
    // engine is never closed.
    let mut hosts: HashSet<u32> = HashSet::new();
    for r in &roots {
        if let Some(p) = parent.get(r) {
            chain(&parent, *p, &mut hosts);
        }
    }
    chain(&parent, self_pid, &mut hosts);
    for (key, ms) in snap.members.iter() {
        let kind = match group_kind(key) {
            GroupKind::Workspace => UnitKind::Workspace,
            GroupKind::App => UnitKind::App,
            GroupKind::Process => UnitKind::Process,
            GroupKind::System => continue,
        };
        let rest: Vec<&ProcSample> = ms.iter().filter(|p| !in_session.contains(&p.pid)).collect();
        if rest.is_empty()
            || rest.iter().any(|p| {
                !p.own
                    || hosts.contains(&p.pid)
                    || never(&p.name)
                    || crate::consumers::protected_reason(
                        p.pid,
                        &p.name,
                        p.exe.as_deref(),
                        self_pid,
                    )
                    .is_some()
            })
        {
            continue;
        }
        let name = snap
            .consumer(key)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| default_group_name(key, ms));
        units.push(Unit {
            key: key.clone(),
            name,
            kind,
            session_id: None,
            workspace: rest
                .iter()
                .find_map(|p| p.workspace.as_ref().map(|w| w.0.clone())),
            mem_mb: mb(rest.iter().map(|p| p.mem_bytes).sum()),
            procs: rest.iter().map(|p| (p.pid, p.start_time)).collect(),
        });
    }
    units
}

/// The guard's finding while memory is low or a unit is over the cap.
fn guard_findings(g: &MemGuard, cfg: &MemoryGuardConfig, can_act: bool, now: u64) -> Vec<Finding> {
    let cand = g.candidate();
    if !g.low() && cand.is_none() {
        return Vec::new();
    }
    let acts = cfg.action == MemoryAction::Stop && can_act;
    let seen = match (g.low(), g.pressure()) {
        (true, _) if g.by_swap() => format!(
            "Swap is filling up (swap in use is {:.0}% of physical memory, {:.0}% of memory free)",
            g.swap_pct(),
            g.free_pct()
        ),
        (true, Some("critical")) => format!(
            "Memory is low ({:.0}% free, kernel pressure critical)",
            g.free_pct()
        ),
        (true, _) => format!("Memory is low ({:.0}% free)", g.free_pct()),
        (false, _) => "A consumer is over the memory guard's cap".to_string(),
    };
    let scope_words = match cfg.scope {
        MemoryScope::Sessions => "what agent sessions started",
        MemoryScope::All => "your apps, workspaces and programs",
    };
    let risk = if g.by_swap() {
        "When swap fills up the machine stalls, and macOS restarts itself rather than end ordinary programs."
    } else if g.low() {
        "With memory this low the machine starts to swap, stall or crash."
    } else {
        "One consumer holding this much memory leaves less for everything else on the machine."
    };
    let (title, why) = match (cand, acts) {
        (Some(c), true) => (
            format!(
                "{seen}; the memory guard is closing {} ({})",
                c.name,
                fmt_mb(c.mem_mb)
            ),
            format!(
                "{risk} The memory guard closes the biggest of {scope_words}, one every {}s, until memory is back.",
                cfg.cooldown_secs
            ),
        ),
        (Some(c), false) => (
            format!(
                "{seen}; the memory guard would close {} ({})",
                c.name,
                fmt_mb(c.mem_mb)
            ),
            if cfg.action == MemoryAction::Stop {
                format!("{risk} This engine is not the one that acts (another engine holds the lock, or the config did not load), so nothing is closed.")
            } else {
                format!("{risk} The memory guard only reports: set memory_guard.action to stop (`mulchie config set memory_guard.action stop`) to let Mulchie close it.")
            },
        ),
        (None, _) => (
            format!("{seen}; nothing the memory guard may close uses {} MB or more", cfg.min_mb),
            format!(
                "{risk} The memory guard looks at {scope_words}{}. `mulchie top` shows what uses the memory.",
                if cfg.scope == MemoryScope::Sessions {
                    "; memory_guard.scope all lets it look at everything of yours"
                } else {
                    ""
                }
            ),
        ),
    };
    let id = match cand.filter(|c| c.trigger == Trigger::OverCap && !g.low()) {
        Some(c) => format!("memory_low:{}", c.key),
        None => "memory_low".to_string(),
    };
    vec![Finding {
        id,
        kind: FindingKind::MemoryLow,
        category: FindingCategory::Limit,
        severity: Severity::High,
        title,
        why,
        evidence: Evidence {
            name: cand.map(|c| c.name.clone()),
            mem_bytes: cand.map(|c| (c.mem_mb * 1_048_576.0) as u64),
            detail: Some(format!(
                "action {}; {} closing(s) since the engine started",
                serde_json::to_value(cfg.action)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
                g.closings().count()
            )),
            ..Evidence::default()
        },
        suggested: vec![
            Suggestion::call(
                "Show the memory guard",
                ApiRequest::get("/v1/limits/status"),
                true,
            ),
            Suggestion::call("Show what uses memory", ApiRequest::get("/v1/state"), true),
        ],
        first_seen_unix: g.low_since().unwrap_or(now),
        last_seen_unix: now,
        dismissed: false,
        dismissed_until: None,
    }]
}

impl Engine {
    /// Let the memory guard look at one sample. In memory only; a closing is
    /// queued for the limits worker.
    pub(crate) fn observe_memory(&self, now: u64, snap: &Snapshot, cfg: &Config) {
        let g = &cfg.memory_guard;
        let can_act = self.automation_allowed();
        let mem = Memory {
            free_pct: snap.mem_free_pct(),
            pressure: snap.mem_summary.pressure.clone(),
            swap_pct: snap.swap_used_pct(),
        };
        let (queued, announce, changed) = {
            let mut sup = lock(&self.sup);
            let sup = &mut *sup;
            let units = if g.action == MemoryAction::Off {
                Vec::new()
            } else {
                build_units(
                    &sup.tracker,
                    snap,
                    g,
                    cfg.sessions.enabled,
                    self.procs.self_pid(),
                )
            };
            let queued = sup.guard.evaluate(now, &mem, &units, g, can_act);
            if let Some(id) = &queued {
                sup.jobs.push_back(LimitJob::GuardClose { id: id.clone() });
            }
            // Only where nothing is closed: a closing announces itself.
            let acts = g.action == MemoryAction::Stop && can_act;
            let announce = if acts {
                None
            } else {
                sup.guard.take_announcement()
            };
            let fresh = guard_findings(&sup.guard, g, can_act, now);
            let changed = !fresh.is_empty() || !sup.guard_findings.is_empty();
            sup.guard_findings = fresh;
            (queued.is_some(), announce, changed)
        };
        if queued {
            self.limit_wake.notify_all();
        }
        if let Some(c) = announce {
            let seen = if c.trigger == Trigger::SwapHigh {
                format!(
                    "Swap is filling up ({:.0}% of physical memory in use as swap)",
                    mem.swap_pct
                )
            } else {
                format!("Memory is low ({:.0}% free)", mem.free_pct)
            };
            let text = format!(
                "{seen}. Mulchie would close {} ({}), but the memory guard only reports here.",
                c.name,
                fmt_mb(c.mem_mb)
            );
            if self.notify.load(Ordering::SeqCst) {
                crate::notify::notify("Mulchie", &text);
            }
            self.log("memory", text, 0, None, "memory_guard");
        }
        if changed {
            self.publish_limit_findings(now);
        }
    }

    /// `state.memory_guard`.
    pub fn memory_guard_status(&self) -> MemoryGuardStatus {
        let cfg = self.config();
        let can_act = self.automation_allowed();
        status_of(&lock(&self.sup).guard, &cfg.memory_guard, can_act)
    }

    /// Carry out a closing: SIGTERM to every process of the unit through the
    /// gate, and a SIGKILL job for what is left after the grace period.
    pub(crate) fn guard_close(&self, id: &str, now: u64) {
        let Some(c) = lock(&self.sup).guard.get(id).cloned() else {
            return;
        };
        if c.state != ActionState::Pending {
            return;
        }
        let cfg = self.config();
        let refuse = |state: ActionState, why: &str| {
            lock(&self.sup)
                .guard
                .set_result(id, state, why.to_string(), now);
        };
        if cfg.memory_guard.action != MemoryAction::Stop {
            return refuse(
                ActionState::Skipped,
                "memory_guard.action is no longer stop",
            );
        }
        if !self.automation_allowed() {
            return refuse(ActionState::NotEnforced, "automation is off on this engine");
        }
        if self.is_shut_down() {
            return refuse(ActionState::Skipped, "the engine is shutting down");
        }
        let reqs: Vec<Request> = c
            .targets
            .iter()
            .map(|(pid, st)| Request::Signal {
                pid: *pid,
                signal: Signal::Term,
                expected_start_time: *st,
            })
            .collect();
        let results = self.execute(&reqs, "memory_guard");
        let sent: Vec<(u32, u64)> = results
            .iter()
            .zip(&c.targets)
            .filter(|(r, _)| r.is_done())
            .map(|(_, t)| *t)
            .collect();
        let (done, total) = (sent.len(), c.targets.len());
        let mut detail = format!("SIGTERM to {done} of {total} process(es)");
        let skipped = sessions::skipped_words(&results);
        if !skipped.is_empty() {
            detail.push_str(&format!(" (skipped {skipped})"));
        }
        let state = sessions::state_of(done, total);
        {
            let mut sup = lock(&self.sup);
            sup.guard.set_result(id, state, detail.clone(), now);
            if !sent.is_empty() {
                sup.jobs.push_back(LimitJob::GuardKill {
                    id: id.to_string(),
                    targets: sent,
                    at: now.saturating_add(cfg.memory_guard.stop_grace_secs),
                });
            }
        }
        self.limit_wake.notify_all();
        let text = if done > 0 {
            format!("{}: {detail}", c.sentence())
        } else {
            format!(
                "Memory guard could not close {} ({}): {detail}",
                c.name,
                fmt_mb(c.mem_mb)
            )
        };
        if done > 0 && self.notify.load(Ordering::SeqCst) {
            crate::notify::notify("Mulchie", &c.sentence());
        }
        self.log(
            "memory",
            text,
            0,
            c.workspace.as_ref().map(|w| w.display().to_string()),
            "memory_guard",
        );
        self.record_guard(
            &c,
            now,
            if done > 0 { "stopped" } else { "refused" },
            &detail,
        );
    }

    /// SIGKILL what a closing's SIGTERM left running after the grace period
    /// (same pid and start time; the gate checks again).
    pub(crate) fn guard_kill(&self, id: &str, targets: &[(u32, u64)], now: u64) {
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
            let results = self.execute(&reqs, "memory_guard");
            let killed = results.iter().filter(|r| r.is_done()).count();
            note.push_str(&format!("; SIGKILL to {killed} of {}", alive.len()));
            let sk = sessions::skipped_words(&results);
            if !sk.is_empty() {
                note.push_str(&format!(" (skipped {sk})"));
            }
        }
        let c = {
            let mut sup = lock(&self.sup);
            sup.guard.add_detail(id, &note);
            sup.guard.get(id).cloned()
        };
        if let Some(c) = c {
            self.record_guard(&c, now, "killed", &note);
        }
    }

    /// A closing in the session's activity record, as a `memory_guard`
    /// event (only for a session unit, and only where records are written).
    fn record_guard(&self, c: &Closing, now: u64, outcome: &str, detail: &str) {
        let Some(sid) = &c.session_id else { return };
        if !self.records_on(&self.config()) {
            return;
        }
        self.record_writer().event(
            sid,
            now,
            "memory_guard",
            json!({
                "closing_id": c.id, "trigger": c.trigger, "free_pct": c.free_pct,
                "swap_pct": c.swap_pct,
                "memory_mb": c.mem_mb, "processes": c.processes,
                "outcome": outcome, "detail": detail,
            }),
        );
    }
}

#[cfg(test)]
#[path = "engine_memguard_tests.rs"]
mod tests;
