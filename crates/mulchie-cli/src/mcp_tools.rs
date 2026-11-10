//! The MCP tools. Read tools and plan tools change nothing. Deleting goes
//! through two calls: a plan tool returns a `plan_id` and its items, and
//! `execute_plan` carries out only items named from that stored plan (the
//! engine's safety gate re-checks each one). An agent cannot delete anything
//! it was not first shown in a plan.

use crate::diagnose::{self, Origin};
use crate::hooks_install::sh_quote as q;
use crate::mcp::Api;
use mulchie_core::api::ApiError;
use mulchie_core::{human_bytes, Paths};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
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
            "Start here. One compact, prioritised picture of the machine: health and why, CPU and memory explained in words with the apps and workspaces using the most, the build queue, reclaimable space (stale build caches, idle agent worktrees, package caches) with sizes, watch findings (observations, most severe first), engine problems, and recommendations. Each recommendation and finding has a plain reason and the exact next tool call. Changes nothing.",
            json!({}),
        ),
        read(
            "get_status",
            "Machine status",
            "CPU and memory in words, the top consumer groups (apps, workspaces, processes) by CPU and by memory with their group keys and the actions their plans can offer, and raw figures: disk, load, swap, agent and build counts, build slots, disk-fill forecast, recent actions. diagnose is usually the better first call.",
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
            "plan_process_action",
            "Plan acting on processes",
            "Plan an action on one process (pid with signal term/kill/stop/cont, tree=true for its descendants too) or on a whole consumer group (group key from get_status or diagnose, with action quit, force_quit, pause, resume, lower_priority, stop_builds or restart; restart is offered only where the plan says so). Changes nothing. Returns a plan_id, what would happen to each process, and why anything is refused (protected processes, including this agent's own app and terminal, are never acted on). Show the plan to the user; to carry it out, call execute_plan with the plan_id and the approved targets (all ready targets for the whole action). Each process is re-checked by its start time, so a reused pid is skipped.",
            json!({
                "pid": { "type": "integer", "minimum": 2, "description": "One process, instead of group" },
                "signal": { "type": "string", "enum": ["term", "kill", "stop", "cont"], "description": "With pid (default term)" },
                "tree": { "type": "boolean", "description": "With pid: its descendants too, deepest first" },
                "group": { "type": "string", "description": "A consumer group key, instead of pid" },
                "action": { "type": "string", "enum": ["quit", "force_quit", "pause", "resume", "lower_priority", "stop_builds", "restart"], "description": "With group" }
            }),
        ),
        read(
            "list_findings",
            "Watch findings",
            "What Mulchie's watch noticed: leftover, runaway, stopped, exposed or oddly placed processes, and problems with Mulchie's own setup. These are observations from heuristics, not verdicts and not malware detection; each says why it was raised, the evidence, and suggested next calls. Most severe first; dismissed ones are left out unless include_dismissed=true.",
            json!({
                "severity": { "type": "string", "enum": ["info", "low", "medium", "high"], "description": "Minimum severity" },
                "kind": { "type": "string", "description": "Only this finding kind, e.g. runaway_cpu" },
                "include_dismissed": { "type": "boolean" }
            }),
        ),
        Tool {
            required: &["pid"],
            ..read(
                "inspect_process",
                "Inspect a process",
                "Everything known about one process, read now: name, executable, command line (secrets redacted), working directory, workspace, consumer group, parents and children, age, CPU and memory, listening ports (from the last watch run), code signature where checked, whether it is protected, and its findings.",
                json!({ "pid": { "type": "integer", "minimum": 0 } }),
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
            description: "Carry out items from a plan made by plan_cache_cleanup, plan_worktree_removal, plan_worktree_sweep or plan_process_action on this server. Plans are kept in this server process's memory, so they are lost if the server restarts. items must be targets of ready items in that plan; anything else is refused. Only call this after the user has seen the plan and approved these items. Each item is re-checked just before it runs and skipped if anything changed. The result lists what was done, what was skipped and why, every backup with its path, and how to restore. A plan can be executed once and expires after 30 minutes.",
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
            description: "Run a package manager's own prune command for one cache key from disk_breakdown or diagnose (npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators). Without execute=true it only shows the cache, its size and what pruning does; show that to the user and call again with execute=true only if they agree. The package manager re-downloads anything it needs later. Refused while that tool is running.",
            props: json!({
                "key": { "type": "string", "description": "The cache key" },
                "execute": { "type": "boolean", "description": "Prune, instead of only showing what would be pruned" }
            }),
            required: &["key"],
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        Tool {
            name: "dismiss_finding",
            title: "Dismiss or restore a finding",
            description: "Hide a finding the user has looked at and does not want to see again, for days (default 30), or show it again with undo=true. Saved in Mulchie's config. Ask the user first; it changes nothing on the machine. An id that is not a current or dismissed finding is refused with the closest ids, unless force=true (to hide one that is not showing yet).",
            props: json!({
                "id": { "type": "string", "description": "The finding id from list_findings or diagnose" },
                "days": { "type": "integer", "minimum": 1, "maximum": 3650 },
                "undo": { "type": "boolean", "description": "Show a dismissed finding again" },
                "force": { "type": "boolean", "description": "Dismiss an id that is not a current finding" }
            }),
            required: &["id"],
            read_only: false,
            destructive: false,
            idempotent: true,
        },
        Tool {
            name: "acquire_build_slot",
            title: "Join the build queue",
            description: "Join the machine-wide build queue before heavy work. Call it, then keep calling it with the returned ticket (every few seconds) until granted is true; then run with the returned job count. A granted slot lasts ttl_secs (default 900, at most 3600): for a longer build, call again with the same ticket before ttl_secs passes to renew it, or the slot expires and goes to someone else. Always call release_build_slot as soon as the work ends; slots this server still holds are released when it exits.",
            props: json!({
                "label": { "type": "string", "description": "What the slot is for, shown to the user" },
                "ticket": { "type": "integer", "minimum": 1, "description": "The ticket from an earlier call: poll or renew" },
                "ttl_secs": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "How long the slot lasts without renewal (default 900, at most 3600; renew for longer work)" }
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

/// Build-slot tools: allowed in read-only mode (the read-only token may take
/// TTL leases).
const SLOT_TOOLS: [&str; 2] = ["acquire_build_slot", "release_build_slot"];

/// Tools offered in read-only mode: those that change nothing, and build slots.
pub fn allowed(tool: &Tool, read_only: bool) -> bool {
    !read_only || tool.read_only || SLOT_TOOLS.contains(&tool.name)
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
    Process,
}

impl PlanKind {
    fn route(self) -> &'static str {
        match self {
            PlanKind::Caches => "/v1/clean",
            PlanKind::Worktrees => "/v1/worktree/remove",
            PlanKind::Sweep => "/v1/sweep",
            PlanKind::Process => "/v1/process/execute",
        }
    }

    fn name(self) -> &'static str {
        match self {
            PlanKind::Caches => "delete build caches",
            PlanKind::Worktrees => "remove worktrees",
            PlanKind::Sweep => "sweep idle agent worktrees",
            PlanKind::Process => "act on processes",
        }
    }
}

struct StoredPlan {
    kind: PlanKind,
    created: Instant,
    items: Vec<Value>,
    /// Carried out whole or not at all: a group or tree action, whose
    /// parts (a restart's quit, a parent's children) belong together.
    whole: bool,
}

/// Plans shown by this server process, by id (in memory only). Execution needs one of these.
#[derive(Default)]
pub struct Plans {
    map: HashMap<String, StoredPlan>,
    order: Vec<String>,
}

impl Plans {
    fn insert(&mut self, kind: PlanKind, items: Vec<Value>) -> String {
        self.insert_as(kind, items, false)
    }

    fn insert_as(&mut self, kind: PlanKind, items: Vec<Value>, whole: bool) -> String {
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
                whole,
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

/// Why a process plan cannot be carried out, in the core's words: its
/// `reason`/`blocked` text, its detail, or the refusal code and the verdicts of
/// the items that are not ready.
pub fn blocked_reason(plan: &Value) -> Option<String> {
    for k in ["reason", "blocked"] {
        if let Some(r) = plan[k].as_str().filter(|r| !r.is_empty()) {
            return Some(r.to_string());
        }
    }
    let refused = plan["refused"].as_str();
    let verdicts: Vec<&str> = plan["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["ready"] != true)
        .filter_map(|i| i["verdict"].as_str().or(i["reason"].as_str()))
        .collect();
    match (refused, plan["detail"].as_str().filter(|d| !d.is_empty())) {
        (Some(code), Some(d)) => Some(format!("{d} [{code}]")),
        (Some(code), None) if !verdicts.is_empty() => {
            Some(format!("{} [{code}]", verdicts.join("; ")))
        }
        (Some(code), None) => Some(code.to_string()),
        (None, _) if !verdicts.is_empty() => Some(verdicts.join("; ")),
        _ => None,
    }
}

/// Store a process plan and present it.
fn present_process(plans: &mut Plans, plan: Value, read_only: bool) -> Value {
    let items: Vec<Value> = plan["items"].as_array().cloned().unwrap_or_default();
    let ready: Vec<Value> = items
        .iter()
        .filter(|i| i["ready"] == true)
        .map(|i| json!({ "target": i["target"], "what": i["name"], "verdict": i["verdict"] }))
        .collect();
    let not_ready: Vec<Value> = items
        .iter()
        .filter(|i| i["ready"] != true)
        .map(|i| json!({ "target": i["target"], "reason": i["reason"], "detail": i["verdict"] }))
        .collect();
    let mut out = json!({
        "kind": PlanKind::Process.name(),
        "target": plan["target"],
        "name": plan["name"],
        "action": plan["action"],
        "detail": plan["detail"],
        "affected": plan["affected"],
        "ready": ready,
        "not_ready": not_ready,
        "ready_count": ready.len(),
    });
    for k in ["refused", "blocked", "reason", "restart"] {
        if !plan[k].is_null() {
            out[k] = plan[k].clone();
        }
    }
    if ready.is_empty() || plan["ready"] == false {
        // Not an error: a blocked plan (protected, or one of this agent's
        // own ancestors) says why, and that is the answer to show.
        let why = blocked_reason(&plan);
        out["next"] = json!(match why {
            Some(w) => format!("Nothing in this plan can be done: {w}. Tell the user why; there is nothing to execute."),
            None => "Nothing in this plan can be done (see detail), so there is nothing to execute.".into(),
        });
    } else if read_only {
        out["next"] = json!("This server is read-only, so the plan cannot be executed here. Show it to the user; they can carry it out with `mulchie signal` or `mulchie group`, or through a read-write MCP server.");
    } else {
        // More than one ready target means a group or a tree: all or none.
        let whole = ready.len() > 1;
        let id = plans.insert_as(PlanKind::Process, items, whole);
        if whole {
            out["all_or_nothing"] = json!(true);
        }
        out["plan_id"] = json!(id);
        out["expires_in_secs"] = json!(PLAN_TTL.as_secs());
        out["next"] = json!("Show the user what would happen to each affected process. If they agree, call execute_plan with this plan_id and items set to the ready targets (all of them for the whole action). Each process is re-checked by its start time first.");
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
        "signal" => {
            let sig = item
                .and_then(|i| i["request"]["signal"].as_str())
                .unwrap_or("");
            match sig {
                "stop" => format!("Paused, not ended: plan_process_action with pid and signal=cont resumes {target}."),
                "cont" => "Nothing to restore: the process was resumed.".into(),
                _ => "A signalled process cannot be brought back; start the program again if it is needed.".into(),
            }
        }
        "renice" => "Lower priority lasts until the process ends; nothing else changed.".into(),
        "reopen_app" => "The app was opened again.".into(),
        "remove_worktree" => {
            let branch =
                of("branch").or_else(|| item.and_then(|i| i["branch"].as_str()).map(String::from));
            let mut s = match branch {
                Some(b) => format!(
                    "From the repository, run `git worktree add {} {}`",
                    q(target),
                    q(&b)
                ),
                None => format!(
                    "From the repository, run `git worktree add {} {}`",
                    q(target),
                    q(item.and_then(|i| i["head"].as_str()).unwrap_or("<commit>"))
                ),
            };
            if let Some(p) = of("patch") {
                s.push_str(&format!(
                    ", then `git -C {} apply {}` for the uncommitted changes",
                    q(target),
                    q(&p)
                ));
            }
            if let Some(a) = of("ignored_archive") {
                s.push_str(&format!(
                    ", then `tar -xzf {} -C {}` for the ignored files",
                    q(&a),
                    q(target)
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
            "no plan {id:?} on this server: plans live in this server process's memory, are lost when it restarts, and can be executed once; make a new plan with plan_cache_cleanup, plan_worktree_removal, plan_worktree_sweep or plan_process_action"
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
    if plan.whole {
        let ready = plan.items.iter().filter(|i| i["ready"] == true).count();
        if chosen.len() != ready {
            plans_restore(plans, id, plan);
            return Err(format!(
                "plan {id} is a group or tree action and is carried out whole: pass all {ready} ready targets, or none. Part of it (a restart without its quit, a parent without its children) is not what the user reviewed; make a new plan for a smaller action."
            ));
        }
    }
    // From here the plan is used: whatever happens, a retry needs a new one.
    let check = "It may have acted on some items: check get_status or the mulchie://actions resource before anything else. This plan has been used; make a new plan if more is needed.";
    let results = api
        .call(
            "POST",
            plan.kind.route(),
            Some(&json!({ "items": requests })),
        )
        .map_err(|e| {
            if e.status == 0 {
                format!("The engine did not answer ({}). {check}", api_err(e))
            } else {
                format!(
                    "{}. This plan has been used; make a new plan if more is needed.",
                    api_err(e)
                )
            }
        })?;
    if !results.is_array() {
        return Err(format!(
            "Unexpected reply from the engine; this server and the engine may be different versions. {check}"
        ));
    }
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
pub fn diagnosis(api: &dyn Api, read_only: bool) -> ToolResult {
    let state = api.call("GET", "/v1/state", None);
    let mut d = match state {
        Ok(st) => {
            let adv = api.call("GET", "/v1/advice", None).unwrap_or(Value::Null);
            diagnose::build(&st, &adv, Origin::Engine)
        }
        // A local look only when no engine runs; one that runs but cannot
        // be used is reported as such, not replaced by a different picture.
        Err(e) if e.code == "engine_not_running" => match api.paths() {
            Some(p) => local_diagnosis(p, &e.message),
            None => diagnose::build(&Value::Null, &Value::Null, Origin::Unavailable(&e.message)),
        },
        Err(e) if e.code == "engine_unreachable" => {
            diagnose::build(&Value::Null, &Value::Null, Origin::Unavailable(&e.message))
        }
        Err(e) => return Err(api_err(e)),
    };
    if read_only {
        strip_refused(&mut d);
    }
    Ok(d)
}

/// Whether a read-only server offers the tool `name`.
pub fn offered_read_only(name: &str) -> bool {
    tool_list()
        .iter()
        .any(|t| t.name == name && allowed(t, true))
}

/// Drop suggested tool calls a read-only server refuses, keeping the CLI
/// commands (the user can run those).
pub fn strip_refused(d: &mut Value) {
    let keep = |c: &Value| c["name"].as_str().is_some_and(offered_read_only);
    for r in d["recommendations"].as_array_mut().into_iter().flatten() {
        if !r["next_call"].is_null() && !keep(&r["next_call"]) {
            r["next_call"] = Value::Null;
        }
    }
    for f in d["findings"]["top"].as_array_mut().into_iter().flatten() {
        if let Some(calls) = f["next_calls"].as_array_mut() {
            calls.retain(keep);
        }
    }
}

pub fn local_diagnosis(paths: &Paths, why: &str) -> Value {
    let engine = crate::commands::local_engine(paths, true);
    let st = serde_json::to_value(engine.snapshot_state()).unwrap_or(Value::Null);
    let adv = serde_json::to_value(engine.advice()).unwrap_or(Value::Null);
    diagnose::build(&st, &adv, Origin::Local(why))
}

/// The secrets map, usable even if a thread panicked holding it.
pub fn lock_secrets(
    m: &Mutex<HashMap<u64, String>>,
) -> std::sync::MutexGuard<'_, HashMap<u64, String>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Ctx<'a> {
    pub api: &'a dyn Api,
    pub plans: &'a mut Plans,
    pub read_only: bool,
    /// Release secrets by ticket, from the lease replies. Never shown.
    pub secrets: &'a Mutex<HashMap<u64, String>>,
}

/// The core's consumer groups, short, for people and models.
pub fn group_brief(g: &Value) -> Value {
    json!({
        "key": g["key"],
        "kind": g["kind"],
        "name": g["name"],
        "cpu_pct": g["cpu_pct"],
        "cpu_share_pct": g["cpu_share_pct"],
        "memory": human_bytes(g["mem_bytes"].as_u64().unwrap_or(0)),
        "mem_share_pct": g["mem_share_pct"],
        "processes": g["procs"],
        "protected": g["protected"],
        "actions": g["actions"],
    })
}

/// CPU and memory in the core's words, with the top groups for each.
pub fn consumers_brief(snap: &Value, n: usize) -> Value {
    let top = |k: &str| -> Vec<Value> {
        snap["consumers"][k]
            .as_array()
            .into_iter()
            .flatten()
            .take(n)
            .map(group_brief)
            .collect()
    };
    json!({
        "cpu": { "headline": snap["cpu_summary"]["headline"], "detail": snap["cpu_summary"]["detail"], "top": top("by_cpu") },
        "memory": { "headline": snap["mem_summary"]["headline"], "detail": snap["mem_summary"]["detail"], "top": top("by_mem") },
        "memory_note": snap["consumers"]["memory_note"],
    })
}

pub fn call_tool(cx: &mut Ctx, name: &str, a: &Map<String, Value>) -> ToolResult {
    let api = cx.api;
    let get = |p: &str| api.call("GET", p, None).map_err(api_err);
    let post = |p: &str, b: Value| api.call("POST", p, Some(&b)).map_err(api_err);
    let arg = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
    let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    match name {
        "diagnose" => diagnosis(api, cx.read_only),
        "get_status" => {
            let st = get("/v1/state")?;
            let snap = &st["snapshot"];
            Ok(json!({
                "consumers": consumers_brief(snap, 5),
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
        "prune_cache" => {
            if !flag("execute") {
                let st = get("/v1/state")?;
                let key = arg("key");
                let cats = st["diskmap"]["categories"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let Some(c) = cats.iter().find(|c| c["key"] == key) else {
                    let known: Vec<&str> = cats.iter().filter_map(|c| c["key"].as_str()).collect();
                    return Err(format!(
                        "no cache with key {key}; known keys: {}",
                        if known.is_empty() {
                            "none yet (the engine has not mapped the disk)".into()
                        } else {
                            known.join(", ")
                        }
                    ));
                };
                if c["prunable"] == false {
                    return Err(format!("{key} has no prune command Mulchie can run"));
                }
                return Ok(json!({
                    "preview": true,
                    "key": key,
                    "name": c["name"],
                    "path": c["path"],
                    "size": human_bytes(c["bytes"].as_u64().unwrap_or(0)),
                    "what_pruning_does": c["hint"],
                    "next": "Nothing was done. Show this to the user; if they agree, call prune_cache again with execute=true.",
                }));
            }
            post("/v1/prune", json!({ "key": arg("key") }))
        }
        "plan_process_action" => {
            let mut body = Map::new();
            match (a.get("pid"), a.get("group")) {
                (Some(pid), None) => {
                    if a.contains_key("action") {
                        return Err(
                            "with pid, use signal (term, kill, stop, cont); action is for group"
                                .into(),
                        );
                    }
                    body.insert("pid".into(), pid.clone());
                    body.insert(
                        "signal".into(),
                        a.get("signal").cloned().unwrap_or(json!("term")),
                    );
                    body.insert("tree".into(), json!(flag("tree")));
                }
                (None, Some(g)) => {
                    if a.contains_key("signal") || a.contains_key("tree") {
                        return Err("with group, use action; signal and tree are for pid".into());
                    }
                    let Some(action) = a.get("action") else {
                        return Err("a group needs an action".into());
                    };
                    body.insert("group".into(), g.clone());
                    body.insert("action".into(), action.clone());
                }
                _ => return Err("pass either pid or group".into()),
            }
            // Groups holding this server or its ancestors (the agent's app,
            // its terminal) are refused by the core.
            body.insert("caller_pid".into(), json!(std::process::id()));
            let plan = post("/v1/process/plan", Value::Object(body))?;
            Ok(present_process(cx.plans, plan, cx.read_only))
        }
        "list_findings" => {
            let mut q = Vec::new();
            if let Some(s) = a.get("severity").and_then(Value::as_str) {
                q.push(format!("severity={s}"));
            }
            if let Some(k) = a.get("kind").and_then(Value::as_str) {
                if !k.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    return Err("kind is a snake_case finding kind, e.g. runaway_cpu".into());
                }
                q.push(format!("kind={k}"));
            }
            if flag("include_dismissed") {
                q.push("dismissed=1".into());
            }
            let path = if q.is_empty() {
                "/v1/findings".to_string()
            } else {
                format!("/v1/findings?{}", q.join("&"))
            };
            let mut v = get(&path)?;
            v["note"] = json!(crate::diagnose::FINDINGS_NOTE);
            Ok(v)
        }
        "dismiss_finding" => {
            if flag("undo") {
                post("/v1/findings/undismiss", json!({ "id": arg("id") }))
            } else {
                let id = a.get("id").and_then(Value::as_str).unwrap_or("");
                if !flag("force") {
                    let known = get("/v1/findings?dismissed=1")?;
                    crate::observe::check_finding_id(id, &known)?;
                }
                let mut b = json!({ "id": id });
                if let Some(d) = a.get("days") {
                    b["days"] = d.clone();
                }
                post("/v1/findings/dismiss", b)
            }
        }
        "inspect_process" => {
            let pid = a.get("pid").and_then(Value::as_u64).unwrap_or(0);
            get(&format!("/v1/process/{pid}"))
        }
        // A TTL, never this server's pid: `mulchie mcp` lives as long as the
        // agent session, so a forgotten release would hold the slot for hours.
        // The release secret stays in this server: the model never sees it.
        "acquire_build_slot" => {
            let mut body = json!({
                "label": a.get("label").and_then(Value::as_str).unwrap_or("mcp agent"),
                "ticket": arg("ticket"),
                "ttl_secs": a.get("ttl_secs").and_then(Value::as_u64).unwrap_or(900),
            });
            // Renewing or polling a ticket sends its secret, which the
            // engine may require on any call that names a ticket.
            let sent = a.get("ticket").and_then(Value::as_u64);
            if let Some(sec) = sent.and_then(|t| lock_secrets(cx.secrets).get(&t).cloned()) {
                body["secret"] = json!(sec);
            }
            let mut r = post("/v1/lease", body)?;
            let mut secrets = lock_secrets(cx.secrets);
            if let Some(o) = r.as_object_mut() {
                let got = o.get("ticket").and_then(Value::as_u64);
                if let (Some(Value::String(secret)), Some(t)) = (o.remove("release_secret"), got) {
                    secrets.insert(t, secret);
                }
                // The engine no longer knows the ticket sent (it expired and
                // a new one was made): its secret is useless now.
                if let Some(old) = sent.filter(|s| Some(*s) != got) {
                    secrets.remove(&old);
                }
            }
            Ok(r)
        }
        "release_build_slot" => {
            let ticket = a.get("ticket").and_then(Value::as_u64).unwrap_or(0);
            let mut b = json!({ "ticket": ticket });
            if let Some(s) = lock_secrets(cx.secrets).get(&ticket) {
                b["secret"] = json!(s);
            }
            let r = api.call("POST", "/v1/release", Some(&b));
            // Forget the secret once the engine has answered about the
            // ticket, released or unknown; keep it only when the engine
            // could not be reached, so the release on exit can retry.
            if !matches!(&r, Err(e) if e.status == 0) {
                lock_secrets(cx.secrets).remove(&ticket);
            }
            r.map_err(api_err)
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// The text for a successful result.
pub fn text_of(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
