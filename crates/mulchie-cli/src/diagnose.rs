//! One compact, prioritised picture of the machine, for people (`mulchie
//! diagnose`) and agents (the `diagnose` MCP tool): health and why, what is
//! using CPU, memory and disk, the build queue, reclaimable space, engine
//! problems, and recommendations that each name the exact next tool call.
//!
//! It is built from the engine's `/v1/state` and `/v1/advice` JSON, so the
//! CLI and MCP produce the same thing. Lists are capped; totals are given.

use crate::hooks_install::sh_quote as q;
use mulchie_core::human_bytes;
use serde_json::{json, Value};

const TOP_WORKSPACES: usize = 6;
const TOP_PROCESSES: usize = 5;
const TOP_ITEMS: usize = 5;
const TOP_FINDINGS: usize = 6;
const TOP_GROUPS: usize = 3;

/// Said wherever findings are shown.
pub const FINDINGS_NOTE: &str = "Findings are observations from simple heuristics (what a process is doing, where it runs from, how long it has run), not verdicts: none of them means malware was found. Each says why it was raised; check before acting.";

/// Package caches smaller than this are not worth a recommendation.
const PRUNE_WORTH: u64 = 1_000_000_000;

/// Where the figures came from.
pub enum Origin<'a> {
    Engine,
    /// A one-off local sample and scan; the reason no engine was used.
    Local(&'a str),
    /// No figures at all.
    Unavailable(&'a str),
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn sz(b: u64) -> String {
    human_bytes(b)
}

struct Rec {
    priority: u8,
    id: &'static str,
    title: String,
    reason: String,
    gain_bytes: Option<u64>,
    gain: String,
    risk: &'static str,
    next_call: Option<Value>,
    command: Option<String>,
}

impl Rec {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "reason": self.reason,
            "expected_gain": self.gain,
            "expected_gain_bytes": self.gain_bytes,
            "risk": self.risk,
            "next_call": self.next_call,
            "cli": self.command,
        })
    }
}

fn call(name: &str, args: Value) -> Option<Value> {
    Some(json!({ "name": name, "arguments": args }))
}

/// The consumer group key of a workspace.
fn ws_key(w: &Value) -> String {
    format!("workspace:{}", w["path"].as_str().unwrap_or(""))
}

fn severity_rank(s: &Value) -> u8 {
    match s.as_str() {
        Some("high") => 0,
        Some("medium") => 1,
        Some("low") => 2,
        _ => 3,
    }
}

/// The MCP tool (and CLI command) for a suggestion's `api_request`: only
/// calls that look or plan, never one that acts. `None` for a route with
/// no such tool.
pub fn tool_for_api_request(r: &Value) -> Option<(&'static str, Value, Option<String>)> {
    let route = r["route"].as_str()?;
    let (path, _) = route.split_once('?').unwrap_or((route, ""));
    let body = &r["body"];
    match (r["method"].as_str()?, path) {
        ("GET", "/v1/state") => Some(("get_status", json!({}), Some("mulchie status".into()))),
        ("GET", "/v1/findings") => {
            Some(("list_findings", json!({}), Some("mulchie findings".into())))
        }
        ("GET", "/v1/sessions") => {
            Some(("list_sessions", json!({}), Some("mulchie sessions".into())))
        }
        ("GET", "/v1/limits/status") => {
            Some(("limits_status", json!({}), Some("mulchie limits".into())))
        }
        ("GET", "/v1/updates") => Some(("list_updates", json!({}), Some("mulchie updates".into()))),
        ("POST", "/v1/findings/dismiss") => {
            let id = body["id"].as_str()?;
            Some((
                "dismiss_finding",
                json!({ "id": id }),
                Some(format!("mulchie findings dismiss {}", q(id))),
            ))
        }
        ("POST", "/v1/process/plan") => {
            let mut args = serde_json::Map::new();
            for k in [
                "pid",
                "signal",
                "tree",
                "group",
                "action",
                "session",
                "include_agent",
            ] {
                if !body[k].is_null() {
                    args.insert(k.into(), body[k].clone());
                }
            }
            let line = match (
                body["session"].as_str(),
                body["group"].as_str(),
                body["pid"].as_u64(),
            ) {
                (Some(sid), _, _) => body["action"].as_str().map(|a| {
                    format!(
                        "mulchie group {} {}{}",
                        q(&format!("session:{sid}")),
                        q(a),
                        if body["include_agent"] == true {
                            " --include-agent"
                        } else {
                            ""
                        }
                    )
                }),
                (_, Some(g), _) => body["action"]
                    .as_str()
                    .map(|a| format!("mulchie group {} {}", q(g), q(a))),
                (_, _, Some(p)) => body["signal"].as_str().map(|sig| {
                    format!(
                        "mulchie signal {p} {}{}",
                        q(sig),
                        if body["tree"] == true { " --tree" } else { "" }
                    )
                }),
                _ => None,
            };
            Some(("plan_process_action", Value::Object(args), line))
        }
        ("GET", p) => {
            if let Some(rest) = p.strip_prefix("/v1/sessions/") {
                let (id, events) = match rest.strip_suffix("/events") {
                    Some(id) => (id, true),
                    None => (rest, false),
                };
                crate::sessions::check_session_id(id).ok()?;
                return Some(if events {
                    (
                        "get_session",
                        json!({ "id": id, "events": true }),
                        Some(format!("mulchie sessions {} --events", q(id))),
                    )
                } else {
                    (
                        "get_session",
                        json!({ "id": id }),
                        Some(format!("mulchie sessions {}", q(id))),
                    )
                });
            }
            let pid: u32 = p.strip_prefix("/v1/process/")?.parse().ok()?;
            Some((
                "inspect_process",
                json!({ "pid": pid }),
                Some(format!("mulchie inspect {pid}")),
            ))
        }
        _ => None,
    }
}

/// A finding, short, with the tool calls (all of them look or plan) and CLI
/// commands that follow from its suggestions.
pub fn finding_brief(f: &Value) -> Value {
    let ev = &f["evidence"];
    let pid = ev["pid"].as_u64();
    let mut next = Vec::new();
    let mut cli = Vec::new();
    for sug in f["suggested"].as_array().into_iter().flatten() {
        // The engine's own call, as data, when it gives one.
        if let Some((name, args, line)) = tool_for_api_request(&sug["api_request"]) {
            if name != "dismiss_finding" {
                next.push(json!({ "label": sug["label"], "name": name, "arguments": args }));
                cli.extend(line);
            }
            continue;
        }
        let r = &sug["request"];
        if r["action"] == "signal" {
            if let (Some(p), Some(sig)) = (r["pid"].as_u64(), r["signal"].as_str()) {
                next.push(json!({ "label": sug["label"], "name": "plan_process_action", "arguments": { "pid": p, "signal": sig } }));
                cli.push(format!("mulchie signal {p} {}", q(sig)));
            }
        }
    }
    if let Some(p) = pid {
        next.push(json!({ "label": "Look at the process", "name": "inspect_process", "arguments": { "pid": p } }));
        cli.push(format!("mulchie inspect {p}"));
    }
    if let Some(id) = f["id"].as_str() {
        next.push(json!({ "label": "Hide it if the user says it is expected", "name": "dismiss_finding", "arguments": { "id": id } }));
        cli.push(format!("mulchie findings dismiss {}", q(id)));
    }
    let mut evidence = serde_json::Map::new();
    for k in [
        "pid",
        "name",
        "exe",
        "cmd",
        "cwd",
        "workspace",
        "path",
        "listen",
        "cpu_pct",
        "detail",
    ] {
        if !ev[k].is_null() {
            evidence.insert(k.into(), ev[k].clone());
        }
    }
    if let Some(m) = ev["mem_bytes"].as_u64() {
        evidence.insert("memory".into(), json!(sz(m)));
    }
    json!({
        "id": f["id"],
        "kind": f["kind"],
        "category": f["category"],
        "severity": f["severity"],
        "title": f["title"],
        "why": f["why"],
        "evidence": evidence,
        "next_calls": next,
        "cli": cli,
    })
}

/// Build the picture. `state` and `advice` may be `Null` when unavailable.
pub fn build(state: &Value, advice: &Value, origin: Origin) -> Value {
    let snap = &state["snapshot"];
    let cfg = &state["config"];
    let mut recs: Vec<Rec> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    // ------------------------------------------------------------ engine
    let (source, running) = match &origin {
        Origin::Engine => ("engine", true),
        Origin::Local(why) => {
            problems.push(format!("{why}; these figures are a one-off local look"));
            ("local", false)
        }
        Origin::Unavailable(why) => {
            problems.push(why.to_string());
            ("none", false)
        }
    };
    if !running {
        recs.push(Rec {
            priority: 0,
            id: "start_engine",
            title: "Start the Mulchie engine".into(),
            reason: "Without the engine there is no live monitoring, no build queue and no automatic protection; ask the user to start it.".into(),
            gain_bytes: None,
            gain: "live monitoring, the build queue, automatic cleanup when enabled".into(),
            risk: "none",
            next_call: None,
            command: Some("mulchie serve   (or: mulchie service install, or open the Mulchie app)".into()),
        });
    }
    if let Some(e) = state["config_error"].as_str() {
        problems.push(format!("config: {e}"));
        recs.push(Rec {
            priority: 0,
            id: "fix_config",
            title: "Fix the Mulchie config file".into(),
            reason: format!(
                "{e}. Mulchie runs with safe defaults and automatic cleanup off until it is fixed."
            ),
            gain_bytes: None,
            gain: "the user's settings take effect again".into(),
            risk: "none",
            next_call: None,
            command: Some("mulchie doctor".into()),
        });
    }
    let stale = running && state["health"] == "stale";
    if stale {
        problems.push(format!(
            "the engine has not sampled for {}s; figures are old",
            u(state, "sample_age_secs")
        ));
        recs.push(Rec {
            priority: 0,
            id: "restart_engine",
            title: "Restart the Mulchie engine".into(),
            reason: "Its sampling has stopped, so health and advice are out of date.".into(),
            gain_bytes: None,
            gain: "current figures".into(),
            risk: "none",
            next_call: None,
            command: Some("mulchie doctor   (then restart the app or the service)".into()),
        });
    }
    let restarts =
        u(&state["loops"]["sample"], "restarts") + u(&state["loops"]["scan"], "restarts");
    if restarts > 0 {
        problems.push(format!(
            "the engine's worker loops restarted {restarts} time(s) after a crash"
        ));
    }

    if matches!(origin, Origin::Unavailable(_)) {
        return finish(source, state, json!(null), problems, recs, None);
    }

    // ------------------------------------------------------------ health
    let mem_total = u(snap, "mem_total");
    let mem_avail = u(snap, "mem_available");
    let mem_free_pct = if mem_total > 0 {
        mem_avail as f64 * 100.0 / mem_total as f64
    } else {
        100.0
    };
    let disk_free = u(snap, "disk_free");
    let disk_total = u(snap, "disk_total");
    let low_disk = u(cfg, "low_disk_gb").max(1) * 1_000_000_000;
    let mem_floor = f(cfg, "throttle_mem_free_pct");
    let cores = u(snap, "cpu_count");
    let load1 = snap["load"][0].as_f64().unwrap_or(0.0);
    let mut why: Vec<String> = Vec::new();
    let disk_low = disk_total > 0 && disk_free < low_disk;
    if disk_total > 0 && disk_free < low_disk / 3 {
        why.push(format!(
            "disk critically low: {} free (warning level {})",
            sz(disk_free),
            sz(low_disk)
        ));
    } else if disk_low {
        why.push(format!(
            "disk low: {} free (warning level {})",
            sz(disk_free),
            sz(low_disk)
        ));
    }
    if mem_total > 0 && mem_free_pct < mem_floor {
        why.push(format!(
            "memory pressure: {mem_free_pct:.0}% free (floor {mem_floor:.0}%)"
        ));
    }
    if cores > 0 && load1 > cores as f64 * 4.0 {
        why.push(format!("overloaded: load {load1:.0} on {cores} cores"));
    } else if cores > 0 && load1 > cores as f64 * 2.0 {
        why.push(format!("heavy load: {load1:.0} on {cores} cores"));
    }
    if f(snap, "cpu_pct") > f(cfg, "throttle_cpu_pct") && f(cfg, "throttle_cpu_pct") > 0.0 {
        why.push(format!("CPU {:.0}% busy", f(snap, "cpu_pct")));
    }
    let swap = u(snap, "swap_used");
    if mem_total > 0 && swap > mem_total / 4 {
        why.push(format!("{} of swap in use", sz(swap)));
    }
    if stale {
        why.push("figures are stale".into());
    }
    if why.is_empty() {
        why.push("no pressure on disk, memory or CPU".into());
    }

    // -------------------------------------------------------- workspaces
    let mut ws: Vec<Value> = snap["workspaces"].as_array().cloned().unwrap_or_default();
    ws.sort_by(|a, b| {
        let k = |w: &Value| f(w, "cpu_pct") + u(w, "mem_bytes") as f64 / 1e8;
        k(b).total_cmp(&k(a))
    });
    let ws_total = ws.len();
    let top_ws: Vec<Value> = ws
        .iter()
        .take(TOP_WORKSPACES)
        .map(|w| {
            json!({
                "label": w["label"], "path": w["path"],
                "cpu_pct": (f(w, "cpu_pct") * 10.0).round() / 10.0,
                "memory": sz(u(w, "mem_bytes")), "mem_bytes": u(w, "mem_bytes"),
                "agents": w["agents"], "builds": w["builds"],
                "paused": w["paused"], "throttled": w["throttled"], "orphan": w["orphan"],
            })
        })
        .collect();
    let mut procs: Vec<Value> = snap["top"].as_array().cloned().unwrap_or_default();
    procs.sort_by(|a, b| f(b, "cpu_pct").total_cmp(&f(a, "cpu_pct")));
    let top_procs: Vec<Value> = procs
        .iter()
        .take(TOP_PROCESSES)
        .map(|p| {
            json!({
                "pid": p["pid"], "name": p["name"], "kind": p["kind"],
                "cpu_pct": (f(p, "cpu_pct") * 10.0).round() / 10.0,
                "memory": sz(u(p, "mem_bytes")), "workspace": p["group"],
            })
        })
        .collect();

    for w in &ws {
        if let Some(o) = w["orphan"].as_str() {
            recs.push(Rec {
                priority: 2,
                id: "orphan_workspace",
                title: format!(
                    "Stop leftover processes in {}",
                    w["label"].as_str().unwrap_or("?")
                ),
                reason: format!(
                    "{o}. They use {} of memory and {:.0}% CPU.",
                    sz(u(w, "mem_bytes")),
                    f(w, "cpu_pct")
                ),
                gain_bytes: None,
                gain: format!(
                    "{} of memory, {:.0}% CPU",
                    sz(u(w, "mem_bytes")),
                    f(w, "cpu_pct")
                ),
                risk: "high",
                next_call: call(
                    "plan_process_action",
                    json!({ "group": ws_key(w), "action": "quit" }),
                ),
                command: Some(format!("mulchie group {} quit", q(&ws_key(w)))),
            });
        }
        if w["paused"] == true {
            recs.push(Rec {
                priority: 4,
                id: "paused_workspace",
                title: format!(
                    "Resume or stop the paused processes in {}",
                    w["label"].as_str().unwrap_or("?")
                ),
                reason: "They are stopped (SIGSTOP) and still hold their memory.".into(),
                gain_bytes: None,
                gain: format!("{} of memory if stopped", sz(u(w, "mem_bytes"))),
                risk: "low",
                next_call: call(
                    "plan_process_action",
                    json!({ "group": ws_key(w), "action": "resume" }),
                ),
                command: Some(format!("mulchie group {} resume", q(&ws_key(w)))),
            });
        }
    }
    let pressure =
        (mem_total > 0 && mem_free_pct < mem_floor) || (cores > 0 && load1 > cores as f64 * 2.0);
    if pressure {
        if let Some(w) = ws
            .iter()
            .find(|w| u(w, "builds") > 0 && w["paused"] != true)
        {
            recs.push(Rec {
                priority: 2,
                id: "deprioritize_builds",
                title: format!("Lower the priority of builds in {}", w["label"].as_str().unwrap_or("?")),
                reason: format!("The machine is under pressure and this workspace runs {} build process(es) using {:.0}% CPU.", u(w, "builds"), f(w, "cpu_pct")),
                gain_bytes: None,
                gain: "a more responsive machine; builds take longer".into(),
                risk: "low",
                next_call: call("plan_process_action", json!({ "group": ws_key(w), "action": "lower_priority" })),
                command: Some(format!("mulchie group {} lower-priority", q(&ws_key(w)))),
            });
        }
    }

    // ------------------------------------------------------- build queue
    let slots = &state["slots"];
    let active: Vec<Value> = slots["active"]
        .as_array()
        .into_iter()
        .flatten()
        .take(TOP_ITEMS)
        .map(|l| json!({ "label": l["label"], "since_unix": l["since_unix"], "expires_unix": l["expires_unix"] }))
        .collect();
    let queue = json!({
        "capacity": slots["capacity"],
        "active": slots["active"].as_array().map_or(0, Vec::len),
        "waiting": slots["waiting"].as_array().map_or(0, Vec::len),
        "holders": active,
        "free_slots": advice["free_slots"],
        "ok_to_start_heavy_work": advice["ok_to_start_heavy_work"],
        "suggested_jobs": advice["suggested_jobs"],
        "reasons": advice["reasons"],
        "throttled_builds": state["throttled"],
    });
    let queue = if running {
        queue
    } else {
        json!({ "note": "the build queue lives in the engine, and none is running" })
    };

    // --------------------------------------------------------- reclaimable
    let scan = &state["scan"];
    let mut stale_items: Vec<&Value> = scan["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["stale"] == true && i["in_use"] != true)
        .collect();
    stale_items.sort_by_key(|i| std::cmp::Reverse(u(i, "bytes")));
    let stale_bytes: u64 = stale_items.iter().map(|i| u(i, "bytes")).sum();
    let map = &state["diskmap"];
    let mut idle: Vec<&Value> = map["worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["sweepable"] == true)
        .collect();
    idle.sort_by_key(|w| std::cmp::Reverse(u(w, "bytes")));
    let idle_bytes: u64 = idle.iter().map(|w| u(w, "bytes")).sum();
    let mut pkg: Vec<&Value> = map["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["prunable"] == true && u(c, "bytes") > 0)
        .collect();
    pkg.sort_by_key(|c| std::cmp::Reverse(u(c, "bytes")));
    let pkg_bytes: u64 = pkg.iter().map(|c| u(c, "bytes")).sum();
    let scanned = !scan.is_null() || !map.is_null();
    let reclaimable = json!({
        "scanned": scanned,
        "scanned_unix": scan["at_unix"],
        "total": sz(stale_bytes + idle_bytes + pkg_bytes),
        "total_bytes": stale_bytes + idle_bytes + pkg_bytes,
        "stale_build_caches": {
            "count": stale_items.len(), "bytes": stale_bytes, "size": sz(stale_bytes),
            "largest": stale_items.iter().take(TOP_ITEMS).map(|i| json!({ "path": i["path"], "kind": i["kind"], "size": sz(u(i, "bytes")) })).collect::<Vec<_>>(),
        },
        "idle_agent_worktrees": {
            "count": idle.len(), "bytes": idle_bytes, "size": sz(idle_bytes),
            "largest": idle.iter().take(TOP_ITEMS).map(|w| json!({
                "path": w["path"], "repo": w["repo_name"], "name": w["name"], "size": sz(u(w, "bytes")),
                "idle_days": w["idle_days"], "uncommitted_files": w["dirty_files"],
            })).collect::<Vec<_>>(),
        },
        "package_caches": {
            "count": pkg.len(), "bytes": pkg_bytes, "size": sz(pkg_bytes),
            "largest": pkg.iter().take(TOP_ITEMS).map(|c| json!({ "key": c["key"], "name": c["name"], "size": sz(u(c, "bytes")) })).collect::<Vec<_>>(),
        },
    });
    let reclaim_priority = if disk_low { 1 } else { 3 };
    if !stale_items.is_empty() {
        recs.push(Rec {
            priority: reclaim_priority,
            id: "clean_stale_caches",
            title: format!("Delete {} stale build cache(s)", stale_items.len()),
            reason: "Build output not used for the stale window, with nothing running in its project. The next build recreates it.".into(),
            gain_bytes: Some(stale_bytes),
            gain: sz(stale_bytes),
            risk: "low",
            next_call: call("plan_cache_cleanup", json!({ "stale": true })),
            command: Some("mulchie clean --stale".into()),
        });
    }
    if !idle.is_empty() {
        let dirty: u64 = idle.iter().map(|w| u(w, "dirty_files")).sum();
        recs.push(Rec {
            priority: reclaim_priority,
            id: "sweep_idle_worktrees",
            title: format!("Remove {} idle agent worktree(s)", idle.len()),
            reason: format!(
                "Agent worktrees idle past the configured window. Branches are kept{}; nothing is lost.",
                if dirty > 0 { format!(", and {dirty} uncommitted file(s) are saved as a patch first") } else { String::new() }
            ),
            gain_bytes: Some(idle_bytes),
            gain: sz(idle_bytes),
            risk: "low",
            next_call: call("plan_worktree_sweep", json!({})),
            command: Some("mulchie sweep".into()),
        });
    }
    for c in pkg.iter().filter(|c| u(c, "bytes") >= PRUNE_WORTH).take(2) {
        recs.push(Rec {
            priority: reclaim_priority + 1,
            id: "prune_package_cache",
            title: format!("Prune the {} cache", c["name"].as_str().unwrap_or("?")),
            reason: format!("{} is held by a package manager cache; its own prune command removes what it no longer needs and it downloads anything else again when needed.", sz(u(c, "bytes"))),
            gain_bytes: Some(u(c, "bytes")),
            gain: format!("up to {}", sz(u(c, "bytes"))),
            risk: "low",
            // A preview: nothing runs until prune_cache is called again with execute=true.
            next_call: call("prune_cache", json!({ "key": c["key"] })),
            command: Some(format!("mulchie prune {}", q(c["key"].as_str().unwrap_or("")))),
        });
    }
    if !scanned && running {
        recs.push(Rec {
            priority: 5,
            id: "wait_for_scan",
            title: "Wait for the first disk scan".into(),
            reason: "The engine has not finished scanning for caches and worktrees yet, so reclaimable space is unknown.".into(),
            gain_bytes: None,
            gain: "a complete picture".into(),
            risk: "none",
            next_call: call("diagnose", json!({})),
            command: Some("mulchie diagnose".into()),
        });
    }

    // ---------------------------------------------------------- findings
    let mut found: Vec<&Value> = state["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["dismissed"] != true)
        .collect();
    found.sort_by_key(|f| severity_rank(&f["severity"]));
    let serious = found
        .iter()
        .filter(|f| severity_rank(&f["severity"]) <= 1)
        .count();
    if serious > 0 {
        recs.push(Rec {
            priority: 2,
            id: "review_findings",
            title: format!("Look at {serious} finding(s) Mulchie's watch flagged"),
            reason: format!(
                "First: {} These are observations, not verdicts; each says why it was raised.",
                found[0]["title"].as_str().unwrap_or("")
            ),
            gain_bytes: None,
            gain: "knowing what these processes are".into(),
            risk: "none",
            next_call: call("list_findings", json!({ "severity": "medium" })),
            command: Some("mulchie findings --severity medium".into()),
        });
    }
    let findings = json!({
        "total": found.len(),
        "counts": state["finding_counts"],
        "note": FINDINGS_NOTE,
        "top": found.iter().take(TOP_FINDINGS).map(|f| finding_brief(f)).collect::<Vec<_>>(),
    });
    let consumers = crate::mcp_tools::consumers_brief(snap, TOP_GROUPS);
    let sessions = sessions_brief(&state["sessions"], &mut recs);
    let updates = updates_brief(&state["updates"], &state["findings"], &mut recs);

    let machine = json!({
        "cpu_pct": (f(snap, "cpu_pct") * 10.0).round() / 10.0,
        "cores": cores,
        "load": snap["load"],
        "memory_used": sz(mem_total.saturating_sub(mem_avail)),
        "memory_total": sz(mem_total),
        "memory_free_pct": mem_free_pct.round(),
        "swap_used": sz(swap),
        "disk_free": sz(disk_free),
        "disk_total": sz(disk_total),
        "disk_free_bytes": disk_free,
        "disk_hours_to_full": state["forecast"]["hours_to_full"],
        "processes": snap["proc_count"],
        "agents": snap["agent_count"],
        "builds": snap["build_count"],
    });
    let health = json!({ "status": state["health"], "why": why });
    let rest = json!({
        "consumers": consumers,
        "findings": findings,
        "sessions": sessions,
        "updates": updates,
        "machine": machine,
        "workspaces": { "total": ws_total, "top": top_ws },
        "top_processes": top_procs,
        "build_queue": queue,
        "reclaimable": reclaimable,
    });
    finish(source, state, health, problems, recs, Some(rest))
}

/// Running sessions, active breaches, hogs and what Mulchie paused, from
/// `state.sessions`, with a recommendation for each that needs a look.
fn sessions_brief(ss: &Value, recs: &mut Vec<Rec>) -> Value {
    if !ss.is_object() {
        return Value::Null;
    }
    let running: Vec<Value> = ss["list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| x["status"] == "running")
        .take(TOP_WORKSPACES)
        .map(|x| {
            json!({
                "id": x["id"], "agent": x["agent"], "workspace": x["workspace"],
                "cpu_cores": x["cpu_cores"], "memory_mb": x["memory_mb"],
                "processes": x["processes"], "runtime_secs": x["runtime_secs"],
                "next_call": { "name": "get_session", "arguments": { "id": x["id"] } },
                "cli": format!("mulchie sessions {}", q(x["id"].as_str().unwrap_or(""))),
            })
        })
        .collect();
    let breaches: Vec<Value> = ss["breaches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| !b["cleared_unix"].is_u64())
        .cloned()
        .collect();
    for b in breaches.iter().take(3) {
        let sid = b["session_id"].as_str().unwrap_or("");
        recs.push(Rec {
            priority: 1,
            id: "session_over_budget",
            title: format!(
                "{} session {sid} is over its {} budget",
                b["agent"].as_str().unwrap_or("An agent"),
                b["cap"].as_str().unwrap_or("")
            ),
            reason: crate::sessions::breach_line(b),
            gain_bytes: None,
            gain: "the session back within its budget".into(),
            risk: "none",
            next_call: call("get_session", json!({ "id": sid })),
            command: Some(format!("mulchie sessions {}", q(sid))),
        });
    }
    let hogs = ss["hogs"].as_array().cloned().unwrap_or_default();
    for h in hogs.iter().take(2) {
        let sid = h["session_id"].as_str().unwrap_or("");
        recs.push(Rec {
            priority: 2,
            id: "session_hog",
            title: format!(
                "{} session {sid} is using {:.0}% of the machine's {}",
                h["agent"].as_str().unwrap_or("An agent"),
                h["share_pct"].as_f64().unwrap_or(0.0),
                h["resource"].as_str().unwrap_or("")
            ),
            reason: "One session is taking most of a strained machine; look at what it runs before pausing or stopping it.".into(),
            gain_bytes: None,
            gain: "room for everything else".into(),
            risk: "none",
            next_call: call("get_session", json!({ "id": sid })),
            command: Some(format!("mulchie sessions {}", q(sid))),
        });
    }
    let left = ss["paused_left"].as_array().cloned().unwrap_or_default();
    if let Some(sid) = left.first().and_then(|p| p["session_id"].as_str()) {
        recs.push(Rec {
            priority: 1,
            id: "resume_paused",
            title: format!("{} process(es) are still paused by an earlier Mulchie", left.len()),
            reason: "An earlier engine paused them for a budget and this one did not resume them; they wait until resumed.".into(),
            gain_bytes: None,
            gain: "the paused work continues".into(),
            risk: "low",
            next_call: call("plan_process_action", json!({ "session": sid, "action": "resume" })),
            command: Some(format!("mulchie group {} resume", q(&format!("session:{sid}")))),
        });
    }
    json!({
        "running": ss["running"],
        "top": running,
        "breaches_active": breaches.len(),
        "breaches": breaches.iter().take(5).map(crate::sessions::breach_line).collect::<Vec<_>>(),
        "hogs": hogs,
        "paused_by_mulchie": ss["paused_by_mulchie"].as_array().map_or(0, Vec::len),
        "paused_left": left.len(),
        "limits_report_only": ss["limits_report_only"],
        "next_call": { "name": "limits_status", "arguments": {} },
    })
}

/// Available updates, from `state.updates` and the update findings.
fn updates_brief(us: &Value, findings: &Value, recs: &mut Vec<Rec>) -> Value {
    if !us.is_object() {
        return Value::Null;
    }
    let titles: Vec<Value> = findings
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["category"] == "update" && f["dismissed"] != true)
        .take(5)
        .map(|f| f["title"].clone())
        .collect();
    let n = us["available"].as_u64().unwrap_or(titles.len() as u64);
    if n > 0 {
        recs.push(Rec {
            priority: 4,
            id: "review_updates",
            title: format!("{n} update(s) available"),
            reason: titles
                .first()
                .and_then(Value::as_str)
                .map(|t| format!("First: {t}."))
                .unwrap_or_else(|| {
                    "Mulchie's update check found newer versions or advisories.".into()
                }),
            gain_bytes: None,
            gain: "current tools and fixed advisories".into(),
            risk: "none",
            next_call: call("list_updates", json!({})),
            command: Some("mulchie updates".into()),
        });
    }
    json!({
        "enabled": us["enabled"],
        "available": n,
        "titles": titles,
        "checked_unix": us["checked_unix"],
        "next_call": { "name": "list_updates", "arguments": {} },
    })
}

fn finish(
    source: &str,
    state: &Value,
    health: Value,
    problems: Vec<String>,
    mut recs: Vec<Rec>,
    rest: Option<Value>,
) -> Value {
    recs.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(b.gain_bytes.unwrap_or(0).cmp(&a.gain_bytes.unwrap_or(0)))
    });
    let status = health["status"].as_str().unwrap_or("unknown").to_string();
    let summary = match (&rest, recs.first()) {
        (None, _) => "Mulchie cannot see this machine: no engine is running.".to_string(),
        (Some(r), first) => {
            let reclaim = r["reclaimable"]["total_bytes"].as_u64().unwrap_or(0);
            let mut s = format!(
                "Health {status}: {}.",
                health["why"]
                    .as_array()
                    .map(|w| w
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; "))
                    .unwrap_or_default()
            );
            if reclaim > 0 {
                s.push_str(&format!(" About {} can be reclaimed safely.", sz(reclaim)));
            }
            if let Some(f) = first {
                s.push_str(&format!(" First: {}.", f.title));
            }
            s
        }
    };
    let mut out = json!({
        "summary": summary,
        // The core's own sentences about CPU and memory come first.
        "cpu": rest.as_ref().map(|r| r["consumers"]["cpu"].clone()),
        "memory": rest.as_ref().map(|r| r["consumers"]["memory"].clone()),
        "source": source,
        "version": state["version"],
        "health": health,
        "engine_problems": problems,
        "recommendations": recs.iter().map(Rec::to_json).collect::<Vec<_>>(),
        "safety": "Every next_call here only looks or plans; dismiss_finding only hides a finding. Caches, worktrees and processes are changed only by execute_plan with items from a plan the user saw, and each item is re-checked first (processes by start time); worktree removals save uncommitted work as a patch and keep branches. prune_cache only previews until called again with execute=true, after the user agrees.",
    });
    if let (Some(r), Some(o)) = (rest, out.as_object_mut()) {
        for (k, v) in r.as_object().cloned().unwrap_or_default() {
            if k != "consumers" {
                o.insert(k, v);
            }
        }
        o.insert("memory_note".into(), r["consumers"]["memory_note"].clone());
    } else if let Some(o) = out.as_object_mut() {
        o.remove("cpu");
        o.remove("memory");
    }
    out
}

/// The picture as text for people.
pub fn render(d: &Value) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", d["summary"].as_str().unwrap_or("")));
    if d["source"] == "local" {
        s.push_str("(No engine running: this is a one-off local look.)\n");
    }
    for (label, k) in [("CPU", "cpu"), ("Memory", "memory")] {
        let c = &d[k];
        if c.is_null() {
            continue;
        }
        let head = c["headline"].as_str().unwrap_or("");
        if !head.is_empty() {
            s.push_str(&format!("\n{label}: {head}"));
            if let Some(det) = c["detail"].as_str().filter(|x| !x.is_empty()) {
                s.push_str(&format!(" {det}"));
            }
            s.push('\n');
        }
        let tops: Vec<String> = c["top"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                let amount = if k == "cpu" {
                    format!("{:.0}%", g["cpu_pct"].as_f64().unwrap_or(0.0))
                } else {
                    g["memory"].as_str().unwrap_or("").to_string()
                };
                format!("{} {amount}", g["name"].as_str().unwrap_or("?"))
            })
            .collect();
        if !tops.is_empty() {
            s.push_str(&format!("  most: {}\n", tops.join(", ")));
        }
    }
    let m = &d["machine"];
    if !m.is_null() {
        s.push_str(&format!(
            "\nMachine: CPU {}% on {} cores, load {}, memory {} of {} used ({}% free), swap {}, disk {} free of {}\n",
            m["cpu_pct"], m["cores"],
            m["load"][0].as_f64().map(|l| format!("{l:.1}")).unwrap_or_default(),
            m["memory_used"].as_str().unwrap_or(""), m["memory_total"].as_str().unwrap_or(""),
            m["memory_free_pct"], m["swap_used"].as_str().unwrap_or(""),
            m["disk_free"].as_str().unwrap_or(""), m["disk_total"].as_str().unwrap_or(""),
        ));
        if let Some(h) = m["disk_hours_to_full"].as_f64() {
            s.push_str(&format!(
                "Disk fills in about {:.0} hours at the current rate.\n",
                h
            ));
        }
    }
    let problems: Vec<&str> = d["engine_problems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !problems.is_empty() {
        s.push_str("\nEngine:\n");
        for p in problems {
            s.push_str(&format!("  ! {p}\n"));
        }
    }
    let top = d["workspaces"]["top"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !top.is_empty() {
        s.push_str(&format!(
            "\nBusiest workspaces ({} in all):\n",
            d["workspaces"]["total"]
        ));
        for w in &top {
            s.push_str(&format!(
                "  {:<44} {:>6}% cpu {:>9}  {} agent · {} build{}\n",
                crate::commands::truncate(w["label"].as_str().unwrap_or(""), 44),
                w["cpu_pct"],
                w["memory"].as_str().unwrap_or(""),
                w["agents"],
                w["builds"],
                if w["orphan"].is_string() {
                    "  (orphaned)"
                } else if w["paused"] == true {
                    "  (paused)"
                } else {
                    ""
                }
            ));
        }
    }
    let q = &d["build_queue"];
    if let Some(n) = q["note"].as_str() {
        s.push_str(&format!("\nBuild queue: {n}\n"));
    } else if !q.is_null() {
        s.push_str(&format!(
            "\nBuild queue: {} of {} slot(s) in use, {} waiting; {}\n",
            q["active"],
            q["capacity"],
            q["waiting"],
            if q["ok_to_start_heavy_work"] == true {
                "ok to start heavy work".to_string()
            } else {
                format!(
                    "wait: {}",
                    q["reasons"]
                        .as_array()
                        .map(|r| r
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; "))
                        .unwrap_or_default()
                )
            }
        ));
    }
    let r = &d["reclaimable"];
    if !r.is_null() {
        if r["scanned"] == true {
            s.push_str(&format!(
                "\nReclaimable: {} in all: {} stale build cache(s) {}, {} idle agent worktree(s) {}, package caches {}\n",
                r["total"].as_str().unwrap_or(""),
                r["stale_build_caches"]["count"], r["stale_build_caches"]["size"].as_str().unwrap_or(""),
                r["idle_agent_worktrees"]["count"], r["idle_agent_worktrees"]["size"].as_str().unwrap_or(""),
                r["package_caches"]["size"].as_str().unwrap_or(""),
            ));
        } else {
            s.push_str("\nReclaimable: not scanned yet.\n");
        }
    }
    let ss = &d["sessions"];
    if ss.is_object() {
        let top = ss["top"].as_array().cloned().unwrap_or_default();
        s.push_str(&format!(
            "\nAgent sessions: {} running",
            ss["running"].as_u64().unwrap_or(0)
        ));
        let br = ss["breaches_active"].as_u64().unwrap_or(0);
        if br > 0 {
            s.push_str(&format!(", {br} over budget"));
        }
        let paused = ss["paused_by_mulchie"].as_u64().unwrap_or(0);
        if paused > 0 {
            s.push_str(&format!(", {paused} process(es) paused by Mulchie"));
        }
        s.push('\n');
        for x in &top {
            s.push_str(&format!(
                "  {:<22} {:<10} {:>5.1} cores {:>7.0} MB  {}\n",
                x["id"].as_str().unwrap_or(""),
                x["agent"].as_str().unwrap_or(""),
                x["cpu_cores"].as_f64().unwrap_or(0.0),
                x["memory_mb"].as_f64().unwrap_or(0.0),
                x["workspace"].as_str().unwrap_or("")
            ));
        }
        for b in ss["breaches"].as_array().into_iter().flatten() {
            s.push_str(&format!("  ! {}\n", b.as_str().unwrap_or("")));
        }
        for h in ss["hogs"].as_array().into_iter().flatten() {
            s.push_str(&format!(
                "  hog: {} {} {:.0}% of {}\n",
                h["agent"].as_str().unwrap_or(""),
                h["session_id"].as_str().unwrap_or(""),
                h["share_pct"].as_f64().unwrap_or(0.0),
                h["resource"].as_str().unwrap_or("")
            ));
        }
    }
    let up = &d["updates"];
    if up["available"].as_u64().unwrap_or(0) > 0 {
        s.push_str(&format!(
            "\nUpdates: {} available (`mulchie updates`)\n",
            up["available"]
        ));
    }
    let fs = d["findings"]["top"].as_array().cloned().unwrap_or_default();
    if !fs.is_empty() {
        s.push_str(&format!(
            "\nWatch noticed ({}; observations, not verdicts):\n",
            d["findings"]["total"]
        ));
        for f in &fs {
            s.push_str(&format!(
                "  [{}] {}\n       {}\n",
                f["severity"].as_str().unwrap_or(""),
                f["title"].as_str().unwrap_or(""),
                f["why"].as_str().unwrap_or("")
            ));
            let cli: Vec<&str> = f["cli"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !cli.is_empty() {
                s.push_str(&format!("       next: {}\n", cli.join("  |  ")));
            }
        }
    }
    let recs = d["recommendations"].as_array().cloned().unwrap_or_default();
    if recs.is_empty() {
        s.push_str("\nNothing to do.\n");
    } else {
        s.push_str("\nRecommendations:\n");
        for (i, r) in recs.iter().enumerate() {
            s.push_str(&format!(
                "  {}. {}  [risk {}, gain {}]\n     {}\n",
                i + 1,
                r["title"].as_str().unwrap_or(""),
                r["risk"].as_str().unwrap_or(""),
                r["expected_gain"].as_str().unwrap_or(""),
                r["reason"].as_str().unwrap_or("")
            ));
            if let Some(c) = r["cli"].as_str() {
                s.push_str(&format!("     run: {c}\n"));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Value {
        json!({
            "version": "0.3.0",
            "health": "strained",
            "config": { "low_disk_gb": 25, "throttle_mem_free_pct": 8.0, "throttle_cpu_pct": 92.0 },
            "config_error": null,
            "sample_age_secs": 2,
            "snapshot": {
                "cpu_pct": 50.0, "cpu_count": 8, "load": [3.0, 2.0, 1.0],
                "mem_total": 16_000_000_000u64, "mem_available": 8_000_000_000u64,
                "swap_used": 0, "disk_total": 500_000_000_000u64, "disk_free": 10_000_000_000u64,
                "proc_count": 300, "agent_count": 2, "build_count": 1,
                "workspaces": (0..10).map(|i| json!({ "label": format!("repo › w{i}"), "path": format!("/r/w{i}"),
                    "cpu_pct": i as f64, "mem_bytes": 1_000_000, "agents": 1, "builds": 0, "paused": false,
                    "orphan": if i == 3 { json!("its worktree was deleted") } else { json!(null) } })).collect::<Vec<_>>(),
                "top": (0..20).map(|i| json!({ "pid": 100 + i, "name": "x", "cpu_pct": i as f64, "mem_bytes": 1 })).collect::<Vec<_>>(),
            },
            "slots": { "capacity": 2, "active": [{ "label": "a" }], "waiting": [] },
            "scan": { "at_unix": 1, "items": [
                { "path": "/r/a/target", "kind": "cargo_target", "bytes": 3_000_000_000u64, "stale": true, "in_use": false },
                { "path": "/r/b/target", "kind": "cargo_target", "bytes": 9_000_000_000u64, "stale": true, "in_use": true },
                { "path": "/r/c/target", "kind": "cargo_target", "bytes": 1_000_000_000u64, "stale": false, "in_use": false }
            ]},
            "diskmap": {
                "worktrees": [
                    { "path": "/r/.claude/worktrees/agent-a", "repo_name": "r", "name": "agent-a", "bytes": 2_000_000_000u64, "idle_days": 9, "dirty_files": 2, "sweepable": true },
                    { "path": "/r/mine", "repo_name": "r", "name": "mine", "bytes": 5_000_000_000u64, "sweepable": false }
                ],
                "categories": [ { "key": "npm", "name": "npm", "bytes": 4_000_000_000u64, "prunable": true } ]
            },
            "forecast": { "hours_to_full": 12.0 },
            "loops": { "sample": { "restarts": 0 }, "scan": { "restarts": 0 } },
        })
    }

    #[test]
    fn prioritised_and_capped() {
        let d = build(
            &state(),
            &json!({ "ok_to_start_heavy_work": false, "reasons": ["x"] }),
            Origin::Engine,
        );
        assert_eq!(d["source"], "engine");
        let why = d["health"]["why"].as_array().unwrap();
        assert!(why[0].as_str().unwrap().contains("disk low"), "{why:?}");
        assert_eq!(d["workspaces"]["total"], 10);
        assert_eq!(
            d["workspaces"]["top"].as_array().unwrap().len(),
            TOP_WORKSPACES
        );
        assert_eq!(d["top_processes"].as_array().unwrap().len(), TOP_PROCESSES);
        // In-use and recent caches are not reclaimable.
        assert_eq!(d["reclaimable"]["stale_build_caches"]["count"], 1);
        assert_eq!(d["reclaimable"]["idle_agent_worktrees"]["count"], 1);
        assert_eq!(d["reclaimable"]["total_bytes"], 9_000_000_000u64);
        let recs = d["recommendations"].as_array().unwrap();
        // Disk is low, so reclaiming comes first, largest first.
        assert_eq!(recs[0]["id"], "clean_stale_caches");
        assert_eq!(
            recs[0]["next_call"],
            json!({ "name": "plan_cache_cleanup", "arguments": { "stale": true } })
        );
        assert_eq!(recs[1]["id"], "sweep_idle_worktrees");
        assert_eq!(recs[1]["next_call"]["name"], "plan_worktree_sweep");
        let orphan = recs.iter().find(|r| r["id"] == "orphan_workspace").unwrap();
        assert_eq!(orphan["risk"], "high");
        // Workspace actions are process plans on the workspace group.
        assert_eq!(orphan["next_call"]["name"], "plan_process_action");
        assert!(orphan["next_call"]["arguments"]["group"]
            .as_str()
            .unwrap()
            .starts_with("workspace:/"));
        assert!(!d.to_string().contains("workspace_action"));
        assert!(recs.iter().any(
            |r| r["id"] == "prune_package_cache" && r["next_call"]["arguments"]["key"] == "npm"
        ));
        for r in recs {
            // A next call only ever looks or previews.
            let args = &r["next_call"]["arguments"];
            assert!(args.get("execute").is_none(), "{r}");
            assert_ne!(r["next_call"]["name"], "execute_plan", "{r}");
            assert!(r["reason"].as_str().unwrap().len() > 10);
            assert!(["none", "low", "medium", "high"].contains(&r["risk"].as_str().unwrap()));
        }
        // Small enough for a context window.
        assert!(d.to_string().len() < 12_000, "{}", d.to_string().len());
        assert!(render(&d).contains("Recommendations:"));
    }

    #[test]
    fn sessions_breaches_and_updates_name_the_next_call() {
        let mut st = state();
        st["sessions"] = json!({ "enabled": true, "running": 1, "ended": 0,
            "list": [{ "id": "s-500-100", "agent": "claude", "status": "running", "workspace": "/code/a",
                "cpu_cores": 2.5, "memory_mb": 900.0, "processes": 7, "runtime_secs": 60 }],
            "breaches": [{ "id": "b", "session_id": "s-500-100", "agent": "claude", "cap": "memory_mb",
                "max": 500.0, "value": 900.0, "action": "notify", "action_state": "notify_only" }],
            "breaches_active": 1, "hogs": [{ "session_id": "s-500-100", "agent": "claude", "resource": "cpu", "share_pct": 80.0, "since_unix": 1 }],
            "paused_by_mulchie": [], "paused_left": [{ "pid": 9, "session_id": "s-500-100", "since_unix": 1 }],
            "limits_report_only": true });
        st["updates"] = json!({ "enabled": true, "available": 1 });
        st["findings"] = json!([{ "id": "u:claude", "kind": "agent_outdated", "category": "update",
            "severity": "low", "title": "claude 2.0 is out", "why": "", "evidence": {}, "suggested": [] }]);
        let d = build(&st, &json!({}), Origin::Engine);
        assert_eq!(d["sessions"]["top"][0]["next_call"]["name"], "get_session");
        assert_eq!(d["sessions"]["breaches_active"], 1);
        assert_eq!(d["updates"]["available"], 1);
        assert_eq!(d["findings"]["top"][0]["category"], "update");
        let recs = d["recommendations"].as_array().unwrap();
        let by = |id: &str| {
            recs.iter()
                .find(|r| r["id"] == id)
                .unwrap_or_else(|| panic!("no {id}: {recs:?}"))
                .clone()
        };
        assert_eq!(
            by("session_over_budget")["next_call"]["name"],
            "get_session"
        );
        assert_eq!(
            by("session_hog")["next_call"]["arguments"]["id"],
            "s-500-100"
        );
        assert_eq!(
            by("resume_paused")["next_call"]["arguments"]["session"],
            "s-500-100"
        );
        assert_eq!(
            by("resume_paused")["cli"],
            "mulchie group session:s-500-100 resume"
        );
        assert_eq!(by("review_updates")["next_call"]["name"], "list_updates");
        let t = render(&d);
        assert!(
            t.contains("Agent sessions: 1 running, 1 over budget"),
            "{t}"
        );
        assert!(t.contains("Updates: 1 available"), "{t}");
    }

    #[test]
    fn suggested_commands_survive_quotes_in_paths_and_ids() {
        let mut st = state();
        st["snapshot"]["workspaces"][3]["path"] = json!("/r/it's here");
        let d = build(&st, &json!({}), Origin::Engine);
        let cmds: Vec<String> = d["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["cli"].as_str().map(String::from))
            .collect();
        let group = cmds
            .iter()
            .find(|c| c.starts_with("mulchie group"))
            .unwrap_or_else(|| panic!("no group command in {cmds:?}"));
        let words = crate::hooks_install::sh_words(group);
        assert_eq!(words[2], "workspace:/r/it's here", "{group}");
        let f = json!({ "id": "odd:it's:1", "kind": "x", "severity": "low", "evidence": { "pid": 7 },
            "suggested": [{ "label": "Stop", "request": { "action": "signal", "pid": 7, "signal": "term" } }] });
        let b = finding_brief(&f);
        let dismiss = b["cli"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find(|c| c.contains("dismiss"))
            .unwrap()
            .to_string();
        assert_eq!(
            crate::hooks_install::sh_words(&dismiss)[3],
            "odd:it's:1",
            "{dismiss}"
        );
    }

    #[test]
    fn engine_problems_come_first() {
        let mut st = state();
        st["health"] = json!("stale");
        st["sample_age_secs"] = json!(99);
        st["config_error"] = json!("config has invalid values: x");
        let d = build(&st, &json!({}), Origin::Engine);
        let recs = d["recommendations"].as_array().unwrap();
        assert!(["fix_config", "restart_engine"].contains(&recs[0]["id"].as_str().unwrap()));
        assert_eq!(d["engine_problems"].as_array().unwrap().len(), 2);

        let d = build(
            &Value::Null,
            &Value::Null,
            Origin::Unavailable("no engine is running"),
        );
        assert_eq!(d["source"], "none");
        assert_eq!(d["recommendations"][0]["id"], "start_engine");
        assert!(d["machine"].is_null());
        assert!(render(&d).contains("cannot see"));
    }

    #[test]
    fn a_quiet_machine() {
        let mut st = state();
        st["snapshot"]["disk_free"] = json!(400_000_000_000u64);
        st["snapshot"]["workspaces"] = json!([]);
        st["scan"] = json!({ "items": [] });
        st["diskmap"] = json!({ "worktrees": [], "categories": [] });
        let d = build(
            &st,
            &json!({ "ok_to_start_heavy_work": true }),
            Origin::Engine,
        );
        assert_eq!(d["recommendations"], json!([]));
        assert!(render(&d).contains("Nothing to do"));
    }

    #[test]
    fn consumers_and_findings_lead_and_are_ordered() {
        let mut st = state();
        st["snapshot"]["cpu_summary"] =
            json!({ "headline": "CPU is busy.", "detail": "Slack uses two cores." });
        st["snapshot"]["mem_summary"] = json!({ "headline": "Memory is tight.", "detail": "" });
        st["snapshot"]["consumers"] = json!({
            "by_cpu": [{ "key": "app:/Applications/Slack.app", "kind": "app", "name": "Slack", "cpu_pct": 200.0, "mem_bytes": 1u64 << 30, "procs": 9 }],
            "by_mem": [], "memory_note": "Shared memory is counted once."
        });
        let f = |id: &str, sev: &str, dismissed: bool| {
            json!({ "id": id, "kind": "runaway_cpu", "severity": sev,
            "title": format!("{id} title"), "why": "Because a heuristic said so.", "dismissed": dismissed,
            "evidence": { "pid": 42, "cmd": ["node", "--token", "[redacted]"] },
            "suggested": [{ "label": "Stop it", "request": { "action": "signal", "pid": 42, "signal": "term" }, "reversible": false }] })
        };
        st["findings"] = json!([
            f("a", "low", false),
            f("b", "high", false),
            f("c", "medium", true),
            f("d", "medium", false)
        ]);
        let d = build(&st, &json!({}), Origin::Engine);
        assert_eq!(d["cpu"]["headline"], "CPU is busy.");
        assert_eq!(d["cpu"]["top"][0]["name"], "Slack");
        assert_eq!(d["memory"]["headline"], "Memory is tight.");
        let top = d["findings"]["top"].as_array().unwrap();
        let ids: Vec<&str> = top.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["b", "d", "a"],
            "most severe first, dismissed left out"
        );
        assert_eq!(
            top[0]["next_calls"][0],
            json!({ "label": "Stop it", "name": "plan_process_action", "arguments": { "pid": 42, "signal": "term" } })
        );
        assert!(d["findings"]["note"]
            .as_str()
            .unwrap()
            .contains("not verdicts"));
        assert!(d["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "review_findings"));
        let text = render(&d);
        assert!(
            text.contains("CPU: CPU is busy. Slack uses two cores."),
            "{text}"
        );
        assert!(
            text.contains("observations, not verdicts") && text.contains("[high] b title"),
            "{text}"
        );
        assert!(!text.to_lowercase().contains("malware detected"));
        // Keys appear in the order a reader needs them.
        let j = d.to_string();
        assert!(j.find("\"cpu\"").unwrap() < j.find("\"recommendations\"").unwrap());
        assert!(j.len() < 12_000, "{}", j.len());
    }

    #[test]
    fn a_tree_plan_keeps_tree_in_the_call_and_the_command() {
        let f = json!({ "id": "runaway_chain:42:7", "kind": "runaway_chain", "severity": "high", "title": "t", "why": "", "evidence": {},
            "suggested": [
                { "label": "Plan killing the chain", "api_request": { "method": "POST", "route": "/v1/process/plan", "body": { "pid": 42, "signal": "kill", "tree": true } } } ] });
        let b = finding_brief(&f);
        assert_eq!(
            b["next_calls"][0]["arguments"],
            json!({ "pid": 42, "signal": "kill", "tree": true })
        );
        assert_eq!(b["cli"][0], "mulchie signal 42 kill --tree");
    }

    #[test]
    fn a_suggestions_api_request_becomes_the_next_call() {
        let f = json!({ "id": "limit_exceeded:b1", "kind": "limit_exceeded", "severity": "high", "title": "t", "why": "", "evidence": {},
            "suggested": [
                { "label": "Plan", "api": "POST /v1/process/plan", "api_request": { "method": "POST", "route": "/v1/process/plan", "body": { "session": "s-1-2", "action": "pause" } } },
                { "label": "Show", "api_request": { "method": "GET", "route": "/v1/sessions/s-1-2" } },
                { "label": "Stopped", "api_request": { "method": "GET", "route": "/v1/state" } },
                { "label": "Odd", "api_request": { "method": "GET", "route": "/v1/sessions/../config" } },
                { "label": "Hide", "api_request": { "method": "POST", "route": "/v1/findings/dismiss", "body": { "id": "limit_exceeded:b1" } } } ] });
        let b = finding_brief(&f);
        let next = b["next_calls"].as_array().unwrap();
        assert_eq!(next[0]["name"], "plan_process_action");
        assert_eq!(
            next[0]["arguments"],
            json!({ "session": "s-1-2", "action": "pause" })
        );
        assert_eq!(next[1]["name"], "get_session");
        assert_eq!(next[1]["arguments"]["id"], "s-1-2");
        assert_eq!(next[2]["name"], "get_status");
        // A route that is not a session id maps to nothing; the dismissal
        // is listed once.
        assert_eq!(next.len(), 4, "{b}");
        assert_eq!(next[3]["name"], "dismiss_finding");
        let cli: Vec<&str> = b["cli"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            cli.contains(&"mulchie group session:s-1-2 pause"),
            "{cli:?}"
        );
        assert!(cli.contains(&"mulchie sessions s-1-2"), "{cli:?}");
        assert_eq!(
            cli.iter()
                .filter(|c| c.starts_with("mulchie findings dismiss"))
                .count(),
            1
        );
        // Never a call that acts.
        for n in next {
            assert_ne!(n["name"], "execute_plan");
        }
    }
}
