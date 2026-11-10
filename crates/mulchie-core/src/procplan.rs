//! Plans for acting on processes: one pid, a pid and its descendants, or a
//! consumer group ("quit Google Chrome", "stop builds in repo › agent-x").
//!
//! A plan is a list of gate [`Request`]s, each carrying the start time the
//! planner saw, plus who is affected and how. Nothing here acts: the caller
//! sends the approved requests back (`POST /v1/process/execute`) and the gate
//! rechecks every one.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::consumers::{main_member, GroupAction, GroupKind, ProcSample};
use crate::monitor::ProcKind;
use crate::process::ProcFacts;
use crate::safety::{PlanItem, Request, Signal};

/// `POST /v1/process/plan`. Either `pid` (with `signal`, default `term`, and
/// `tree` for its descendants too) or `group` with `action`.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct ProcessPlanRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<Signal>,
    /// With `pid`: also every descendant (children first).
    #[serde(default)]
    pub tree: bool,
    /// A consumer group key from `snapshot.consumers` (`app:…`, `workspace:…`,
    /// `process:…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<GroupAction>,
    /// The caller's own pid, when it knows it: groups holding the caller or
    /// one of its ancestors (its terminal, its agent app) are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pid: Option<u32>,
}

/// How one process is affected by a plan.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Affected {
    pub pid: u32,
    pub name: String,
    pub start_time: u64,
    /// `signalled` (a request names it), `exits_with_app` (an app helper or
    /// child that ends when the main process quits), `reniced`.
    pub how: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcessPlan {
    pub created_unix: u64,
    /// `pid <n>` or the group key.
    pub target: String,
    pub name: String,
    /// The signal (`term`, `kill`, …) or group action (`quit`, …).
    pub action: String,
    /// At least one item is ready and nothing refused the plan.
    pub ready: bool,
    /// Why there is nothing to do: `protected`, `unknown_group`,
    /// `unknown_pid`, `restart_unavailable`, `no_builds`, `invalid_request`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// One sentence for a person.
    pub detail: String,
    /// Send the `request` of each ready item to `POST /v1/process/execute`.
    pub items: Vec<PlanItem>,
    pub affected: Vec<Affected>,
    /// For a restart: what happens after the quit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
}

impl ProcessPlan {
    pub fn refused(now: u64, target: &str, action: &str, code: &str, detail: String) -> Self {
        Self {
            created_unix: now,
            target: target.into(),
            name: target.into(),
            action: action.into(),
            ready: false,
            refused: Some(code.into()),
            detail,
            items: Vec::new(),
            affected: Vec::new(),
            restart: None,
        }
    }

    /// The requests of the ready items, in order.
    pub fn approved(&self) -> Vec<Request> {
        self.items
            .iter()
            .filter(|i| i.ready)
            .map(|i| i.request.clone())
            .collect()
    }
}

/// What a group action turns into, before the gate checks each request.
#[derive(Debug, Clone, PartialEq)]
pub struct Drafted {
    pub requests: Vec<Request>,
    pub affected: Vec<Affected>,
    pub restart: Option<String>,
    pub detail: String,
}

fn sig(p: &ProcSample, signal: Signal) -> Request {
    Request::Signal {
        pid: p.pid,
        signal,
        expected_start_time: p.start_time,
    }
}

fn affected(p: &ProcSample, how: &str) -> Affected {
    Affected {
        pid: p.pid,
        name: p.name.clone(),
        start_time: p.start_time,
        how: how.into(),
    }
}

/// Members whose parent is not in the group.
fn roots(ms: &[ProcSample]) -> Vec<&ProcSample> {
    let pids: HashSet<u32> = ms.iter().map(|p| p.pid).collect();
    ms.iter().filter(|p| !pids.contains(&p.ppid)).collect()
}

/// Turn a group action into requests. `Err((code, detail))` when it cannot be
/// done. `members` are the group's processes from the latest sample.
#[allow(clippy::too_many_arguments)]
pub fn draft_group(
    kind: GroupKind,
    name: &str,
    bundle: Option<&Path>,
    bundle_id: Option<&str>,
    members: &[ProcSample],
    action: GroupAction,
    nice: i32,
    macos: bool,
) -> Result<Drafted, (&'static str, String)> {
    if members.is_empty() {
        return Err(("unknown_group", format!("{name} has no running processes")));
    }
    let all_signalled = |s: Signal| -> (Vec<Request>, Vec<Affected>) {
        (
            members.iter().map(|p| sig(p, s)).collect(),
            members.iter().map(|p| affected(p, "signalled")).collect(),
        )
    };
    let quit = || -> (Vec<Request>, Vec<Affected>, String) {
        let targets: Vec<&ProcSample> = if kind == GroupKind::App {
            main_member(members, bundle).into_iter().collect()
        } else {
            roots(members)
        };
        let ids: HashSet<u32> = targets.iter().map(|p| p.pid).collect();
        let reqs = targets.iter().map(|p| sig(p, Signal::Term)).collect();
        let aff = members
            .iter()
            .map(|p| {
                if ids.contains(&p.pid) {
                    affected(p, "signalled")
                } else {
                    affected(p, "exits_with_app")
                }
            })
            .collect();
        let detail = if kind == GroupKind::App {
            format!(
                "Ask {name} to quit (SIGTERM to its main process); its {} other process(es) should exit with it",
                members.len().saturating_sub(1)
            )
        } else {
            format!(
                "Ask {} top-level process(es) of {name} to exit (SIGTERM); {} process(es) in all",
                targets.len(),
                members.len()
            )
        };
        (reqs, aff, detail)
    };
    let (requests, affected_list, restart, detail) = match action {
        GroupAction::Quit => {
            let (r, a, d) = quit();
            (r, a, None, d)
        }
        GroupAction::ForceQuit => {
            let (r, a) = all_signalled(Signal::Kill);
            let d = format!(
                "Force quit {name}: SIGKILL to all {} process(es); unsaved work in them is lost",
                members.len()
            );
            (r, a, None, d)
        }
        GroupAction::Pause => {
            let (r, a) = all_signalled(Signal::Stop);
            let d = format!(
                "Pause all {} process(es) of {name} (SIGSTOP)",
                members.len()
            );
            (r, a, None, d)
        }
        GroupAction::Resume => {
            let (r, a) = all_signalled(Signal::Cont);
            let d = format!(
                "Resume all {} process(es) of {name} (SIGCONT)",
                members.len()
            );
            (r, a, None, d)
        }
        GroupAction::LowerPriority => {
            let r = members
                .iter()
                .map(|p| Request::Renice {
                    pid: p.pid,
                    nice,
                    expected_start_time: p.start_time,
                })
                .collect();
            let a = members.iter().map(|p| affected(p, "reniced")).collect();
            let d = format!(
                "Lower the priority of all {} process(es) of {name} to nice {nice}",
                members.len()
            );
            (r, a, None, d)
        }
        GroupAction::StopBuilds => {
            let builds: Vec<&ProcSample> = members
                .iter()
                .filter(|p| p.kind == ProcKind::Build)
                .collect();
            if builds.is_empty() {
                return Err((
                    "no_builds",
                    format!("{name} has no build processes running"),
                ));
            }
            let r = builds.iter().map(|p| sig(p, Signal::Term)).collect();
            let a = builds.iter().map(|p| affected(p, "signalled")).collect();
            let d = format!(
                "Stop {} build process(es) in {name} (SIGTERM)",
                builds.len()
            );
            (r, a, None, d)
        }
        GroupAction::Restart => {
            let id = match (kind, bundle_id, macos) {
                (GroupKind::App, Some(id), true) => id,
                (GroupKind::App, _, false) => {
                    return Err(("restart_unavailable", "restarting an app needs macOS".into()))
                }
                (GroupKind::App, None, _) => {
                    return Err((
                        "restart_unavailable",
                        format!("{name} has no bundle id, so Mulchie cannot reopen it"),
                    ))
                }
                _ => {
                    return Err((
                        "restart_unavailable",
                        "only an application can be restarted; Mulchie cannot restart workspace or command-line processes".into(),
                    ))
                }
            };
            let Some(main) = main_member(members, bundle) else {
                return Err(("unknown_group", format!("{name} has no main process")));
            };
            let (mut r, a, _) = quit();
            r.push(Request::ReopenApp {
                bundle_id: id.to_string(),
                after_pid: main.pid,
                after_start_time: main.start_time,
            });
            let how = format!(
                "after pid {} exits (up to {}s), open -b {id}",
                main.pid,
                crate::safety::REOPEN_WAIT.as_secs()
            );
            let d = format!("Restart {name}: ask it to quit, then reopen it");
            (r, a, Some(how), d)
        }
    };
    Ok(Drafted {
        requests,
        affected: affected_list,
        restart,
        detail,
    })
}

/// `pid` and, with `tree`, its descendants from a fresh process list,
/// deepest first so children are signalled before their parent.
pub fn draft_pid(
    procs: &[ProcFacts],
    pid: u32,
    signal: Signal,
    tree: bool,
) -> Result<Drafted, (&'static str, String)> {
    let Some(root) = procs.iter().find(|p| p.pid == pid) else {
        return Err(("unknown_pid", format!("no process {pid}")));
    };
    let mut kids: HashMap<u32, Vec<&ProcFacts>> = HashMap::new();
    for p in procs {
        kids.entry(p.ppid).or_default().push(p);
    }
    let mut order: Vec<(&ProcFacts, usize)> = vec![(root, 0)];
    if tree {
        let mut i = 0;
        while i < order.len() && order.len() < 2000 {
            let (p, depth) = order[i];
            if let Some(cs) = kids.get(&p.pid) {
                for c in cs {
                    if c.pid != p.pid && c.pid > 1 {
                        order.push((c, depth + 1));
                    }
                }
            }
            i += 1;
        }
    }
    order.sort_by_key(|(p, d)| (std::cmp::Reverse(*d), p.pid));
    let requests = order
        .iter()
        .map(|(p, _)| Request::Signal {
            pid: p.pid,
            signal,
            expected_start_time: p.start_time,
        })
        .collect();
    let affected = order
        .iter()
        .map(|(p, _)| Affected {
            pid: p.pid,
            name: p.name.clone(),
            start_time: p.start_time,
            how: "signalled".into(),
        })
        .collect();
    let detail = if order.len() > 1 {
        format!(
            "{} {} (pid {pid}) and {} descendant(s)",
            signal.name(),
            root.name,
            order.len() - 1
        )
    } else {
        format!("{} {} (pid {pid})", signal.name(), root.name)
    };
    Ok(Drafted {
        requests,
        affected,
        restart: None,
        detail,
    })
}

#[cfg(test)]
#[path = "procplan_tests.rs"]
mod tests;
