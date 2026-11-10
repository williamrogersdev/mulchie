//! The MCP tools. Read tools and plan tools change nothing. Deleting goes
//! through two calls: a plan tool returns a `plan_id` and its items, and
//! `execute_plan` carries out only items named from that stored plan (the
//! engine's safety gate re-checks each one). An agent cannot delete anything
//! it was not first shown in a plan.

use crate::diagnose::{self, Origin};
use crate::mcp::Api;
use mulchie_core::api::ApiError;
use mulchie_core::{human_bytes, Paths};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a plan can be executed after it was made.
pub const PLAN_TTL: Duration = Duration::from_secs(30 * 60);
/// Plans kept per session; the oldest goes first.
const MAX_PLANS: usize = 32;

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    props: Value,
    required: &'static [&'static str],
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

impl Tool {
    pub fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": self.props,
            "required": self.required,
            "additionalProperties": false,
        })
    }

    pub fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.schema(),
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            },
        })
    }
}

fn read(name: &'static str, title: &'static str, description: &'static str, props: Value) -> Tool {
    Tool {
        name,
        title,
        description,
        props,
        required: &[],
        read_only: true,
        destructive: false,
        idempotent: true,
    }
}

pub fn tool_list() -> Vec<Tool> {
    let v = vec![
        read(
            "diagnose",
            "Diagnose this machine",
            "Start here. One compact, prioritised picture of the machine: health and why, the busiest workspaces and processes, the build queue, reclaimable space (stale build caches, idle agent worktrees, package caches) with sizes, engine problems, and recommendations. Each recommendation has a plain reason, the expected gain, a risk level, and the exact next tool call. Changes nothing.",
            json!({}),
        ),
        read(
            "get_status",
            "Machine status",
            "Raw machine figures: disk, memory, CPU, load, swap, agent and build counts, build slots, disk-fill forecast, recent actions. diagnose is usually the better first call.",
            json!({}),
        ),
        read(
            "can_i_build",
            "Can I start heavy work?",
            "Whether now is a good moment to start heavy work (builds, test suites), how many build slots are free, and the suggested -j job count.",
            json!({}),
        ),
        read(
            "top_processes",
            "Top processes",
            "Processes ranked by CPU, memory or disk I/O, with pid, kind (agent/build/runtime), workspace, niceness, state and command line.",
            json!({
                "sort": { "type": "string", "enum": ["cpu", "mem", "io"], "description": "Rank by this (default cpu)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "How many (default 20)" },
                "workspace": { "type": "string", "description": "Only processes whose group contains this text" }
            }),
        ),
        read(
            "list_workspaces",
            "Workspaces",
            "Every repo or worktree with running processes: CPU, memory, I/O, agents, builds, paused/throttled state and orphan warnings.",
            json!({ "include_processes": { "type": "boolean", "description": "Include each workspace's processes" } }),
        ),
        read(
            "get_history",
            "Recent history",
            "Time series sampled every 3 s for up to the last 30 minutes (at most 120 points): cpu, load, memory, swap, disk free, I/O and top workspaces.",
            json!({ "minutes": { "type": "integer", "minimum": 1, "maximum": 30, "description": "How far back (default 10)" } }),
        ),
        read(
            "disk_breakdown",
            "Disk breakdown",
            "Where disk space went: toolchain and package caches, model stores, and every git worktree with size, idle days, uncommitted files and a removal verdict.",
            json!({}),
        ),
        read(
            "reclaim_candidates",
            "Build caches",
            "Build caches (Rust target, .next, node_modules if enabled, Xcode DerivedData if enabled) with size, last use, and in-use and stale flags, from the engine's last scan.",
            json!({}),
        ),
        read(
            "plan_cache_cleanup",
            "Plan deleting build caches",
            "Plan deleting recognised build caches: stale=true for every stale cache, or paths from reclaim_candidates. Changes nothing. Returns a plan_id, the items that are ready (with sizes) and the ones that are not (with reasons). Show the plan to the user; to delete what they approve, call execute_plan with the plan_id and those items' targets.",
            json!({
                "stale": { "type": "boolean", "description": "Every stale cache" },
                "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Absolute cache directories, instead of stale" }
            }),
        ),
        Tool {
            required: &["paths"],
            ..read(
                "plan_worktree_removal",
                "Plan removing git worktrees",
                "Plan removing specific git worktrees. Changes nothing. Each ready item records the HEAD read now; removal is skipped if HEAD moves before execution. Worktrees with uncommitted changes are not ready unless allow_dirty=true, in which case a patch is saved first. Returns a plan_id; show the plan to the user, then call execute_plan with the approved targets.",
                json!({
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Absolute worktree paths" },
                    "allow_dirty": { "type": "boolean", "description": "Allow removing worktrees with uncommitted changes (saved as a patch first)" }
                }),
            )
        },
        read(
            "plan_worktree_sweep",
            "Plan sweeping idle agent worktrees",
            "Plan removing every agent worktree idle past the configured window. Changes nothing. Uncommitted work would be saved as a patch, ignored non-cache files archived, a detached HEAD kept on a backup/<name> branch, and branches are never deleted. Returns a plan_id; show the plan to the user, then call execute_plan with the approved targets.",
            json!({}),
        ),
        Tool {
            name: "execute_plan",
            title: "Execute approved plan items",
            description: "Carry out items from a plan made by plan_cache_cleanup, plan_worktree_removal or plan_worktree_sweep in this session. items must be targets of ready items in that plan; anything else is refused. Only call this after the user has seen the plan and approved these items. Each item is re-checked just before it runs and skipped if anything changed. The result lists what was done, what was skipped and why, every backup with its path, and how to restore. A plan can be executed once and expires after 30 minutes.",
            props: json!({
                "plan_id": { "type": "string", "description": "From the plan tool's result" },
                "items": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Targets of the approved ready items" }
            }),
            required: &["plan_id", "items"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "prune_cache",
            title: "Prune a package manager cache",
            description: "Run a package manager's own prune command for one cache key from disk_breakdown or diagnose (npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators). The tool re-downloads anything it needs later. Refused while that tool is running. Ask the user first.",
            props: json!({ "key": { "type": "string", "description": "The cache key" } }),
            required: &["key"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "workspace_action",
            title: "Act on a workspace's processes",
            description: "Act on all of a workspace's processes owned by the current user: pause (SIGSTOP), resume (SIGCONT), deprioritize (renice), stop_builds (SIGTERM to builds only) or kill_all (SIGTERM to everything). Without execute=true it only lists the processes it would affect; show that to the user before executing.",
            props: json!({
                "path": { "type": "string", "description": "The workspace path from list_workspaces or diagnose" },
                "action": { "type": "string", "enum": ["pause", "resume", "deprioritize", "stop_builds", "kill_all"] },
                "execute": { "type": "boolean", "description": "Act, instead of only showing what would be affected" }
            }),
            required: &["path", "action"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "process_action",
            title: "Signal or renice a process",
            description: "Signal (term, kill, stop, cont) or renice one process owned by the current user. A pid reused by another process since the last sample is refused. Without execute=true it only shows the process it would act on.",
            props: json!({
                "pid": { "type": "integer", "minimum": 2 },
                "action": { "type": "string", "enum": ["term", "kill", "stop", "cont", "renice"] },
                "nice": { "type": "integer", "minimum": 0, "maximum": 20, "description": "For renice (default 10)" },
                "execute": { "type": "boolean", "description": "Act, instead of only showing the process" }
            }),
            required: &["pid", "action"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "acquire_build_slot",
            title: "Join the build queue",
            description: "Join the machine-wide build queue before heavy work. Call it, then keep calling it with the returned ticket (every few seconds) until granted is true; then run with the returned job count. A granted slot lasts ttl_secs (default 900): call again with the same ticket before ttl_secs passes to renew it during long work, or the slot expires and goes to someone else. Always call release_build_slot as soon as the work ends.",
            props: json!({
                "label": { "type": "string", "description": "What the slot is for, shown to the user" },
                "ticket": { "type": "integer", "minimum": 1, "description": "The ticket from an earlier call: poll or renew" },
                "ttl_secs": { "type": "integer", "minimum": 1, "maximum": 86400, "description": "How long the slot lasts without renewal (default 900)" }
            }),
            required: &[],
            read_only: false,
            destructive: false,
            idempotent: false,
        },
        Tool {
            name: "release_build_slot",
            title: "Leave the build queue",
            description: "Release a build slot or leave the queue. Call it as soon as the heavy work ends.",
            props: json!({ "ticket": { "type": "integer", "minimum": 1 } }),
            required: &["ticket"],
            read_only: false,
            destructive: false,
            idempotent: true,
        },
    ];
    v
}

/// Tools offered in read-only mode: everything that is not destructive.
pub fn allowed(tool: &Tool, read_only: bool) -> bool {
    !(read_only && tool.destructive)
}

// ------------------------------------------------------------ validation

/// Check arguments against the small schema subset the tools use.
pub fn validate(schema: &Value, args: &Map<String, Value>) -> Result<(), String> {
    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (k, v) in args {
        let Some(p) = props.get(k) else {
            let known: Vec<&str> = props.keys().map(String::as_str).collect();
            return Err(if known.is_empty() {
                format!("unknown argument `{k}`: this tool takes no arguments")
            } else {
                format!(
                    "unknown argument `{k}`; expected one of: {}",
                    known.join(", ")
                )
            });
        };
        check_value(k, p, v)?;
    }
    for r in schema["required"].as_array().into_iter().flatten() {
        let r = r.as_str().unwrap_or("");
        if !args.contains_key(r) {
            return Err(format!("missing required argument `{r}`"));
        }
    }
    Ok(())
}

fn check_value(k: &str, p: &Value, v: &Value) -> Result<(), String> {
    match p["type"].as_str().unwrap_or("") {
        "string" => {
            let s = v.as_str().ok_or(format!("`{k}` must be a string"))?;
            if let Some(e) = p["enum"].as_array() {
                if !e.iter().any(|x| x == s) {
                    let opts: Vec<&str> = e.iter().filter_map(Value::as_str).collect();
                    return Err(format!("`{k}` must be one of: {}", opts.join(", ")));
                }
            }
        }
        "boolean" => {
            v.as_bool().ok_or(format!("`{k}` must be true or false"))?;
        }
        "integer" => {
            let n = v.as_i64().ok_or(format!("`{k}` must be a whole number"))?;
            if let Some(m) = p["minimum"].as_i64() {
                if n < m {
                    return Err(format!("`{k}` must be at least {m}"));
                }
            }
            if let Some(m) = p["maximum"].as_i64() {
                if n > m {
                    return Err(format!("`{k}` must be at most {m}"));
                }
            }
        }
        "array" => {
            let a = v.as_array().ok_or(format!("`{k}` must be an array"))?;
            if let Some(m) = p["minItems"].as_u64() {
                if (a.len() as u64) < m {
                    return Err(format!("`{k}` needs at least {m} item(s)"));
                }
            }
            for (i, x) in a.iter().enumerate() {
                check_value(&format!("{k}[{i}]"), &p["items"], x)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ------------------------------------------------------------------ plans

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Caches,
    Worktrees,
    Sweep,
}

impl PlanKind {
    fn route(self) -> &'static str {
        match self {
            PlanKind::Caches => "/v1/clean",
            PlanKind::Worktrees => "/v1/worktree/remove",
            PlanKind::Sweep => "/v1/sweep",
        }
    }

    fn name(self) -> &'static str {
        match self {
            PlanKind::Caches => "delete build caches",
            PlanKind::Worktrees => "remove worktrees",
            PlanKind::Sweep => "sweep idle agent worktrees",
        }
    }
}

struct StoredPlan {
    kind: PlanKind,
    created: Instant,
    items: Vec<Value>,
}

/// Plans shown in this session, by id. Execution needs one of these.
#[derive(Default)]
pub struct Plans {
    map: HashMap<String, StoredPlan>,
    order: Vec<String>,
}

impl Plans {
    fn insert(&mut self, kind: PlanKind, items: Vec<Value>) -> String {
        let id = mulchie_core::paths::random_hex(8).unwrap_or_else(|_| {
            format!(
                "{:x}",
                Instant::now().elapsed().as_nanos() ^ self.order.len() as u128
            )
        });
        self.map.insert(
            id.clone(),
            StoredPlan {
                kind,
                created: Instant::now(),
                items,
            },
        );
        self.order.push(id.clone());
        while self.order.len() > MAX_PLANS {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
        id
    }

    fn take(&mut self, id: &str) -> Option<StoredPlan> {
        self.order.retain(|x| x != id);
        self.map.remove(id)
    }

    #[cfg(test)]
    pub fn age_all(&mut self, by: Duration) {
        for p in self.map.values_mut() {
            p.created -= by;
        }
    }
}

fn item_label(i: &Value) -> String {
    match i["repo_name"].as_str() {
        Some(r) => format!("{r} › {}", i["name"].as_str().unwrap_or("")),
        None => i["name"].as_str().unwrap_or("").to_string(),
    }
}

/// Store a plan and present it to the model.
fn present(plans: &mut Plans, kind: PlanKind, plan: Value, read_only: bool) -> Value {
    let items: Vec<Value> = plan["items"].as_array().cloned().unwrap_or_default();
    let ready: Vec<Value> = items
        .iter()
        .filter(|i| i["ready"] == true)
        .map(|i| {
            json!({
                "target": i["target"],
                "what": item_label(i),
                "size": human_bytes(i["bytes"].as_u64().unwrap_or(0)),
                "bytes": i["bytes"],
                "branch": i["branch"],
                "idle_days": i["idle_days"],
                "uncommitted_files": i["dirty_files"],
                "verdict": i["verdict"],
                "would_back_up": i["would_backup"],
            })
        })
        .collect();
    let not_ready: Vec<Value> = items
        .iter()
        .filter(|i| i["ready"] != true)
        .map(|i| json!({ "target": i["target"], "reason": i["reason"], "detail": i["verdict"] }))
        .collect();
    let ready_bytes = plan["ready_bytes"].as_u64().unwrap_or(0);
    let mut out = json!({
        "kind": kind.name(),
        "ready": ready,
        "not_ready": not_ready,
        "ready_count": ready.len(),
        "ready_size": human_bytes(ready_bytes),
        "ready_bytes": ready_bytes,
    });
    if ready.is_empty() {
        out["next"] = json!("Nothing in this plan is ready, so there is nothing to execute.");
    } else if read_only {
        out["next"] = json!("This server is read-only, so the plan cannot be executed here. Show it to the user; they can run it with the mulchie CLI (`mulchie clean` or `mulchie sweep`) or a read-write MCP server.");
    } else {
        let id = plans.insert(kind, items);
        out["plan_id"] = json!(id);
        out["expires_in_secs"] = json!(PLAN_TTL.as_secs());
        out["next"] = json!("Show the user this plan (what, size, what would be backed up). To carry out the items they approve, call execute_plan with this plan_id and items set to their targets. Each item is re-checked just before it runs.");
    }
    out
}

/// How to get back what a done result removed.
fn restore_hint(result: &Value, item: Option<&Value>) -> Option<String> {
    if result["status"] != "done" {
        return None;
    }
    let target = result["target"].as_str().unwrap_or("");
    let backups = result["backups"].as_array().cloned().unwrap_or_default();
    let of = |kind: &str| {
        backups
            .iter()
            .find(|b| b["kind"] == kind)
            .and_then(|b| b["location"].as_str())
            .map(String::from)
    };
    Some(match result["action"].as_str().unwrap_or("") {
        "delete_cache" => "Nothing to restore: a build cache, recreated by the next build.".into(),
        "prune_worktree" => {
            "Nothing to restore: only git's record of a worktree whose directory was already gone."
                .into()
        }
        "remove_worktree" => {
            let branch =
                of("branch").or_else(|| item.and_then(|i| i["branch"].as_str()).map(String::from));
            let mut s = match branch {
                Some(b) => format!("From the repository, run `git worktree add '{target}' {b}`"),
                None => format!(
                    "From the repository, run `git worktree add '{target}' {}`",
                    item.and_then(|i| i["head"].as_str()).unwrap_or("<commit>")
                ),
            };
            if let Some(p) = of("patch") {
                s.push_str(&format!(
                    ", then `git -C '{target}' apply '{p}'` for the uncommitted changes"
                ));
            }
            if let Some(a) = of("ignored_archive") {
                s.push_str(&format!(
                    ", then `tar -xzf '{a}' -C '{target}'` for the ignored files"
                ));
            }
            s.push('.');
            s
        }
        _ => return None,
    })
}

fn execute(api: &dyn Api, plans: &mut Plans, a: &Map<String, Value>) -> ToolResult {
    let id = a.get("plan_id").and_then(Value::as_str).unwrap_or("");
    let Some(plan) = plans.take(id) else {
        return Err(format!(
            "no plan {id:?} in this session (plans can be executed once); make a new plan with plan_cache_cleanup, plan_worktree_removal or plan_worktree_sweep"
        ));
    };
    if plan.created.elapsed() > PLAN_TTL {
        return Err(
            "that plan is more than 30 minutes old; make a new one so it reflects the machine now"
                .into(),
        );
    }
    let wanted: Vec<&str> = a
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut requests = Vec::new();
    let mut chosen = Vec::new();
    for t in &wanted {
        let Some(item) = plan.items.iter().find(|i| i["target"] == *t) else {
            plans_restore(plans, id, plan);
            return Err(format!(
                "{t} is not in plan {id}; only targets listed in its ready items can be executed"
            ));
        };
        if item["ready"] != true {
            let why = item["verdict"].as_str().unwrap_or("").to_string();
            plans_restore(plans, id, plan);
            return Err(format!(
                "{t} was not ready in the plan ({why}), so it cannot be executed"
            ));
        }
        if !chosen.iter().any(|c: &&Value| c["target"] == *t) {
            chosen.push(item);
            requests.push(item["request"].clone());
        }
    }
    let results = api
        .call(
            "POST",
            plan.kind.route(),
            Some(&json!({ "items": requests })),
        )
        .map_err(api_err)?;
    let mut freed = 0u64;
    let mut done = 0;
    let mut out = Vec::new();
    for r in results.as_array().into_iter().flatten() {
        let item = plan.items.iter().find(|i| i["target"] == r["target"]);
        if r["status"] == "done" {
            done += 1;
            freed += r["freed_bytes"].as_u64().unwrap_or(0);
        }
        out.push(json!({
            "target": r["target"],
            "status": r["status"],
            "freed": human_bytes(r["freed_bytes"].as_u64().unwrap_or(0)),
            "backups": r["backups"],
            "restore": restore_hint(r, item),
            "skip_reason": r["reason"],
            "detail": r["detail"],
        }));
    }
    let total = out.len();
    Ok(json!({
        "done": done,
        "skipped": total - done,
        "freed": human_bytes(freed),
        "freed_bytes": freed,
        "results": out,
        "note": "Skipped items changed after the plan was made (or failed a safety check) and were left alone. Backups are kept in Mulchie's backups directory until the user deletes them.",
    }))
}

/// Put a plan back after a refused execute, so the model can retry.
fn plans_restore(plans: &mut Plans, id: &str, plan: StoredPlan) {
    plans.map.insert(id.to_string(), plan);
    plans.order.push(id.to_string());
}

// ------------------------------------------------------------------- calls

/// A tool failure the model should read.
pub type ToolResult = Result<Value, String>;

pub fn api_err(e: ApiError) -> String {
    match e.code.as_str() {
        "engine_not_running" | "engine_unreachable" => e.message,
        _ => crate::conn::describe(&e),
    }
}

/// The diagnosis, from the engine or, when none runs, a local look.
pub fn diagnosis(api: &dyn Api) -> ToolResult {
    let state = api.call("GET", "/v1/state", None);
    match state {
        Ok(st) => {
            let adv = api.call("GET", "/v1/advice", None).unwrap_or(Value::Null);
            Ok(diagnose::build(&st, &adv, Origin::Engine))
        }
        Err(e) if matches!(e.code.as_str(), "engine_not_running" | "engine_unreachable") => {
            Ok(match api.paths() {
                Some(p) => local_diagnosis(p, &e.message),
                None => {
                    diagnose::build(&Value::Null, &Value::Null, Origin::Unavailable(&e.message))
                }
            })
        }
        Err(e) => Err(api_err(e)),
    }
}

pub fn local_diagnosis(paths: &Paths, why: &str) -> Value {
    let engine = crate::commands::local_engine(paths, true);
    let st = serde_json::to_value(engine.snapshot_state()).unwrap_or(Value::Null);
    let adv = serde_json::to_value(engine.advice()).unwrap_or(Value::Null);
    diagnose::build(&st, &adv, Origin::Local(why))
}

pub struct Ctx<'a> {
    pub api: &'a dyn Api,
    pub plans: &'a mut Plans,
    pub read_only: bool,
}

pub fn call_tool(cx: &mut Ctx, name: &str, a: &Map<String, Value>) -> ToolResult {
    let api = cx.api;
    let get = |p: &str| api.call("GET", p, None).map_err(api_err);
    let post = |p: &str, b: Value| api.call("POST", p, Some(&b)).map_err(api_err);
    let arg = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    match name {
        "diagnose" => diagnosis(api),
        "get_status" => {
            let st = get("/v1/state")?;
            let snap = &st["snapshot"];
            Ok(json!({
                "health": st["health"],
                "sample_age_secs": st["sample_age_secs"],
                "config_error": st["config_error"],
                "low_disk": st["low_disk"],
                "forecast": st["forecast"],
                "slots": st["slots"],
                "throttled_builds": st["throttled"],
                "machine": {
                    "cpu_pct": snap["cpu_pct"], "cpu_count": snap["cpu_count"], "load": snap["load"],
                    "mem_total": snap["mem_total"], "mem_available": snap["mem_available"],
                    "swap_used": snap["swap_used"], "disk_total": snap["disk_total"], "disk_free": snap["disk_free"],
                    "read_bps": snap["read_bps"], "write_bps": snap["write_bps"],
                    "processes": snap["proc_count"], "agents": snap["agent_count"], "builds": snap["build_count"],
                },
                "top_groups": snap["groups"].as_array().map(|g| g.iter().take(10).cloned().collect::<Vec<_>>()),
                "recent_actions": st["actions"].as_array().map(|g| g.iter().take(10).cloned().collect::<Vec<_>>()),
            }))
        }
        "can_i_build" => get("/v1/advice"),
        "top_processes" => {
            let st = get("/v1/state")?;
            let sort = a.get("sort").and_then(Value::as_str).unwrap_or("cpu");
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let filter = a.get("workspace").and_then(Value::as_str).unwrap_or("");
            let mut procs: Vec<Value> = st["snapshot"]["top"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            procs.retain(|p| {
                filter.is_empty() || p["group"].as_str().unwrap_or("").contains(filter)
            });
            let n = |p: &Value, k: &str| p[k].as_f64().unwrap_or(0.0);
            let key = |p: &Value| match sort {
                "mem" => n(p, "mem_bytes"),
                "io" => n(p, "read_bps") + n(p, "write_bps"),
                _ => n(p, "cpu_pct"),
            };
            procs.sort_by(|x, y| key(y).total_cmp(&key(x)));
            procs.truncate(limit);
            Ok(Value::Array(procs))
        }
        "list_workspaces" => {
            let st = get("/v1/state")?;
            let mut ws = st["snapshot"]["workspaces"].clone();
            if !flag("include_processes") {
                for w in ws.as_array_mut().into_iter().flatten() {
                    if let Some(o) = w.as_object_mut() {
                        o.remove("procs");
                    }
                }
            }
            Ok(ws)
        }
        "get_history" => {
            let h = get("/v1/history")?;
            let mins = a
                .get("minutes")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .min(30);
            // 20 samples a minute.
            let n = mins.saturating_mul(20) as usize;
            let arr = h.as_array().cloned().unwrap_or_default();
            let start = arr.len().saturating_sub(n);
            let step = ((arr.len() - start) / 120).max(1);
            Ok(Value::Array(
                arr[start..].iter().step_by(step).cloned().collect(),
            ))
        }
        "disk_breakdown" => {
            let st = get("/v1/state")?;
            Ok(json!({
                "disk_free": st["snapshot"]["disk_free"],
                "disk_total": st["snapshot"]["disk_total"],
                "forecast": st["forecast"],
                "diskmap": st["diskmap"],
                "scanning": st["scanning"],
            }))
        }
        "reclaim_candidates" => {
            let st = get("/v1/state")?;
            if st["scan"].is_null() {
                return Ok(
                    json!({ "scan": null, "note": "the engine has not finished a scan yet; ask again in a minute" }),
                );
            }
            Ok(st["scan"].clone())
        }
        "plan_cache_cleanup" => {
            let plan = match (a.get("paths"), flag("stale")) {
                (Some(p), false) => post("/v1/clean/plan", json!({ "paths": p }))?,
                (None, true) => get("/v1/clean/plan")?,
                (Some(_), true) => return Err("pass either paths or stale=true, not both".into()),
                (None, false) => {
                    return Err(
                        "pass stale=true for every stale cache, or paths from reclaim_candidates"
                            .into(),
                    )
                }
            };
            Ok(present(cx.plans, PlanKind::Caches, plan, cx.read_only))
        }
        "plan_worktree_removal" => {
            let plan = post(
                "/v1/worktree/plan",
                json!({ "paths": arg("paths"), "allow_dirty": flag("allow_dirty") }),
            )?;
            Ok(present(cx.plans, PlanKind::Worktrees, plan, cx.read_only))
        }
        "plan_worktree_sweep" => {
            let plan = get("/v1/sweep")?;
            Ok(present(cx.plans, PlanKind::Sweep, plan, cx.read_only))
        }
        "execute_plan" => execute(api, cx.plans, a),
        "prune_cache" => post("/v1/prune", json!({ "key": arg("key") })),
        "workspace_action" => {
            if !flag("execute") {
                let st = get("/v1/state")?;
                let path = arg("path");
                let ws = st["snapshot"]["workspaces"]
                    .as_array()
                    .and_then(|w| w.iter().find(|w| w["path"] == path).cloned())
                    .ok_or("that workspace has no running processes")?;
                let procs: Vec<Value> = ws["procs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p["own"] != false)
                    .filter(|p| arg("action") != "stop_builds" || p["kind"] == "build")
                    .map(|p| json!({ "pid": p["pid"], "name": p["name"], "kind": p["kind"], "cmd": p["cmd"] }))
                    .collect();
                return Ok(json!({
                    "preview": true,
                    "workspace": ws["label"],
                    "action": arg("action"),
                    "would_affect": procs,
                    "next": "Nothing was done. Show this to the user; if they agree, call workspace_action again with execute=true.",
                }));
            }
            post(
                "/v1/workspace",
                json!({ "path": arg("path"), "action": arg("action") }),
            )
        }
        "process_action" => {
            if !flag("execute") {
                let st = get("/v1/state")?;
                let pid = arg("pid");
                let snap = &st["snapshot"];
                let found = snap["top"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(
                        snap["workspaces"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .flat_map(|w| w["procs"].as_array().into_iter().flatten()),
                    )
                    .find(|p| p["pid"] == pid)
                    .cloned()
                    .ok_or("unknown pid; it may have exited, or it is not among the processes Mulchie tracks")?;
                return Ok(json!({
                    "preview": true,
                    "action": arg("action"),
                    "process": found,
                    "next": "Nothing was done. Show this to the user; if they agree, call process_action again with execute=true.",
                }));
            }
            let mut b = json!({ "pid": arg("pid"), "action": arg("action") });
            if a.get("action").and_then(Value::as_str) == Some("renice") {
                b["nice"] = json!(a.get("nice").and_then(Value::as_i64).unwrap_or(10));
            }
            post("/v1/process", b)
        }
        // A TTL, never this server's pid: `mulchie mcp` lives as long as the
        // agent session, so a forgotten release would hold the slot for hours.
        "acquire_build_slot" => post(
            "/v1/lease",
            json!({
                "label": a.get("label").and_then(Value::as_str).unwrap_or("mcp agent"),
                "ticket": arg("ticket"),
                "ttl_secs": a.get("ttl_secs").and_then(Value::as_u64).unwrap_or(900),
            }),
        ),
        "release_build_slot" => post("/v1/release", json!({ "ticket": arg("ticket") })),
        other => Err(format!("unknown tool {other}")),
    }
}

/// The text for a successful result.
pub fn text_of(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
