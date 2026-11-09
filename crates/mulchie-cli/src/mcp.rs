use mulchie_core::api::Client;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const INSTRUCTIONS: &str = "Mulchie monitors this machine for multi-agent workloads. Call can_i_build before starting heavy builds or test suites, and acquire_build_slot / release_build_slot around them when several agents share the machine. Use top_processes and list_workspaces to find what is using CPU, memory or disk I/O, and disk_breakdown to see where disk space went. Destructive tools only act on recognised build caches, idle git worktrees (with a patch backup and the branch kept) and processes owned by the current user.";

fn tools() -> Value {
    let obj = |props: Value, req: &[&str]| json!({ "type": "object", "properties": props, "required": req });
    json!([
        { "name": "get_status", "description": "Machine health: disk, memory, CPU, load, swap, agent and build counts, build slots, disk-fill forecast.", "inputSchema": obj(json!({}), &[]) },
        { "name": "can_i_build", "description": "Whether it is a good moment to start heavy work (builds, test suites), how many build slots are free and the suggested -j job count.", "inputSchema": obj(json!({}), &[]) },
        { "name": "top_processes", "description": "Processes ranked by CPU, memory or disk I/O, with pid, kind (agent/build/runtime), workspace, niceness, state and command line.",
          "inputSchema": obj(json!({ "sort": { "type": "string", "enum": ["cpu", "mem", "io"] }, "limit": { "type": "integer" }, "workspace": { "type": "string", "description": "only processes whose group contains this text" } }), &[]) },
        { "name": "list_workspaces", "description": "Every repo or worktree with running processes: aggregated CPU, memory, I/O, agents, builds, paused/throttled state and orphan warnings.",
          "inputSchema": obj(json!({ "include_processes": { "type": "boolean" } }), &[]) },
        { "name": "get_history", "description": "Time series sampled every 3s for the last 30 minutes: cpu, load, memory, swap, disk free, I/O and top workspaces.",
          "inputSchema": obj(json!({ "minutes": { "type": "integer" } }), &[]) },
        { "name": "disk_breakdown", "description": "Where disk space went: toolchain and package caches, model stores, and every git worktree with size, idle days, dirty files and a removal verdict.", "inputSchema": obj(json!({}), &[]) },
        { "name": "reclaim_candidates", "description": "Build caches (Rust target, .next, node_modules if enabled, Xcode DerivedData) with size, last use, in-use and stale flags.", "inputSchema": obj(json!({}), &[]) },
        { "name": "clean_caches", "description": "Delete recognised build caches. Pass paths from reclaim_candidates, or stale=true for every stale one. Refuses caches in projects with running agents or builds.",
          "inputSchema": obj(json!({ "paths": { "type": "array", "items": { "type": "string" } }, "stale": { "type": "boolean" } }), &[]) },
        { "name": "prune_cache", "description": "Run a package manager's own prune command for one cache key from disk_breakdown (npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators).",
          "inputSchema": obj(json!({ "key": { "type": "string" } }), &["key"]) },
        { "name": "remove_worktrees", "description": "Remove git worktrees by path. Branches are kept; a detached HEAD gets a backup/<name> branch. Worktrees with uncommitted changes are refused unless backup=true, which saves a patch first.",
          "inputSchema": obj(json!({ "paths": { "type": "array", "items": { "type": "string" } }, "backup": { "type": "boolean" } }), &["paths"]) },
        { "name": "sweep_worktrees", "description": "Remove every idle agent worktree past the configured idle window. dry_run=true lists them without removing.",
          "inputSchema": obj(json!({ "dry_run": { "type": "boolean" } }), &[]) },
        { "name": "workspace_action", "description": "Act on all of a workspace's processes: pause (SIGSTOP), resume, deprioritize (renice), stop_builds (SIGTERM builds only) or kill_all (SIGTERM everything).",
          "inputSchema": obj(json!({ "path": { "type": "string" }, "action": { "type": "string", "enum": ["pause", "resume", "deprioritize", "stop_builds", "kill_all"] } }), &["path", "action"]) },
        { "name": "process_action", "description": "Signal or renice one process owned by the current user.",
          "inputSchema": obj(json!({ "pid": { "type": "integer" }, "action": { "type": "string", "enum": ["term", "kill", "stop", "cont", "renice"] }, "nice": { "type": "integer" } }), &["pid", "action"]) },
        { "name": "acquire_build_slot", "description": "Join the machine-wide build queue. Poll with the returned ticket until granted, then run with the returned job count. Always release afterwards.",
          "inputSchema": obj(json!({ "label": { "type": "string" }, "ticket": { "type": "integer" } }), &[]) },
        { "name": "release_build_slot", "description": "Release a build slot ticket.", "inputSchema": obj(json!({ "ticket": { "type": "integer" } }), &["ticket"]) },
    ])
}

fn call_tool(name: &str, a: &Value) -> Result<Value, String> {
    let c = Client::connect()
        .ok_or("Mulchie is not running. Start Mulchie.app or run `mulchie serve`.")?;
    let get = |p: &str| c.call("GET", p, None);
    let post = |p: &str, b: Value| c.call("POST", p, Some(&b));
    match name {
        "get_status" => {
            let st = get("/v1/state")?;
            Ok(json!({
                "health": st["health"],
                "low_disk": st["low_disk"],
                "forecast": st["forecast"],
                "slots": st["slots"],
                "throttled_builds": st["throttled"],
                "machine": {
                    "cpu_pct": st["snapshot"]["cpu_pct"], "cpu_count": st["snapshot"]["cpu_count"], "load": st["snapshot"]["load"],
                    "mem_total": st["snapshot"]["mem_total"], "mem_available": st["snapshot"]["mem_available"],
                    "swap_used": st["snapshot"]["swap_used"], "disk_total": st["snapshot"]["disk_total"], "disk_free": st["snapshot"]["disk_free"],
                    "read_bps": st["snapshot"]["read_bps"], "write_bps": st["snapshot"]["write_bps"],
                    "processes": st["snapshot"]["proc_count"], "agents": st["snapshot"]["agent_count"], "builds": st["snapshot"]["build_count"],
                },
                "top_groups": st["snapshot"]["groups"].as_array().map(|g| g.iter().take(10).cloned().collect::<Vec<_>>()),
                "recent_actions": st["actions"].as_array().map(|g| g.iter().take(10).cloned().collect::<Vec<_>>()),
            }))
        }
        "can_i_build" => get("/v1/advice"),
        "top_processes" => {
            let st = get("/v1/state")?;
            let sort = a["sort"].as_str().unwrap_or("cpu");
            let limit = a["limit"].as_u64().unwrap_or(20) as usize;
            let filter = a["workspace"].as_str().unwrap_or("");
            let mut procs: Vec<Value> = st["snapshot"]["top"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            procs.retain(|p| {
                filter.is_empty() || p["group"].as_str().unwrap_or("").contains(filter)
            });
            let key = |p: &Value| match sort {
                "mem" => p["mem_bytes"].as_f64().unwrap_or(0.0),
                "io" => {
                    p["read_bps"].as_f64().unwrap_or(0.0) + p["write_bps"].as_f64().unwrap_or(0.0)
                }
                _ => p["cpu_pct"].as_f64().unwrap_or(0.0),
            };
            procs.sort_by(|x, y| key(y).total_cmp(&key(x)));
            procs.truncate(limit);
            Ok(Value::Array(procs))
        }
        "list_workspaces" => {
            let st = get("/v1/state")?;
            let mut ws = st["snapshot"]["workspaces"].clone();
            if !a["include_processes"].as_bool().unwrap_or(false) {
                if let Some(arr) = ws.as_array_mut() {
                    for w in arr {
                        if let Some(o) = w.as_object_mut() {
                            o.remove("procs");
                        }
                    }
                }
            }
            Ok(ws)
        }
        "get_history" => {
            let h = get("/v1/history")?;
            let mins = a["minutes"].as_u64().unwrap_or(10);
            let n = (mins * 20) as usize;
            let arr = h.as_array().cloned().unwrap_or_default();
            let start = arr.len().saturating_sub(n);
            let step = ((arr.len() - start) / 120).max(1);
            Ok(Value::Array(
                arr[start..].iter().step_by(step).cloned().collect(),
            ))
        }
        "disk_breakdown" => {
            let st = get("/v1/state")?;
            Ok(
                json!({ "disk_free": st["snapshot"]["disk_free"], "disk_total": st["snapshot"]["disk_total"], "forecast": st["forecast"], "diskmap": st["diskmap"], "scanning": st["scanning"] }),
            )
        }
        "reclaim_candidates" => Ok(get("/v1/state")?["scan"].clone()),
        "clean_caches" => {
            if a["stale"].as_bool().unwrap_or(false) {
                // Plan, then execute exactly the ready items of that plan.
                let plan = get("/v1/clean/plan")?;
                post("/v1/clean", json!({ "items": ready_requests(&plan) }))
            } else {
                post("/v1/clean", json!({ "paths": a["paths"] }))
            }
        }
        "prune_cache" => post("/v1/prune", json!({ "key": a["key"] })),
        "remove_worktrees" => {
            // The plan reads each HEAD now; removal fails if it moves after.
            let plan = post(
                "/v1/worktree/plan",
                json!({ "paths": a["paths"], "allow_dirty": a["backup"].as_bool().unwrap_or(false) }),
            )?;
            let skipped: Vec<Value> = plan["items"]
                .as_array()
                .map(|v| v.iter().filter(|i| i["ready"] != true).cloned().collect())
                .unwrap_or_default();
            let done = post(
                "/v1/worktree/remove",
                json!({ "items": ready_requests(&plan) }),
            )?;
            Ok(json!({ "results": done, "not_ready": skipped }))
        }
        "sweep_worktrees" => {
            let plan = get("/v1/sweep")?;
            if a["dry_run"].as_bool().unwrap_or(false) {
                Ok(plan)
            } else {
                post("/v1/sweep", json!({ "items": ready_requests(&plan) }))
            }
        }
        "workspace_action" => post(
            "/v1/workspace",
            json!({ "path": a["path"], "action": a["action"] }),
        ),
        "process_action" => {
            let mut b = json!({ "pid": a["pid"], "action": a["action"] });
            if a["action"] == "renice" {
                b["nice"] = json!(a["nice"].as_i64().unwrap_or(10));
            }
            post("/v1/process", b)
        }
        "acquire_build_slot" => post(
            "/v1/lease",
            json!({ "label": a["label"].as_str().unwrap_or("mcp agent"), "pid": std::process::id(), "ticket": a["ticket"] }),
        ),
        "release_build_slot" => post("/v1/release", json!({ "ticket": a["ticket"] })),
        other => Err(format!("unknown tool {other}")),
    }
}

/// The requests of a plan's ready items.
fn ready_requests(plan: &Value) -> Vec<Value> {
    plan["items"]
        .as_array()
        .map(|v| {
            v.iter()
                .filter(|i| i["ready"] == true)
                .map(|i| i["request"].clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn serve() -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let e = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } });
            let _ = writeln!(out, "{e}");
            let _ = out.flush();
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or("");
        let params = &msg["params"];
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mulchie", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match call_tool(name, &args) {
                    Ok(v) => {
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] })
                    }
                    Err(e) => {
                        json!({ "content": [{ "type": "text", "text": e }], "isError": true })
                    }
                })
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        let reply = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": m } })
            }
        };
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
    0
}
