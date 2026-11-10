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
use crate::mcp_tools::{self as tools, Ctx, Plans, Tool};
use mulchie_core::api::{ApiError, Client};
use mulchie_core::Paths;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
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
const INTERNAL_ERROR: i64 = -32603;
const UNSUPPORTED_VERSION: i64 = -32022;

/// Unknown resource (legacy clients; 2026-07-28 uses -32602).
const RESOURCE_NOT_FOUND: i64 = -32002;

/// Set to 1 (or true) to start the server read-only.
pub const READ_ONLY_ENV: &str = "MULCHIE_MCP_READ_ONLY";

const INSTRUCTIONS: &str =
    "Mulchie watches this machine while several coding agents share it: CPU, \
memory, disk, build caches, git worktrees, processes and a machine-wide build queue. \
Start with the diagnose tool: it explains CPU and memory in words with the apps and workspaces \
using the most, health and why, reclaimable space with sizes, watch findings, engine problems, and \
recommendations, each with a reason, risk and the exact next tool call. \
Findings are observations from heuristics (a leftover, runaway, stopped or exposed process, an \
executable in an odd place, a setup problem), never verdicts: say what was observed and why, in \
the finding's own words, and never call anything malware. \
Every change is plan-then-execute: plan_cache_cleanup, plan_worktree_removal, plan_worktree_sweep \
and plan_process_action change nothing and return a plan_id with ready items; show the user the \
plan (what, size, what would be backed up, which processes are affected) and call execute_plan \
with that plan_id and only the items the user approved. Nothing outside a plan can be executed, \
and each item is re-checked just before it runs (processes by their start time). Protected \
processes, including this agent's own app and terminal, are never acted on. Before a worktree is \
removed, uncommitted changes are saved as a patch, ignored non-cache files as a tar.gz archive, \
and a detached HEAD on a backup/<name> branch, under Mulchie's backups directory; execute_plan's \
result lists each backup's path and how to restore. Branches are never deleted. Build caches are \
not backed up: the next build recreates them. prune_cache only previews unless execute=true; show \
the preview and ask first. dismiss_finding hides a finding the user chose to ignore. \
Before heavy builds or test suites, call can_i_build, and when other agents share the machine \
wrap the work in acquire_build_slot / release_build_slot (poll with the ticket until granted, \
renew before ttl_secs passes, release as soon as the work ends).";

const READ_ONLY_NOTE: &str = " This server is read-only (it uses Mulchie's read-only token): it \
can look, plan and use build slots, but cannot delete, signal, prune or dismiss; the user can carry \
out a plan with the mulchie CLI.";

fn server_info() -> Value {
    json!({ "name": "mulchie", "title": "Mulchie", "version": env!("CARGO_PKG_VERSION") })
}

// ------------------------------------------------------------- engine access

/// The engine's HTTP API, as the tools see it.
pub trait Api {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError>;
    /// Mulchie's directories, for a local look when no engine runs.
    fn paths(&self) -> Option<&Paths> {
        None
    }
}

/// The running engine, connected on first use and again after it restarts.
pub struct EngineApi {
    paths: Paths,
    client: Mutex<Option<Client>>,
    /// Connect with the read-only token: the engine itself then refuses
    /// anything but reads, plans and TTL build slots.
    read_only: bool,
}

impl EngineApi {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            client: Mutex::new(None),
            read_only: false,
        }
    }

    pub fn read_only(paths: Paths) -> Self {
        Self {
            read_only: true,
            ..Self::new(paths)
        }
    }
}

impl Api for EngineApi {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let mut guard = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            match conn::connect_as(&self.paths, self.read_only) {
                Conn::Engine(c) => *guard = Some(c),
                Conn::NotRunning => {
                    let msg = format!("Mulchie's engine is not running; {START_HINT}.");
                    return Err(ApiError::new(0, "engine_not_running", msg));
                }
                other => {
                    return Err(ApiError::new(
                        0,
                        "engine_unreachable",
                        format!(
                            "{}; the user can run `mulchie doctor` to see why",
                            other.why_not()
                        ),
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

    fn paths(&self) -> Option<&Paths> {
        Some(&self.paths)
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
    /// Only tools that change nothing (plus build slots).
    pub read_only: bool,
    /// Plans shown by this server process (in memory, lost on restart);
    /// execute_plan needs one.
    pub plans: Plans,
    /// Build-slot release secrets by ticket; never shown to the client.
    secrets: HashMap<u64, String>,
}

fn capabilities() -> Value {
    json!({ "tools": {}, "prompts": {}, "resources": {} })
}

impl<'a> Session<'a> {
    pub fn new(api: &'a dyn Api) -> Self {
        Self {
            api,
            negotiated: None,
            read_only: false,
            plans: Plans::default(),
            secrets: HashMap::new(),
        }
    }

    pub fn read_only(api: &'a dyn Api) -> Self {
        Self {
            read_only: true,
            ..Self::new(api)
        }
    }

    fn instructions(&self) -> String {
        let mut s = INSTRUCTIONS.to_string();
        if self.read_only {
            s.push_str(READ_ONLY_NOTE);
        }
        s
    }

    fn tools(&self) -> Vec<Tool> {
        tools::tool_list()
            .into_iter()
            .filter(|t| tools::allowed(t, self.read_only))
            .collect()
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
            let rid = if id_ok {
                id.unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            return Some(error_reply(rid, rpc_err(INVALID_REQUEST, "missing method")));
        };
        let Some(method) = method.as_str() else {
            let rid = if id_ok {
                id.unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            return Some(error_reply(
                rid,
                rpc_err(INVALID_REQUEST, "method must be a string"),
            ));
        };
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            // A notification gets no reply, even a malformed one.
            id.as_ref()?;
            let rid = if id_ok {
                id.unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            return Some(error_reply(
                rid,
                rpc_err(INVALID_REQUEST, "jsonrpc must be \"2.0\""),
            ));
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
                // Only what this path accepts: listing the legacy versions here
                // would invite a dual-era client to retry them in `_meta`.
                data: Some(
                    json!({ "supported": [MODERN_VERSION], "requested": meta[META_VERSION] }),
                ),
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
                "supportedVersions": [MODERN_VERSION],
                "capabilities": capabilities(),
                "instructions": self.instructions(),
            }));
        }
        self.dispatch(method, params).map_err(|mut e| {
            if e.code == RESOURCE_NOT_FOUND {
                e.code = INVALID_PARAMS;
            }
            e
        })
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
                    "capabilities": capabilities(),
                    "serverInfo": server_info(),
                    "instructions": self.instructions(),
                }))
            }
            "server/discover" => Err(rpc_err(
                INVALID_PARAMS,
                format!("server/discover needs _meta with {META_VERSION} and {META_CAPS}"),
            )),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": self.tools().iter().map(Tool::describe).collect::<Vec<_>>()
            })),
            "tools/call" => {
                let Some(name) = params["name"].as_str() else {
                    return Err(rpc_err(INVALID_PARAMS, "tools/call needs a tool name"));
                };
                let all = tools::tool_list();
                let Some(tool) = all.iter().find(|t| t.name == name) else {
                    return Err(rpc_err(INVALID_PARAMS, format!("Unknown tool: {name}")));
                };
                if !tools::allowed(tool, self.read_only) {
                    return Ok(json!({
                        "content": [{ "type": "text", "text": format!(
                            "{name} is not available: this Mulchie MCP server is read-only (started with --read-only or {READ_ONLY_ENV}=1), so it cannot delete, signal or prune anything. Show the user the plan instead; they can carry it out with the mulchie CLI."
                        ) }],
                        "isError": true,
                    }));
                }
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(m)) => m.clone(),
                    Some(_) => return Err(rpc_err(INVALID_PARAMS, "arguments must be an object")),
                };
                let mut cx = Ctx {
                    api: self.api,
                    plans: &mut self.plans,
                    read_only: self.read_only,
                    secrets: &mut self.secrets,
                };
                let outcome = tools::validate(&tool.schema(), &args)
                    .and_then(|()| tools::call_tool(&mut cx, name, &args));
                Ok(match outcome {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": tools::text_of(&v) }],
                        "isError": false,
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": e }],
                        "isError": true,
                    }),
                })
            }
            "prompts/list" => Ok(json!({ "prompts": prompt_list() })),
            "prompts/get" => {
                let name = params["name"].as_str().unwrap_or("");
                prompt(name, self.read_only)
                    .ok_or_else(|| rpc_err(INVALID_PARAMS, format!("Unknown prompt: {name}")))
            }
            "resources/list" => Ok(json!({ "resources": resource_list() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => {
                let Some(uri) = params["uri"].as_str() else {
                    return Err(rpc_err(INVALID_PARAMS, "resources/read needs a uri"));
                };
                let text = self.read_resource(uri)?;
                Ok(json!({
                    "contents": [{ "uri": uri, "mimeType": "application/json", "text": text }]
                }))
            }
            _ => Err(rpc_err(
                METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }

    fn read_resource(&self, uri: &str) -> Result<String, RpcError> {
        let failed = |e: String| rpc_err(INTERNAL_ERROR, e);
        let get = |p: &str| {
            self.api
                .call("GET", p, None)
                .map_err(|e| failed(tools::api_err(e)))
        };
        let v = match uri {
            "mulchie://diagnosis" => tools::diagnosis(self.api).map_err(failed)?,
            "mulchie://state" => get("/v1/state")?,
            "mulchie://plan/sweep" => get("/v1/sweep")?,
            "mulchie://actions" => get("/v1/state")?["actions"].clone(),
            "mulchie://findings" => get("/v1/findings")?,
            _ => {
                return Err(RpcError {
                    code: RESOURCE_NOT_FOUND,
                    message: "Resource not found".into(),
                    data: Some(json!({ "uri": uri })),
                })
            }
        };
        Ok(tools::text_of(&v))
    }
}

// ------------------------------------------------------- prompts, resources

fn prompt_list() -> Value {
    json!([
        {
            "name": "whats-going-on",
            "title": "What's going on with my computer?",
            "description": "Diagnose the machine and explain in plain language what is using it and what could be done.",
        },
        {
            "name": "optimize",
            "title": "Optimize my computer",
            "description": "Diagnose, propose a plan, and carry out only the steps the user approves.",
        },
    ])
}

fn prompt(name: &str, read_only: bool) -> Option<Value> {
    let (description, text) = match name {
        "whats-going-on" => (
            "Diagnose the machine and explain it",
            "Call Mulchie's diagnose tool and explain to me, in plain language, what is going on with \
this computer: whether it is healthy and why, what is using CPU, memory and disk, whether builds \
are queued, how much space could be reclaimed, what Mulchie's watch noticed, and any engine \
problems. Use the summaries' own words; findings are observations, not verdicts, so say what was \
seen and why it was flagged, and never call anything malware. Then list the recommendations with \
their expected gain and risk. Do not change anything."
                .to_string(),
        ),
        "optimize" => (
            "Diagnose, plan, then act on approval",
            format!(
                "Help me optimize this computer with Mulchie. First call diagnose and summarise what \
you found. Then, for the recommendations worth doing, make the plans (plan_cache_cleanup, \
plan_worktree_sweep, plan_worktree_removal, plan_process_action) and show me each plan: what \
would be removed or which processes would be affected, its size, and what would be backed up. {} Never act on anything I have not approved, and finish by \
telling me what was freed, what was skipped and where any backups are.",
                if read_only {
                    "This server is read-only, so stop after showing the plans and tell me the mulchie CLI \
commands that would carry them out."
                } else {
                    "Wait for my approval, then call execute_plan with only the items I approved. For \
prune_cache, preview first and ask me before executing."
                }
            ),
        ),
        _ => return None,
    };
    Some(json!({
        "description": description,
        "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
    }))
}

fn resource_list() -> Value {
    let r = |uri: &str, name: &str, title: &str, description: &str| json!({ "uri": uri, "name": name, "title": title, "description": description, "mimeType": "application/json" });
    json!([
        r("mulchie://diagnosis", "diagnosis", "Diagnosis", "The same picture as the diagnose tool."),
        r("mulchie://state", "state", "Engine state", "The engine's full state: snapshot, health, slots, scan, disk map and recent actions."),
        r("mulchie://plan/sweep", "sweep-plan", "Sweep plan", "What a sweep of idle agent worktrees would do now (read only; execute through plan_worktree_sweep and execute_plan)."),
        r("mulchie://actions", "actions", "Recent actions", "Mulchie's recent action journal, newest first."),
        r("mulchie://findings", "findings", "Watch findings", "What the watch noticed (observations, not verdicts), most severe first."),
    ])
}

/// Serve until the reader ends.
#[cfg(test)]
pub fn serve<R: BufRead, W: Write>(reader: R, writer: W, api: &dyn Api) -> std::io::Result<()> {
    serve_session(reader, writer, Session::new(api))
}

pub fn serve_session<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    mut session: Session,
) -> std::io::Result<()> {
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

/// Whether the environment asks for a read-only server.
pub fn read_only_from_env() -> bool {
    std::env::var(READ_ONLY_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn command(paths: &Paths, read_only: bool) -> i32 {
    let read_only = read_only || read_only_from_env();
    let api = if read_only {
        EngineApi::read_only(paths.clone())
    } else {
        EngineApi::new(paths.clone())
    };
    let session = if read_only {
        Session::read_only(&api)
    } else {
        Session::new(&api)
    };
    let stdin = std::io::stdin();
    match serve_session(stdin.lock(), std::io::stdout().lock(), session) {
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
