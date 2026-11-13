//! Runaway chains wired into the engine.
//!
//! The sample loop finds the chains in the sample and lets
//! [`Runaway::evaluate`] decide, in memory. A stopping is queued for the
//! limits worker and runs through the safety gate: SIGSTOP to the chain from
//! a fresh process list, again until a pass finds nothing new (a stopped
//! process starts nothing, and what it started in the meantime is still
//! under it), then SIGKILL to all of it. Killing without the freeze would
//! race the chain: the newest level starts the next one about every second,
//! and a level started between the list and the kill would carry on as an
//! orphan. Only the engine that automates stops anything; any other engine
//! reports.

use super::sessions::LimitJob;
use super::*;
use crate::config::{MemoryAction, RunawayConfig};
use crate::consumers::Severity;
use crate::limits::ActionState;
use crate::memguard::fmt_mb;
use crate::runaway::{detect, tree, Chain, Memo, Runaway, Stopping};
use crate::watch::{ApiRequest, Evidence, FindingCategory, FindingKind, Suggestion};

/// Freeze passes over a chain, at most.
const FREEZE_PASSES: usize = 8;

/// `state.runaway` and `runaway` in `GET /v1/limits/status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RunawayStatus {
    pub action: MemoryAction,
    /// Copies of a command nested inside each other that make a chain.
    pub depth: usize,
    /// This engine stops chains: the action is `stop` and it automates.
    pub enforcing: bool,
    /// The chains running now.
    pub chains: Vec<Chain>,
    /// What Mulchie stopped or tried to, newest first (at most 50, since
    /// this engine started).
    pub stoppings: Vec<Stopping>,
    /// Commands the `runaway_repeat` guard rule matches now.
    pub remembered: Vec<Memo>,
}

fn status_of(r: &Runaway, cfg: &RunawayConfig, can_act: bool) -> RunawayStatus {
    RunawayStatus {
        action: cfg.action,
        depth: cfg.depth,
        enforcing: cfg.action == MemoryAction::Stop && can_act,
        chains: r.chains().to_vec(),
        stoppings: r.stoppings().cloned().collect(),
        remembered: r.memos().to_vec(),
    }
}

const RISK: &str = "A command that starts itself adds a level about every second and every level keeps its memory, so the machine runs out of memory and swap within minutes.";

/// One finding per chain running now.
fn chain_findings(r: &Runaway, cfg: &RunawayConfig, can_act: bool, now: u64) -> Vec<Finding> {
    let acts = cfg.action == MemoryAction::Stop && can_act;
    r.chains()
        .iter()
        .map(|c| {
            let what = if acts {
                "Mulchie is stopping the chain (it freezes every process of it, then kills them).".to_string()
            } else if cfg.action == MemoryAction::Stop {
                "This engine is not the one that acts (another engine holds the lock, or the config did not load), so nothing is stopped.".to_string()
            } else {
                format!(
                    "Mulchie only reports here: `mulchie config set runaway.action stop` lets it stop such a chain. To stop this one now, run `mulchie signal {} kill --tree`.",
                    c.root_pid
                )
            };
            Finding {
                id: format!("runaway_chain:{}:{}", c.root_pid, c.root_start_time),
                kind: FindingKind::RunawayChain,
                category: FindingCategory::Limit,
                severity: Severity::High,
                title: format!("{}{}", c.sentence(), if acts { "; Mulchie is stopping it" } else { "" }),
                why: format!("{RISK} {what}"),
                evidence: Evidence {
                    pid: Some(c.root_pid),
                    start_time: Some(c.root_start_time),
                    cmd: Some(c.command.clone()),
                    cwd: c.cwd.clone(),
                    workspace: c.workspace.clone(),
                    mem_bytes: Some((c.mem_mb * 1_048_576.0) as u64),
                    detail: Some(format!(
                        "{} copies nested inside each other, {} process(es) in all{}",
                        c.depth,
                        c.processes,
                        c.session_id
                            .as_ref()
                            .map(|s| format!(", in session {s}"))
                            .unwrap_or_default()
                    )),
                    ..Evidence::default()
                },
                suggested: vec![
                    Suggestion::call(
                        "Plan killing the chain",
                        ApiRequest::post(
                            "/v1/process/plan",
                            json!({"pid": c.root_pid, "signal": "kill", "tree": true}),
                        ),
                        false,
                    ),
                    Suggestion::call(
                        "Show runaway chains",
                        ApiRequest::get("/v1/limits/status"),
                        true,
                    ),
                ],
                first_seen_unix: if c.first_seen_unix == 0 { now } else { c.first_seen_unix },
                last_seen_unix: now,
                dismissed: false,
                dismissed_until: None,
            }
        })
        .collect()
}

impl Engine {
    /// Look for runaway chains in one sample. In memory only; a stopping is
    /// queued for the limits worker.
    pub(crate) fn observe_runaway(&self, now: u64, snap: &Snapshot, cfg: &Config) {
        let r = &cfg.runaway;
        let can_act = self.automation_allowed();
        let self_pid = self.procs.self_pid();
        let mut chains = if r.action == MemoryAction::Off {
            Vec::new()
        } else {
            detect(&snap.session_procs, r.depth)
        };
        // Never Mulchie itself or something it runs under.
        chains.retain(|c| !c.procs.iter().any(|p| p.0 == self_pid));
        let acts = r.action == MemoryAction::Stop && can_act;
        let (queued, fresh, changed) = {
            let mut sup = lock(&self.sup);
            let sup = &mut *sup;
            if cfg.sessions.enabled {
                for c in &mut chains {
                    c.session_id = sup
                        .tracker
                        .session_of(c.root_pid, c.root_start_time)
                        .map(String::from);
                }
            }
            let d = sup.runaway.evaluate(now, chains, r, can_act);
            for id in &d.stop {
                sup.jobs.push_back(LimitJob::RunawayStop { id: id.clone() });
            }
            let found = chain_findings(&sup.runaway, r, can_act, now);
            let changed = !found.is_empty() || !sup.runaway_findings.is_empty();
            sup.runaway_findings = found;
            (!d.stop.is_empty(), d.fresh, changed)
        };
        if queued {
            self.limit_wake.notify_all();
        }
        for c in fresh {
            let text = format!(
                "{}. {}",
                c.sentence(),
                if acts {
                    "Mulchie is stopping it."
                } else {
                    "Mulchie only reports here; nothing was stopped."
                }
            );
            // Where it is stopped, the stopping announces itself.
            if !acts && self.notify.load(Ordering::SeqCst) {
                crate::notify::notify("Mulchie", &text);
            }
            self.log(
                "runaway",
                text,
                0,
                c.workspace.as_ref().map(|w| w.display().to_string()),
                "runaway",
            );
        }
        if changed {
            self.publish_limit_findings(now);
        }
    }

    /// `state.runaway`.
    pub fn runaway_status(&self) -> RunawayStatus {
        let cfg = self.config();
        let can_act = self.automation_allowed();
        status_of(&lock(&self.sup).runaway, &cfg.runaway, can_act)
    }

    /// The commands the `runaway_repeat` guard rule matches now.
    pub(crate) fn runaway_memos(&self) -> Vec<Memo> {
        lock(&self.sup).runaway.memos().to_vec()
    }

    /// Carry out a stopping: freeze the chain, then kill it, all through
    /// the gate.
    pub(crate) fn runaway_stop(&self, id: &str, now: u64) {
        let Some(s) = lock(&self.sup).runaway.get(id).cloned() else {
            return;
        };
        if s.state != ActionState::Pending {
            return;
        }
        let cfg = self.config();
        let refuse = |state: ActionState, why: &str| {
            lock(&self.sup)
                .runaway
                .set_result(id, state, why.to_string());
        };
        if cfg.runaway.action != MemoryAction::Stop {
            return refuse(ActionState::Skipped, "runaway.action is no longer stop");
        }
        if !self.automation_allowed() {
            return refuse(ActionState::NotEnforced, "automation is off on this engine");
        }
        if self.is_shut_down() {
            return refuse(ActionState::Skipped, "the engine is shutting down");
        }
        let signal = |targets: &[(u32, u64)], signal: Signal| -> Vec<ActionResult> {
            let reqs: Vec<Request> = targets
                .iter()
                .map(|(pid, st)| Request::Signal {
                    pid: *pid,
                    signal,
                    expected_start_time: *st,
                })
                .collect();
            self.execute(&reqs, "runaway")
        };
        // Freeze until a pass finds nothing that was not asked to stop yet.
        let mut asked: HashSet<(u32, u64)> = HashSet::new();
        let mut frozen: HashSet<(u32, u64)> = HashSet::new();
        for _ in 0..FREEZE_PASSES {
            let now_there = tree(&self.procs.list(), s.root);
            let new: Vec<(u32, u64)> = now_there.into_iter().filter(|t| asked.insert(*t)).collect();
            if new.is_empty() {
                break;
            }
            let results = signal(&new, Signal::Stop);
            frozen.extend(
                results
                    .iter()
                    .zip(&new)
                    .filter(|(r, _)| r.is_done())
                    .map(|(_, t)| *t),
            );
        }
        if asked.is_empty() {
            return refuse(ActionState::Skipped, "the chain had already ended");
        }
        let targets = tree(&self.procs.list(), s.root);
        let results = signal(&targets, Signal::Kill);
        let killed: HashSet<(u32, u64)> = results
            .iter()
            .zip(&targets)
            .filter(|(r, _)| r.is_done())
            .map(|(_, t)| *t)
            .collect();
        let (done, total) = (killed.len(), targets.len().max(asked.len()));
        let mut detail = format!(
            "froze {} and killed {done} of {total} process(es)",
            frozen.len()
        );
        let skipped = sessions::skipped_words(&results);
        if !skipped.is_empty() {
            detail.push_str(&format!(" (skipped {skipped})"));
        }
        // Nothing stays frozen that was not killed.
        let left: Vec<(u32, u64)> = frozen
            .iter()
            .copied()
            .filter(|t| !killed.contains(t))
            .filter(|(p, st)| self.procs.get(*p).is_some_and(|f| f.start_time == *st))
            .collect();
        if !left.is_empty() {
            let resumed = signal(&left, Signal::Cont)
                .iter()
                .filter(|r| r.is_done())
                .count();
            detail.push_str(&format!("; resumed {resumed} it could not kill"));
        }
        let state = sessions::state_of(done, total);
        lock(&self.sup)
            .runaway
            .set_result(id, state, detail.clone());
        let text = if done > 0 {
            format!("{}: {detail}", s.sentence())
        } else {
            format!(
                "Mulchie could not stop the runaway `{}` ({} process(es), {}): {detail}",
                s.command,
                s.processes,
                fmt_mb(s.mem_mb)
            )
        };
        if done > 0 && self.notify.load(Ordering::SeqCst) {
            crate::notify::notify("Mulchie", &format!("{}.", s.sentence()));
        }
        self.log(
            "runaway",
            text,
            0,
            s.workspace.as_ref().map(|w| w.display().to_string()),
            "runaway",
        );
        if let Some(sid) = &s.session_id {
            if self.records_on(&cfg) {
                self.record_writer().event(
                    sid,
                    now,
                    "runaway",
                    json!({
                        "stopping_id": s.id, "command": s.command, "cwd": s.cwd,
                        "depth": s.depth, "processes": s.processes, "memory_mb": s.mem_mb,
                        "outcome": if done > 0 { "stopped" } else { "refused" },
                        "detail": detail,
                    }),
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "engine_runaway_tests.rs"]
mod tests;
