//! `mulchie mcp`: an MCP server on stdio (newline-delimited JSON-RPC 2.0).
//!
//! Dual-era, per the MCP versioning rules:
//! - legacy clients (protocol 2025-11-25 and earlier) open with `initialize`;
//!   the server answers with the client's version when it supports it, else
//!   the latest legacy version it supports;
//! - modern clients (2026-07-28) send the version in every request's
//!   `_meta` and may call `server/discover`; results carry
//!   `resultType: "complete"`.
//!
//! Protocol problems are JSON-RPC errors (-32700 parse, -32600 invalid
//! request, -32601 unknown method, -32602 invalid params or unknown tool,
//! -32022 unsupported version). Problems running a tool, including invalid
//! arguments and a stopped engine, are tool results with `isError: true` so
//! the model can read them and recover. Notifications get no reply; unknown
//! ones are ignored, and so are responses from the client.

use crate::conn::{self, Conn, START_HINT};
use mulchie_core::api::{ApiError, Client};
use mulchie_core::Paths;
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::sync::Mutex;

pub const MODERN_VERSION: &str = "2026-07-28";
/// Newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "Mulchie keeps this machine healthy while several agents share it. \
Call can_i_build before heavy builds or test suites. When several agents share the machine, \
wrap heavy work in acquire_build_slot / release_build_slot: poll acquire_build_slot with the \
returned ticket until granted, renew it with the same ticket before ttl_secs passes, and \
release it as soon as the work ends. Use top_processes and list_workspaces to see what uses \
CPU, memory or disk I/O, and disk_breakdown for where disk space went. Destructive tools \
(clean_caches, remove_worktrees, sweep_worktrees, prune_cache, workspace_action, \
process_action) accept dry_run=true where noted to return the plan first; they only touch \
recognised build caches, idle git worktrees (after backing up uncommitted work) and \
processes owned by the current user.";

fn server_info() -> Value {
    json!({ "name": "mulchie", "title": "Mulchie", "version": env!("CARGO_PKG_VERSION") })
}

fn all_versions() -> Vec<&'static str> {
    std::iter::once(MODERN_VERSION)
        .chain(LEGACY_VERSIONS.iter().copied())
        .collect()
}

// ------------------------------------------------------------- engine access

/// The engine's HTTP API, as the tools see it.
pub trait Api {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError>;
}

/// The running engine, connected on first use and again after it restarts.
pub struct EngineApi {
    paths: Paths,
    client: Mutex<Option<Client>>,
}

impl EngineApi {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            client: Mutex::new(None),
        }
    }
}

impl Api for EngineApi {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let mut guard = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            match conn::connect(&self.paths) {
                Conn::Engine(c) => *guard = Some(c),
                Conn::NotRunning => {
                    return Err(ApiError::new(
                        0,
                        "engine_not_running",
                        format!("Mulchie's engine is not running; {START_HINT}. Ask the user to start it."),
                    ))
                }
                other => {
                    return Err(ApiError::new(
                        0,
                        "engine_unreachable",
                        format!("{}; the user can run `mulchie doctor` to see why", other.why_not()),
                    ))
                }
            }
        }
        let c = guard.clone().expect("connected above");
        drop(guard);
        let r = conn::call(&c, method, path, body);
        if let Err(e) = &r {
            if matches!(e.code.as_str(), "unreachable" | "io_error" | "unauthorized") {
                // The engine restarted or went away: reconnect next time.
                *self.client.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        r
    }
}

// --------------------------------------------------------------------- tools

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// `properties` of the input schema.
    props: Value,
    required: &'static [&'static str],
    read_only: bool,
    destructive: bool,
    idempotent: bool,
}

fn tool_list() -> Vec<Tool> {
    let dry_run = json!({ "type": "boolean", "description": "Return the plan without changing anything" });
    vec![
        Tool {
            name: "get_status",
            title: "Machine status",
            description: "Machine health: disk, memory, CPU, load, swap, agent and build counts, build slots, disk-fill forecast, recent actions. Says when the engine's figures are stale.",
            props: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "can_i_build",
            title: "Can I start heavy work?",
            description: "Whether now is a good moment to start heavy work (builds, test suites), how many build slots are free, and the suggested -j job count.",
            props: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "top_processes",
            title: "Top processes",
            description: "Processes ranked by CPU, memory or disk I/O, with pid, kind (agent/build/runtime), workspace, niceness, state and command line.",
            props: json!({
                "sort": { "type": "string", "enum": ["cpu", "mem", "io"], "description": "Rank by this (default cpu)" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "How many (default 20)" },
                "workspace": { "type": "string", "description": "Only processes whose group contains this text" }
            }),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "list_workspaces",
            title: "Workspaces",
            description: "Every repo or worktree with running processes: CPU, memory, I/O, agents, builds, paused/throttled state and orphan warnings.",
            props: json!({ "include_processes": { "type": "boolean", "description": "Include each workspace's processes" } }),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "get_history",
            title: "Recent history",
            description: "Time series sampled every 3 s for up to the last 30 minutes (at most 120 points): cpu, load, memory, swap, disk free, I/O and top workspaces.",
            props: json!({ "minutes": { "type": "integer", "minimum": 1, "maximum": 30, "description": "How far back (default 10)" } }),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "disk_breakdown",
            title: "Disk breakdown",
            description: "Where disk space went: toolchain and package caches, model stores, and every git worktree with size, idle days, dirty files and a removal verdict.",
            props: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "reclaim_candidates",
            title: "Reclaimable build caches",
            description: "Build caches (Rust target, .next, node_modules if enabled, Xcode DerivedData if enabled) with size, last use, and in-use and stale flags, from the engine's last scan.",
            props: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "clean_caches",
            title: "Delete build caches",
            description: "Delete recognised build caches: pass paths from reclaim_candidates, or stale=true for every stale cache. The engine plans first (refusing caches in projects with running processes, and with stale=true anything no longer stale) and deletes exactly the ready items of that plan. dry_run=true returns the plan only.",
            props: json!({
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute cache directories" },
                "stale": { "type": "boolean", "description": "Every stale cache instead of paths" },
                "dry_run": dry_run.clone()
            }),
            required: &[],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "prune_cache",
            title: "Prune a package manager cache",
            description: "Run a package manager's own prune command for one cache key from disk_breakdown (npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators). Refused while that tool is running.",
            props: json!({ "key": { "type": "string", "description": "The cache key" } }),
            required: &["key"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "remove_worktrees",
            title: "Remove git worktrees",
            description: "Remove git worktrees by path. The engine plans first, reading each HEAD now; removal is skipped if HEAD moves afterwards. Branches are kept; a detached HEAD gets a backup/<name> branch. Worktrees with uncommitted changes are refused unless allow_dirty=true, which saves a patch first. dry_run=true returns the plan only.",
            props: json!({
                "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Absolute worktree paths" },
                "allow_dirty": { "type": "boolean", "description": "Remove even with uncommitted changes (a patch is saved first)" },
                "dry_run": dry_run.clone()
            }),
            required: &["paths"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "sweep_worktrees",
            title: "Sweep idle agent worktrees",
            description: "Remove every idle agent worktree past the configured idle window, after backing up uncommitted work. The engine plans first and removes exactly the ready items. dry_run=true returns the plan only.",
            props: json!({ "dry_run": dry_run.clone() }),
            required: &[],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "workspace_action",
            title: "Act on a workspace's processes",
            description: "Act on all of a workspace's processes owned by the current user: pause (SIGSTOP), resume (SIGCONT), deprioritize (renice), stop_builds (SIGTERM to builds only) or kill_all (SIGTERM to everything). dry_run=true lists the processes it would affect.",
            props: json!({
                "path": { "type": "string", "description": "The workspace path from list_workspaces" },
                "action": { "type": "string", "enum": ["pause", "resume", "deprioritize", "stop_builds", "kill_all"] },
                "dry_run": dry_run.clone()
            }),
            required: &["path", "action"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "process_action",
            title: "Signal or renice a process",
            description: "Signal (term, kill, stop, cont) or renice one process owned by the current user. A pid reused by another process since the last sample is refused. dry_run=true shows the process it would act on.",
            props: json!({
                "pid": { "type": "integer", "minimum": 2 },
                "action": { "type": "string", "enum": ["term", "kill", "stop", "cont", "renice"] },
                "nice": { "type": "integer", "minimum": 0, "maximum": 20, "description": "For renice (default 10)" },
                "dry_run": dry_run
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
    ]
}

impl Tool {
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": self.props,
            "required": self.required,
            "additionalProperties": false,
        })
    }

    fn describe(&self) -> Value {
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

/// Check arguments against the small schema subset the tools use.
fn validate(schema: &Value, args: &Map<String, Value>) -> Result<(), String> {
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    for (k, v) in args {
        let Some(p) = props.get(k) else {
            let known: Vec<&str> = props.keys().map(String::as_str).collect();
            return Err(if known.is_empty() {
                format!("unknown argument `{k}`: this tool takes no arguments")
            } else {
                format!("unknown argument `{k}`; expected one of: {}", known.join(", "))
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

/// A tool failure the model should read.
type ToolResult = Result<Value, String>;

fn api_err(e: ApiError) -> String {
    match e.code.as_str() {
        "engine_not_running" | "engine_unreachable" => e.message,
        _ => conn::describe(&e),
    }
}

/// Requests of a plan's ready items, and its other items.
fn split_plan(plan: &Value) -> (Vec<Value>, Vec<Value>) {
    let mut ready = Vec::new();
    let mut not_ready = Vec::new();
    for i in plan["items"].as_array().into_iter().flatten() {
        if i["ready"] == true {
            ready.push(i["request"].clone());
        } else {
            not_ready.push(json!({
                "target": i["target"],
                "reason": i["reason"],
                "detail": i["verdict"],
            }));
        }
    }
    (ready, not_ready)
}

/// Execute exactly the ready items of `plan` through `route`.
fn execute_plan(api: &dyn Api, plan: &Value, route: &str) -> ToolResult {
    let (ready, not_ready) = split_plan(plan);
    if ready.is_empty() {
        return Ok(json!({ "results": [], "not_ready": not_ready, "note": "nothing in the plan was ready, so nothing was changed" }));
    }
    let results = api
        .call("POST", route, Some(&json!({ "items": ready })))
        .map_err(api_err)?;
    Ok(json!({ "results": results, "not_ready": not_ready }))
}

fn call_tool(api: &dyn Api, name: &str, a: &Map<String, Value>) -> ToolResult {
    let get = |p: &str| api.call("GET", p, None).map_err(api_err);
    let post = |p: &str, b: Value| api.call("POST", p, Some(&b)).map_err(api_err);
    let arg = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    match name {
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
            procs.retain(|p| filter.is_empty() || p["group"].as_str().unwrap_or("").contains(filter));
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
            let mins = a.get("minutes").and_then(Value::as_u64).unwrap_or(10).min(30);
            // 20 samples a minute.
            let n = mins.saturating_mul(20) as usize;
            let arr = h.as_array().cloned().unwrap_or_default();
            let start = arr.len().saturating_sub(n);
            let step = ((arr.len() - start) / 120).max(1);
            Ok(Value::Array(arr[start..].iter().step_by(step).cloned().collect()))
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
                let _ = post("/v1/scan", json!({}));
                return Ok(json!({ "scan": null, "note": "the engine has not finished a scan yet; one was requested, so ask again in a minute" }));
            }
            Ok(st["scan"].clone())
        }
        "clean_caches" => {
            let paths = a.get("paths").and_then(Value::as_array);
            let plan = match (paths, flag("stale")) {
                (Some(p), false) if !p.is_empty() => {
                    post("/v1/clean/plan", json!({ "paths": p }))?
                }
                (None, true) => get("/v1/clean/plan")?,
                (Some(_), true) => return Err("pass either paths or stale=true, not both".into()),
                _ => return Err("pass paths (from reclaim_candidates) or stale=true".into()),
            };
            if flag("dry_run") {
                return Ok(plan);
            }
            execute_plan(api, &plan, "/v1/clean")
        }
        "prune_cache" => post("/v1/prune", json!({ "key": arg("key") })),
        "remove_worktrees" => {
            let plan = post(
                "/v1/worktree/plan",
                json!({ "paths": arg("paths"), "allow_dirty": flag("allow_dirty") }),
            )?;
            if flag("dry_run") {
                return Ok(plan);
            }
            execute_plan(api, &plan, "/v1/worktree/remove")
        }
        "sweep_worktrees" => {
            let plan = get("/v1/sweep")?;
            if flag("dry_run") {
                return Ok(plan);
            }
            execute_plan(api, &plan, "/v1/sweep")
        }
        "workspace_action" => {
            if flag("dry_run") {
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
                    .map(|p| json!({ "pid": p["pid"], "name": p["name"], "kind": p["kind"] }))
                    .collect();
                return Ok(json!({ "workspace": ws["label"], "action": arg("action"), "would_affect": procs }));
            }
            post("/v1/workspace", json!({ "path": arg("path"), "action": arg("action") }))
        }
        "process_action" => {
            if flag("dry_run") {
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
                return Ok(json!({ "action": arg("action"), "process": found }));
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

// ------------------------------------------------------------------ protocol

/// A JSON-RPC error.
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

fn error_reply(id: Value, e: RpcError) -> Value {
    let mut err = json!({ "code": e.code, "message": e.message });
    if let Some(d) = e.data {
        err["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": err })
}

/// One server session over one stream.
pub struct Session<'a> {
    api: &'a dyn Api,
    /// The version agreed by `initialize`, for legacy clients.
    pub negotiated: Option<String>,
}

impl<'a> Session<'a> {
    pub fn new(api: &'a dyn Api) -> Self {
        Self {
            api,
            negotiated: None,
        }
    }

    /// Handle one line; the reply to write, if any.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.handle(v),
            Err(e) => Some(error_reply(
                Value::Null,
                rpc_err(PARSE_ERROR, format!("parse error: {e}")),
            )),
        }
    }

    /// Handle one parsed message; the reply to write, if any.
    pub fn handle(&mut self, msg: Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            let what = if msg.is_array() {
                "batches are not supported; send one message per line"
            } else {
                "a message must be a JSON object"
            };
            return Some(error_reply(Value::Null, rpc_err(INVALID_REQUEST, what)));
        };
        let id = obj.get("id").cloned();
        let id_ok = matches!(id, Some(Value::String(_)) | Some(Value::Number(_)));
        let Some(method) = obj.get("method") else {
            // A response from the client (we send no requests), or junk.
            if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) {
                return None;
            }
            let rid = if id_ok { id.unwrap_or(Value::Null) } else { Value::Null };
            return Some(error_reply(rid, rpc_err(INVALID_REQUEST, "missing method")));
        };
        let Some(method) = method.as_str() else {
            let rid = if id_ok { id.unwrap_or(Value::Null) } else { Value::Null };
            return Some(error_reply(rid, rpc_err(INVALID_REQUEST, "method must be a string")));
        };
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            if id.is_none() {
                return None;
            }
            let rid = if id_ok { id.unwrap_or(Value::Null) } else { Value::Null };
            return Some(error_reply(rid, rpc_err(INVALID_REQUEST, "jsonrpc must be \"2.0\"")));
        }
        let Some(id) = id else {
            // A notification: never answered. `notifications/initialized`,
            // `notifications/cancelled` and anything unknown are all fine.
            return None;
        };
        if !id_ok {
            return Some(error_reply(
                Value::Null,
                rpc_err(INVALID_REQUEST, "id must be a string or a number"),
            ));
        }
        let params = match obj.get("params") {
            None => Value::Object(Map::new()),
            Some(p @ Value::Object(_)) => p.clone(),
            Some(_) => {
                return Some(error_reply(
                    id,
                    rpc_err(INVALID_PARAMS, "params must be an object"),
                ))
            }
        };
        let modern = method != "initialize"
            && params
                .get("_meta")
                .and_then(|m| m.get(META_VERSION))
                .is_some();
        let result = if modern {
            self.modern(method, &params)
        } else {
            self.dispatch(method, &params)
        };
        Some(match result {
            Ok(mut r) => {
                if modern {
                    if let Some(o) = r.as_object_mut() {
                        o.insert("resultType".into(), json!("complete"));
                        let meta = o.entry("_meta").or_insert_with(|| json!({}));
                        meta[META_SERVER] = server_info();
                    }
                }
                json!({ "jsonrpc": "2.0", "id": id, "result": r })
            }
            Err(e) => error_reply(id, e),
        })
    }

    /// A request carrying per-request `_meta` (protocol 2026-07-28).
    fn modern(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let meta = &params["_meta"];
        let requested = meta[META_VERSION].as_str().unwrap_or("");
        if requested != MODERN_VERSION {
            return Err(RpcError {
                code: UNSUPPORTED_VERSION,
                message: "Unsupported protocol version".into(),
                data: Some(json!({ "supported": all_versions(), "requested": meta[META_VERSION] })),
            });
        }
        if !meta[META_CAPS].is_object() {
            return Err(rpc_err(
                INVALID_PARAMS,
                format!("_meta must include {META_CAPS}"),
            ));
        }
        if method == "server/discover" {
            return Ok(json!({
                "supportedVersions": all_versions(),
                "capabilities": { "tools": {} },
                "instructions": INSTRUCTIONS,
            }));
        }
        self.dispatch(method, params)
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let Some(asked) = params["protocolVersion"].as_str() else {
                    return Err(rpc_err(INVALID_PARAMS, "initialize needs protocolVersion"));
                };
                let version = if LEGACY_VERSIONS.contains(&asked) {
                    asked
                } else {
                    LEGACY_VERSIONS[0]
                };
                self.negotiated = Some(version.to_string());
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": server_info(),
                    "instructions": INSTRUCTIONS,
                }))
            }
            "server/discover" => Err(rpc_err(
                INVALID_PARAMS,
                format!("server/discover needs _meta with {META_VERSION} and {META_CAPS}"),
            )),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": tool_list().iter().map(Tool::describe).collect::<Vec<_>>()
            })),
            "tools/call" => {
                let Some(name) = params["name"].as_str() else {
                    return Err(rpc_err(INVALID_PARAMS, "tools/call needs a tool name"));
                };
                let tools = tool_list();
                let Some(tool) = tools.iter().find(|t| t.name == name) else {
                    return Err(rpc_err(INVALID_PARAMS, format!("Unknown tool: {name}")));
                };
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(m)) => m.clone(),
                    Some(_) => return Err(rpc_err(INVALID_PARAMS, "arguments must be an object")),
                };
                let outcome = validate(&tool.schema(), &args)
                    .and_then(|()| call_tool(self.api, name, &args));
                Ok(match outcome {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                        "isError": false,
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": e }],
                        "isError": true,
                    }),
                })
            }
            _ => Err(rpc_err(METHOD_NOT_FOUND, format!("method not found: {method}"))),
        }
    }
}

/// Serve until the reader ends.
pub fn serve<R: BufRead, W: Write>(mut reader: R, mut writer: W, api: &dyn Api) -> std::io::Result<()> {
    let mut session = Session::new(api);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = session.handle_line(line.trim_end_matches(['\r', '\n'])) {
            writeln!(writer, "{reply}")?;
            writer.flush()?;
        }
    }
}

pub fn command(paths: &Paths) -> i32 {
    let api = EngineApi::new(paths.clone());
    let stdin = std::io::stdin();
    match serve(stdin.lock(), std::io::stdout().lock(), &api) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("mulchie mcp: {e}");
            1
        }
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
