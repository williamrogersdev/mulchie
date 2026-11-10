use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

/// A fake engine: canned answers per route, and a log of calls.
#[derive(Default)]
struct Fake {
    answers: HashMap<String, Result<Value, ApiError>>,
    calls: RefCell<Vec<(String, String, Option<Value>)>>,
}

impl Fake {
    fn with(mut self, route: &str, v: Value) -> Self {
        self.answers.insert(route.into(), Ok(v));
        self
    }

    fn failing(mut self, route: &str, e: ApiError) -> Self {
        self.answers.insert(route.into(), Err(e));
        self
    }

    fn posted(&self, path: &str) -> Vec<Value> {
        self.calls
            .borrow()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == path)
            .map(|(_, _, b)| b.clone().unwrap_or(Value::Null))
            .collect()
    }
}

impl Api for Fake {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        self.calls
            .borrow_mut()
            .push((method.into(), path.into(), body.cloned()));
        self.answers
            .get(&format!("{method} {path}"))
            .cloned()
            .unwrap_or_else(|| Err(ApiError::new(404, "not_found", format!("no route {path}"))))
    }
}

/// Run lines through `serve` over in-memory streams; the replies.
fn exchange(api: &dyn Api, lines: &[Value]) -> Vec<Value> {
    let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let mut out = Vec::new();
    serve(input.as_bytes(), &mut out, api).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn one(api: &dyn Api, msg: Value) -> Value {
    let mut r = exchange(api, &[msg]);
    assert_eq!(r.len(), 1, "{r:?}");
    r.remove(0)
}

fn req(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn call(id: u64, name: &str, args: Value) -> Value {
    req(id, "tools/call", json!({ "name": name, "arguments": args }))
}

fn text(r: &Value) -> String {
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

fn modern_meta() -> Value {
    json!({ "_meta": {
        "io.modelcontextprotocol/protocolVersion": MODERN_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "t", "version": "1" }
    }})
}

#[test]
fn initialize_negotiates() {
    let f = Fake::default();
    for v in LEGACY_VERSIONS {
        let r = one(
            &f,
            req(
                1,
                "initialize",
                json!({ "protocolVersion": v, "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
            ),
        );
        assert_eq!(r["result"]["protocolVersion"], *v);
        assert_eq!(r["result"]["serverInfo"]["name"], "mulchie");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"]["instructions"].is_string());
    }
    // Unknown (future or ancient) versions get our latest legacy one.
    for v in ["1999-01-01", "2030-01-01", MODERN_VERSION] {
        let r = one(
            &f,
            req(
                1,
                "initialize",
                json!({ "protocolVersion": v, "capabilities": {} }),
            ),
        );
        assert_eq!(r["result"]["protocolVersion"], LEGACY_VERSIONS[0]);
    }
    let r = one(&f, req(1, "initialize", json!({})));
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
}

#[test]
fn full_legacy_handshake() {
    let f = Fake::default();
    let r = exchange(
        &f,
        &[
            req(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-06-18", "capabilities": {} }),
            ),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            req(2, "ping", json!({})),
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 9 } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/something_new" }),
            req(3, "tools/list", json!({})),
        ],
    );
    assert_eq!(r.len(), 3, "notifications must get no reply: {r:?}");
    assert_eq!(r[0]["id"], 1);
    assert_eq!(r[1], json!({ "jsonrpc": "2.0", "id": 2, "result": {} }));
    assert_eq!(r[2]["id"], 3);
    // Legacy results carry no resultType.
    assert!(r[2]["result"].get("resultType").is_none());
}

fn tool_names(r: &Value) -> Vec<String> {
    r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn tools_have_titles_and_annotations() {
    let f = Fake::default();
    let r = one(&f, req(1, "tools/list", json!({})));
    let tools = r["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 25);
    assert_eq!(tools[0]["name"], "diagnose", "diagnose comes first");
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert!(t["title"].is_string(), "{name}");
        assert!(t["description"].as_str().unwrap().len() > 20, "{name}");
        assert_eq!(t["inputSchema"]["type"], "object", "{name}");
        let a = &t["annotations"];
        for k in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(a[k].is_boolean(), "{name} {k}");
        }
        // Read-only tools are never destructive.
        if a["readOnlyHint"] == true {
            assert_eq!(a["destructiveHint"], false, "{name}");
        }
    }
    let ann = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["annotations"].clone();
    let desc = |n: &str| {
        tools.iter().find(|t| t["name"] == n).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for n in ["execute_plan", "prune_cache"] {
        assert_eq!(ann(n)["destructiveHint"], true, "{n}");
        assert_eq!(ann(n)["readOnlyHint"], false, "{n}");
    }
    // Hiding a finding changes nothing on the machine.
    assert_eq!(ann("dismiss_finding")["destructiveHint"], false);
    assert_eq!(ann("dismiss_finding")["readOnlyHint"], false);
    for n in [
        "diagnose",
        "get_status",
        "can_i_build",
        "top_processes",
        "disk_breakdown",
        "plan_cache_cleanup",
        "plan_worktree_removal",
        "plan_worktree_sweep",
        "plan_process_action",
        "list_findings",
        "inspect_process",
        "list_sessions",
        "get_session",
        "limits_status",
        "list_updates",
        "check_command",
    ] {
        assert_eq!(ann(n)["readOnlyHint"], true, "{n}");
        assert_eq!(ann(n)["openWorldHint"], false, "{n}");
    }
    assert_eq!(ann("check_updates")["readOnlyHint"], false);
    assert_eq!(ann("check_updates")["destructiveHint"], false);
    assert_eq!(ann("check_updates")["openWorldHint"], true);
    // No one-step deletion or signal is offered.
    let names = tool_names(&r);
    for gone in [
        "clean_caches",
        "remove_worktrees",
        "sweep_worktrees",
        "workspace_action",
        "process_action",
    ] {
        assert!(!names.contains(&gone.to_string()), "{gone}");
    }
    for n in [
        "plan_cache_cleanup",
        "plan_worktree_removal",
        "plan_worktree_sweep",
    ] {
        let d = desc(n);
        assert!(
            d.contains("Changes nothing") && d.contains("execute_plan"),
            "{n}: {d}"
        );
    }
    let d = desc("list_findings");
    assert!(!d.to_lowercase().contains("malware detected"), "{d}");
    let d = desc("execute_plan");
    assert!(
        d.contains("approved") && d.contains("re-checked") && d.contains("restore"),
        "{d}"
    );
    assert_eq!(ann("acquire_build_slot")["destructiveHint"], false);
    let d = desc("acquire_build_slot");
    assert!(d.contains("renew") && d.contains("ttl_secs") && d.contains("release_build_slot"));
}

#[test]
fn read_only_mode_hides_and_refuses_destructive_tools() {
    let f = Fake::default()
        .with("POST /v1/prune", json!({ "ok": true }))
        .with("GET /v1/sweep", plan_with(&["/w/a"], &[]))
        .with("POST /v1/lease", json!({ "granted": true, "ticket": 1 }));
    let mut s = Session::read_only(&f);
    let r = s.handle(req(1, "tools/list", json!({}))).unwrap();
    let names = tool_names(&r);
    assert_eq!(names.len(), 21, "{names:?}");
    for n in [
        "execute_plan",
        "prune_cache",
        "dismiss_finding",
        "check_updates",
    ] {
        assert!(!names.contains(&n.to_string()), "{n}");
    }
    for n in [
        "diagnose",
        "plan_worktree_sweep",
        "plan_process_action",
        "list_findings",
        "inspect_process",
        "acquire_build_slot",
        "release_build_slot",
    ] {
        assert!(names.contains(&n.to_string()), "{n}");
    }
    let init = s
        .handle(req(
            2,
            "initialize",
            json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
        ))
        .unwrap();
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("read-only"));

    // Called anyway: refused before reaching the engine.
    let r = s
        .handle(call(3, "prune_cache", json!({ "key": "npm" })))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("read-only"), "{}", text(&r));
    let r = s
        .handle(call(
            4,
            "execute_plan",
            json!({ "plan_id": "x", "items": ["/w/a"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    // Plans still work, but hand out no plan_id.
    let r = s.handle(call(5, "plan_worktree_sweep", json!({}))).unwrap();
    let v: Value = serde_json::from_str(&text(&r)).unwrap();
    assert!(v.get("plan_id").is_none(), "{v}");
    assert_eq!(v["ready"][0]["target"], "/w/a");
    // Build slots still work.
    let r = s.handle(call(6, "acquire_build_slot", json!({}))).unwrap();
    assert_eq!(r["result"]["isError"], false);
    assert!(f.posted("/v1/prune").is_empty());
    assert!(f.posted("/v1/sweep").is_empty());
}

#[test]
fn jsonrpc_errors() {
    let f = Fake::default();
    let mut out = Vec::new();
    serve("{not json\n".as_bytes(), &mut out, &f).unwrap();
    let r: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r["error"]["code"], PARSE_ERROR);
    assert_eq!(r["id"], Value::Null);

    assert_eq!(one(&f, json!([1, 2]))["error"]["code"], INVALID_REQUEST);
    assert_eq!(one(&f, json!("x"))["error"]["code"], INVALID_REQUEST);
    let r = one(&f, json!({ "jsonrpc": "2.0", "id": 4 }));
    assert_eq!(
        (r["error"]["code"].clone(), r["id"].clone()),
        (json!(INVALID_REQUEST), json!(4))
    );
    let r = one(&f, json!({ "jsonrpc": "1.0", "id": 5, "method": "ping" }));
    assert_eq!(r["error"]["code"], INVALID_REQUEST);
    let r = one(
        &f,
        json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }),
    );
    assert_eq!(r["error"]["code"], INVALID_REQUEST);
    let r = one(
        &f,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "ping", "params": [1] }),
    );
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
    let r = one(&f, req(7, "sampling/createMessage", json!({})));
    assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
    let r = one(&f, call(8, "no_such_tool", json!({})));
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
    assert!(r["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no_such_tool"));
    let r = one(&f, req(9, "tools/call", json!({ "arguments": {} })));
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
    let r = one(
        &f,
        req(
            10,
            "tools/call",
            json!({ "name": "get_status", "arguments": [1] }),
        ),
    );
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
    // String ids are echoed.
    let r = one(
        &f,
        json!({ "jsonrpc": "2.0", "id": "abc", "method": "ping" }),
    );
    assert_eq!(r["id"], "abc");
    // Responses from the client are ignored.
    assert!(exchange(&f, &[json!({ "jsonrpc": "2.0", "id": 11, "result": {} })]).is_empty());
    // Blank lines are skipped.
    let mut out = Vec::new();
    serve("\n\r\n".as_bytes(), &mut out, &f).unwrap();
    assert!(out.is_empty());
}

#[test]
fn invalid_arguments_are_tool_errors() {
    let f = Fake::default();
    for (name, args) in [
        ("top_processes", json!({ "sort": "disk" })),
        ("top_processes", json!({ "limit": 0 })),
        ("top_processes", json!({ "limit": "5" })),
        ("get_status", json!({ "verbose": true })),
        ("prune_cache", json!({})),
        ("plan_worktree_removal", json!({ "paths": [] })),
        ("plan_worktree_removal", json!({ "paths": [3] })),
        ("plan_worktree_removal", json!({})),
        ("execute_plan", json!({ "plan_id": "x", "items": [] })),
        ("execute_plan", json!({ "items": ["/a"] })),
        ("acquire_build_slot", json!({ "ttl_secs": 0 })),
        ("acquire_build_slot", json!({ "ttl_secs": 100000 })),
        ("plan_process_action", json!({})),
        ("plan_process_action", json!({ "pid": 1 })),
        (
            "plan_process_action",
            json!({ "pid": 42, "action": "quit" }),
        ),
        ("plan_process_action", json!({ "pid": 42, "signal": "hup" })),
        ("plan_process_action", json!({ "group": "app:/A.app" })),
        (
            "plan_process_action",
            json!({ "group": "app:/A.app", "action": "explode" }),
        ),
        (
            "plan_process_action",
            json!({ "group": "app:/A.app", "action": "quit", "tree": true }),
        ),
        (
            "plan_process_action",
            json!({ "pid": 42, "group": "app:/A.app" }),
        ),
        ("list_findings", json!({ "severity": "dire" })),
        ("list_findings", json!({ "kind": "Runaway CPU&x=1" })),
        ("dismiss_finding", json!({})),
        ("dismiss_finding", json!({ "id": "f", "days": 0 })),
        ("inspect_process", json!({})),
        ("inspect_process", json!({ "pid": "x" })),
        ("plan_cache_cleanup", json!({})),
        (
            "plan_cache_cleanup",
            json!({ "stale": true, "paths": ["/x"] }),
        ),
        ("plan_cache_cleanup", json!({ "dry_run": true })),
    ] {
        let r = one(&f, call(1, name, args.clone()));
        assert_eq!(r["result"]["isError"], true, "{name} {args}: {r}");
        assert!(r.get("error").is_none());
    }
    // Nothing reached the engine.
    assert!(f.calls.borrow().iter().all(|(m, _, _)| m != "POST"));
}

#[test]
fn engine_errors_are_tool_errors() {
    let f = Fake::default().failing(
        "GET /v1/advice",
        ApiError::new(
            0,
            "engine_not_running",
            "Mulchie's engine is not running; start it",
        ),
    );
    let r = one(&f, call(1, "can_i_build", json!({})));
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("not running"));
    let f = Fake::default().failing(
        "POST /v1/lease",
        ApiError::new(400, "ttl_out_of_range", "ttl_secs 0 is out of range"),
    );
    let r = one(&f, call(1, "acquire_build_slot", json!({})));
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("ttl_out_of_range"));
}

fn plan_with(ready: &[&str], not_ready: &[&str]) -> Value {
    let mut items = Vec::new();
    for p in ready {
        items.push(json!({ "ready": true, "target": p, "verdict": "ok", "reason": null,
            "request": { "action": "remove_worktree", "path": p, "expected_head": "abc", "allow_dirty": true, "require_idle": true } }));
    }
    for p in not_ready {
        items.push(json!({ "ready": false, "target": p, "verdict": "busy", "reason": "busy",
            "request": { "action": "remove_worktree", "path": p, "expected_head": "", "allow_dirty": true, "require_idle": true } }));
    }
    json!({ "created_unix": 1, "items": items, "ready_bytes": 0 })
}

fn result(r: &Value) -> Value {
    assert_eq!(r["result"]["isError"], false, "{r}");
    serde_json::from_str(&text(r)).unwrap()
}

#[test]
fn sweep_plans_then_executes_only_approved_items() {
    let plan = plan_with(&["/w/a", "/w/b"], &["/w/c"]);
    let f = Fake::default()
        .with("GET /v1/sweep", plan.clone())
        .with(
            "POST /v1/sweep",
            json!([{ "action": "remove_worktree", "target": "/w/a", "status": "done", "freed_bytes": 2048,
                "backups": [{ "kind": "patch", "location": "/b/a.patch" }, { "kind": "branch", "location": "backup/a" }] }]),
        );
    let mut s = Session::new(&f);
    // The plan changes nothing.
    let shown = result(&s.handle(call(1, "plan_worktree_sweep", json!({}))).unwrap());
    assert!(f.posted("/v1/sweep").is_empty());
    let id = shown["plan_id"].as_str().unwrap().to_string();
    assert_eq!(shown["ready_count"], 2);
    assert_eq!(shown["ready"][0]["target"], "/w/a");
    assert_eq!(shown["not_ready"][0]["target"], "/w/c");
    assert!(shown["next"].as_str().unwrap().contains("execute_plan"));

    // Items outside the plan, or not ready in it, are refused.
    for bad in ["/w/zzz", "/w/c"] {
        let r = s
            .handle(call(
                2,
                "execute_plan",
                json!({ "plan_id": id, "items": [bad] }),
            ))
            .unwrap();
        assert_eq!(r["result"]["isError"], true, "{bad}");
    }
    assert!(f.posted("/v1/sweep").is_empty());
    // An unknown plan is refused.
    let r = s
        .handle(call(
            3,
            "execute_plan",
            json!({ "plan_id": "nope", "items": ["/w/a"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);

    // Only the approved item is posted, with the plan's own request.
    let out = result(
        &s.handle(call(
            4,
            "execute_plan",
            json!({ "plan_id": id, "items": ["/w/a"] }),
        ))
        .unwrap(),
    );
    let posted = f.posted("/v1/sweep");
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0]["items"], json!([plan["items"][0]["request"]]));
    assert_eq!(out["done"], 1);
    assert_eq!(out["results"][0]["backups"][0]["location"], "/b/a.patch");
    let restore = out["results"][0]["restore"].as_str().unwrap();
    assert!(
        restore.contains("git worktree add /w/a backup/a") && restore.contains("apply /b/a.patch"),
        "{restore}"
    );

    // A plan runs once.
    let r = s
        .handle(call(
            5,
            "execute_plan",
            json!({ "plan_id": id, "items": ["/w/b"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(f.posted("/v1/sweep").len(), 1);
}

#[test]
fn old_plans_expire() {
    let f = Fake::default()
        .with("GET /v1/sweep", plan_with(&["/w/a"], &[]))
        .with("POST /v1/sweep", json!([]));
    let mut s = Session::new(&f);
    let shown = result(&s.handle(call(1, "plan_worktree_sweep", json!({}))).unwrap());
    s.plans
        .age_all(crate::mcp_tools::PLAN_TTL + std::time::Duration::from_secs(1));
    let r = s
        .handle(call(
            2,
            "execute_plan",
            json!({ "plan_id": shown["plan_id"], "items": ["/w/a"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("30 minutes"));
    assert!(f.posted("/v1/sweep").is_empty());
}

#[test]
fn nothing_ready_gives_no_plan_id() {
    let f = Fake::default().with("GET /v1/clean/plan", plan_with(&[], &["/c"]));
    let r = one(&f, call(1, "plan_cache_cleanup", json!({ "stale": true })));
    let v = result(&r);
    assert!(v.get("plan_id").is_none());
    assert!(f.posted("/v1/clean").is_empty());
}

#[test]
fn removal_and_cache_plans_go_to_their_routes() {
    let f = Fake::default()
        .with("POST /v1/worktree/plan", plan_with(&["/w/a"], &[]))
        .with("POST /v1/worktree/remove", json!([]))
        .with("POST /v1/clean/plan", plan_with(&["/p/target"], &[]))
        .with("POST /v1/clean", json!([]));
    let mut s = Session::new(&f);
    let w = result(
        &s.handle(call(
            1,
            "plan_worktree_removal",
            json!({ "paths": ["/w/a"], "allow_dirty": true }),
        ))
        .unwrap(),
    );
    assert_eq!(
        f.posted("/v1/worktree/plan")[0],
        json!({ "paths": ["/w/a"], "allow_dirty": true })
    );
    let c = result(
        &s.handle(call(
            2,
            "plan_cache_cleanup",
            json!({ "paths": ["/p/target"] }),
        ))
        .unwrap(),
    );
    assert!(f.posted("/v1/worktree/remove").is_empty() && f.posted("/v1/clean").is_empty());
    result(
        &s.handle(call(
            3,
            "execute_plan",
            json!({ "plan_id": w["plan_id"], "items": ["/w/a"] }),
        ))
        .unwrap(),
    );
    assert_eq!(
        f.posted("/v1/worktree/remove")[0]["items"][0]["path"],
        "/w/a"
    );
    result(
        &s.handle(call(
            4,
            "execute_plan",
            json!({ "plan_id": c["plan_id"], "items": ["/p/target"] }),
        ))
        .unwrap(),
    );
    assert_eq!(f.posted("/v1/clean").len(), 1);
}

fn process_plan(ready: bool) -> Value {
    json!({
        "created_unix": 1,
        "target": "app:/Applications/Slack.app",
        "name": "Slack",
        "action": "quit",
        "ready": ready,
        "refused": if ready { Value::Null } else { json!("protected") },
        "detail": if ready { "Quit Slack: 3 processes." } else { "Slack holds this agent; Mulchie will not act on it." },
        "items": [{
            "ready": ready, "target": "pid 42", "name": "Slack", "verdict": if ready { "ok" } else { "protected" },
            "reason": null, "bytes": 0,
            "request": { "action": "signal", "pid": 42, "signal": "term", "expected_start_time": 777 }
        }],
        "affected": [
            { "pid": 42, "name": "Slack", "start_time": 777, "how": "signalled" },
            { "pid": 43, "name": "Slack Helper", "start_time": 778, "how": "exits_with_app" }
        ],
    })
}

#[test]
fn process_actions_plan_then_execute() {
    let f = Fake::default()
        .with("POST /v1/process/plan", process_plan(true))
        .with(
            "POST /v1/process/execute",
            json!([{ "action": "signal", "target": "pid 42", "status": "done", "freed_bytes": 0, "backups": [] }]),
        );
    let mut s = Session::new(&f);
    let shown = result(
        &s.handle(call(
            1,
            "plan_process_action",
            json!({ "group": "app:/Applications/Slack.app", "action": "quit" }),
        ))
        .unwrap(),
    );
    // The plan asked the core, which knows the caller, and changed nothing.
    let asked = f.posted("/v1/process/plan");
    assert_eq!(asked[0]["group"], "app:/Applications/Slack.app");
    assert_eq!(asked[0]["action"], "quit");
    assert_eq!(asked[0]["caller_pid"], std::process::id());
    assert!(f.posted("/v1/process/execute").is_empty());
    assert_eq!(shown["affected"].as_array().unwrap().len(), 2);
    assert_eq!(shown["ready"][0]["target"], "pid 42");
    let id = shown["plan_id"].as_str().unwrap().to_string();
    // Only targets in the plan.
    let r = s
        .handle(call(
            2,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 1"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    assert!(f.posted("/v1/process/execute").is_empty());
    let done = result(
        &s.handle(call(
            3,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42"] }),
        ))
        .unwrap(),
    );
    // The stored request, with its start time, went to the execute route.
    let sent = f.posted("/v1/process/execute");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["items"][0]["expected_start_time"], 777);
    assert_eq!(sent[0]["items"][0]["signal"], "term");
    assert!(
        done.to_string().contains("cannot be brought back"),
        "{done}"
    );
    // A plan is executed once.
    let r = s
        .handle(call(
            4,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);

    // A pid plan sends the signal and tree, defaulting to term.
    let r = one(
        &f,
        call(5, "plan_process_action", json!({ "pid": 42, "tree": true })),
    );
    result(&r);
    let asked = f.posted("/v1/process/plan");
    assert_eq!(asked[1]["pid"], 42);
    assert_eq!(asked[1]["signal"], "term");
    assert_eq!(asked[1]["tree"], true);
}

#[test]
fn a_refused_process_plan_has_no_plan_id() {
    let f = Fake::default().with("POST /v1/process/plan", process_plan(false));
    let v = result(&one(
        &f,
        call(
            1,
            "plan_process_action",
            json!({ "group": "app:/Applications/Slack.app", "action": "quit" }),
        ),
    ));
    assert!(v.get("plan_id").is_none(), "{v}");
    assert_eq!(v["refused"], "protected");
    // Blocked, not an error: the reason is the answer.
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains("nothing to execute") && next.contains("holds this agent"),
        "{next}"
    );
    // Read-only: a ready plan is shown but not stored.
    let f = Fake::default().with("POST /v1/process/plan", process_plan(true));
    let mut s = Session::read_only(&f);
    let v = result(
        &s.handle(call(
            1,
            "plan_process_action",
            json!({ "pid": 42, "signal": "stop" }),
        ))
        .unwrap(),
    );
    assert!(v.get("plan_id").is_none(), "{v}");
    assert!(v["next"].as_str().unwrap().contains("read-only"));
}

#[test]
fn findings_and_process_detail() {
    let findings = json!({ "findings": [{ "id": "runaway_cpu:42:777", "kind": "runaway_cpu", "severity": "medium",
        "title": "node has used a full core for 40 minutes", "why": "High CPU for a long time can be a stuck loop.",
        "evidence": { "pid": 42 }, "suggested": [] }], "counts": { "medium": 1 }, "enabled": true });
    let f = Fake::default()
        .with("GET /v1/findings", findings.clone())
        .with(
            "GET /v1/findings?severity=medium&kind=runaway_cpu&dismissed=1",
            findings.clone(),
        )
        .with("GET /v1/findings?dismissed=1", findings.clone())
        .with("POST /v1/findings/dismiss", json!({ "ok": true }))
        .with("POST /v1/findings/undismiss", json!({ "ok": true }))
        .with(
            "GET /v1/process/42",
            json!({ "pid": 42, "name": "node", "cmd": ["node", "--token", "[REDACTED]"] }),
        )
        .with("GET /v1/state", json!({ "findings": [] }));
    let v = result(&one(&f, call(1, "list_findings", json!({}))));
    assert_eq!(v["findings"][0]["kind"], "runaway_cpu");
    let note = v["note"].as_str().unwrap();
    assert!(
        note.contains("not verdicts") && note.contains("malware"),
        "{note}"
    );
    let v = result(&one(
        &f,
        call(
            2,
            "list_findings",
            json!({ "severity": "medium", "kind": "runaway_cpu", "include_dismissed": true }),
        ),
    ));
    assert_eq!(v["counts"]["medium"], 1);
    result(&one(
        &f,
        call(
            3,
            "dismiss_finding",
            json!({ "id": "runaway_cpu:42:777", "days": 7 }),
        ),
    ));
    assert_eq!(
        f.posted("/v1/findings/dismiss"),
        vec![json!({ "id": "runaway_cpu:42:777", "days": 7 })]
    );
    result(&one(
        &f,
        call(
            4,
            "dismiss_finding",
            json!({ "id": "runaway_cpu:42:777", "undo": true }),
        ),
    ));
    assert_eq!(
        f.posted("/v1/findings/undismiss"),
        vec![json!({ "id": "runaway_cpu:42:777" })]
    );
    let v = result(&one(&f, call(5, "inspect_process", json!({ "pid": 42 }))));
    assert_eq!(v["cmd"][2], "[REDACTED]");
    // The findings resource.
    let r = one(
        &f,
        req(6, "resources/read", json!({ "uri": "mulchie://findings" })),
    );
    assert!(
        r["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("runaway_cpu"),
        "{r}"
    );
}

#[test]
fn the_release_secret_stays_in_the_server() {
    let f = Fake::default()
        .with(
            "POST /v1/lease",
            json!({ "granted": true, "ticket": 9, "release_secret": "s3cr3t" }),
        )
        .with("POST /v1/release", json!({ "ok": true }));
    let mut s = Session::new(&f);
    let r = s.handle(call(1, "acquire_build_slot", json!({}))).unwrap();
    assert!(!r.to_string().contains("s3cr3t"), "{r}");
    assert_eq!(result(&r)["ticket"], 9);
    // Renewing or polling the ticket sends its secret too.
    result(
        &s.handle(call(2, "acquire_build_slot", json!({ "ticket": 9 })))
            .unwrap(),
    );
    assert_eq!(f.posted("/v1/lease")[1]["secret"], "s3cr3t");
    assert!(f.posted("/v1/lease")[0].get("secret").is_none());
    result(
        &s.handle(call(2, "release_build_slot", json!({ "ticket": 9 })))
            .unwrap(),
    );
    assert_eq!(
        f.posted("/v1/release"),
        vec![json!({ "ticket": 9, "secret": "s3cr3t" })]
    );
    // Forgotten after use.
    result(
        &s.handle(call(3, "release_build_slot", json!({ "ticket": 9 })))
            .unwrap(),
    );
    assert_eq!(f.posted("/v1/release")[1], json!({ "ticket": 9 }));
}

#[test]
fn diagnose_from_the_engine_and_without_one() {
    let f = Fake::default()
        .with(
            "GET /v1/state",
            json!({ "health": "ok", "snapshot": { "disk_free": 1, "disk_total": 2 } }),
        )
        .with("GET /v1/advice", json!({ "ok_to_start_heavy_work": true }));
    let v = result(&one(&f, call(1, "diagnose", json!({}))));
    assert_eq!(v["source"], "engine");
    assert!(v["recommendations"].is_array());
    // No engine and no paths: an answer, not an error.
    let f = Fake::default().failing(
        "GET /v1/state",
        ApiError::new(0, "engine_not_running", "Mulchie's engine is not running"),
    );
    let v = result(&one(&f, call(2, "diagnose", json!({}))));
    assert_eq!(v["source"], "none");
    assert_eq!(v["recommendations"][0]["id"], "start_engine");
}

#[test]
fn prompts_and_resources() {
    let f = Fake::default()
        .with(
            "GET /v1/state",
            json!({ "health": "ok", "snapshot": {}, "actions": [{ "kind": "x" }] }),
        )
        .with("GET /v1/sweep", plan_with(&[], &[]));
    let init = one(
        &f,
        req(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
        ),
    );
    let caps = &init["result"]["capabilities"];
    assert!(caps["prompts"].is_object() && caps["resources"].is_object());
    let i = init["result"]["instructions"].as_str().unwrap();
    assert!(
        i.contains("Start with the diagnose tool")
            && i.contains("execute_plan")
            && i.contains("backups")
    );

    let r = one(&f, req(2, "prompts/list", json!({})));
    let names: Vec<&str> = r["result"]["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["whats-going-on", "optimize"]);
    let r = one(&f, req(3, "prompts/get", json!({ "name": "optimize" })));
    let m = &r["result"]["messages"][0];
    assert_eq!(m["role"], "user");
    assert!(m["content"]["text"].as_str().unwrap().contains("approv"));
    assert_eq!(
        one(&f, req(4, "prompts/get", json!({ "name": "nope" })))["error"]["code"],
        INVALID_PARAMS
    );

    let r = one(&f, req(5, "resources/list", json!({})));
    let uris: Vec<&str> = r["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["uri"].as_str().unwrap())
        .collect();
    for u in [
        "mulchie://state",
        "mulchie://plan/sweep",
        "mulchie://actions",
        "mulchie://diagnosis",
    ] {
        assert!(uris.contains(&u), "{u}");
    }
    let r = one(
        &f,
        req(6, "resources/read", json!({ "uri": "mulchie://actions" })),
    );
    let c = &r["result"]["contents"][0];
    assert_eq!(c["mimeType"], "application/json");
    let v: Value = serde_json::from_str(c["text"].as_str().unwrap()).unwrap();
    assert_eq!(v, json!([{ "kind": "x" }]));
    let r = one(
        &f,
        req(7, "resources/read", json!({ "uri": "mulchie://nope" })),
    );
    assert_eq!(r["error"]["code"], RESOURCE_NOT_FOUND);
    let mut p = modern_meta();
    p["uri"] = json!("mulchie://nope");
    assert_eq!(
        one(&f, req(8, "resources/read", p))["error"]["code"],
        INVALID_PARAMS
    );
}

#[test]
fn build_slot_uses_a_ttl_not_a_pid() {
    let f = Fake::default()
        .with(
            "POST /v1/lease",
            json!({ "granted": true, "ticket": 3, "jobs": 4 }),
        )
        .with("POST /v1/release", json!({ "released": true }));
    one(
        &f,
        call(1, "acquire_build_slot", json!({ "label": "tests" })),
    );
    let b = &f.posted("/v1/lease")[0];
    assert_eq!(b["ttl_secs"], 900);
    assert_eq!(b["label"], "tests");
    assert!(b.get("pid").is_none());
    one(
        &f,
        call(
            2,
            "acquire_build_slot",
            json!({ "ticket": 3, "ttl_secs": 60 }),
        ),
    );
    assert_eq!(f.posted("/v1/lease")[1]["ticket"], 3);
    assert_eq!(f.posted("/v1/lease")[1]["ttl_secs"], 60);
    one(&f, call(3, "release_build_slot", json!({ "ticket": 3 })));
    assert_eq!(f.posted("/v1/release")[0], json!({ "ticket": 3 }));
}

#[test]
fn history_handles_huge_and_short_windows() {
    let samples: Vec<Value> = (0..700).map(|t| json!({ "t": t })).collect();
    let f = Fake::default().with("GET /v1/history", Value::Array(samples));
    let r = one(&f, call(1, "get_history", json!({ "minutes": 30 })));
    let v: Value = serde_json::from_str(&text(&r)).unwrap();
    assert!(v.as_array().unwrap().len() <= 120);
    let r = one(&f, call(2, "get_history", json!({ "minutes": 99999999 })));
    assert_eq!(r["result"]["isError"], true);
}

#[test]
fn modern_requests() {
    let f = Fake::default().with("GET /v1/advice", json!({ "ok_to_start_heavy_work": true }));
    let r = one(&f, req(1, "server/discover", modern_meta()));
    let res = &r["result"];
    assert_eq!(res["resultType"], "complete");
    assert!(res["supportedVersions"]
        .as_array()
        .unwrap()
        .contains(&json!(MODERN_VERSION)));
    // Only versions this `_meta` path accepts, so no dual-era loop.
    assert_eq!(res["supportedVersions"], json!([MODERN_VERSION]));
    assert!(res["capabilities"]["tools"].is_object());
    assert!(res["capabilities"]["prompts"].is_object());
    assert_eq!(res["_meta"][META_SERVER]["name"], "mulchie");

    // Every modern request works without initialize, and says it completed.
    let mut p = modern_meta();
    p["name"] = json!("can_i_build");
    p["arguments"] = json!({});
    let r = one(&f, req(2, "tools/call", p));
    assert_eq!(r["result"]["resultType"], "complete");
    assert_eq!(r["result"]["isError"], false);

    // An unsupported version names the supported ones.
    let mut p = modern_meta();
    p["_meta"][META_VERSION] = json!("2099-01-01");
    let r = one(&f, req(3, "tools/list", p));
    assert_eq!(r["error"]["code"], UNSUPPORTED_VERSION);
    assert_eq!(r["error"]["data"]["requested"], "2099-01-01");
    assert_eq!(r["error"]["data"]["supported"], json!([MODERN_VERSION]));
    // A legacy version in `_meta` is not offered back as supported.
    let mut p = modern_meta();
    p["_meta"][META_VERSION] = json!("2025-11-25");
    let r = one(&f, req(6, "tools/list", p));
    assert_eq!(r["error"]["code"], UNSUPPORTED_VERSION);
    assert_eq!(r["error"]["data"]["supported"], json!([MODERN_VERSION]));

    // Missing required _meta field.
    let mut p = modern_meta();
    p["_meta"].as_object_mut().unwrap().remove(META_CAPS);
    let r = one(&f, req(4, "tools/list", p));
    assert_eq!(r["error"]["code"], INVALID_PARAMS);

    // server/discover without _meta.
    let r = one(&f, req(5, "server/discover", json!({})));
    assert_eq!(r["error"]["code"], INVALID_PARAMS);
}

#[test]
fn engine_api_without_engine_explains() {
    let dir = tempfile::tempdir().unwrap();
    let api = EngineApi::new(Paths::under(dir.path()));
    let r = one(&api, call(1, "get_status", json!({})));
    assert_eq!(r["result"]["isError"], true);
    let t = text(&r);
    assert!(
        t.contains("not running") && t.contains("mulchie serve"),
        "{t}"
    );
}

#[test]
fn against_a_real_engine() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let engine =
        mulchie_core::Engine::new(paths.clone(), crate::commands::safe_test_config(&paths));
    let mut lk = engine.acquire_lock().unwrap();
    let server = mulchie_core::api::serve(engine.clone(), 0).unwrap();
    lk.set_port(Some(server.port())).unwrap();
    let api = EngineApi::new(paths.clone());
    let r = exchange(
        &api,
        &[
            req(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
            ),
            call(
                2,
                "acquire_build_slot",
                json!({ "label": "t", "ttl_secs": 30 }),
            ),
            call(3, "plan_worktree_sweep", json!({})),
        ],
    );
    assert_eq!(r[1]["result"]["isError"], false, "{}", r[1]);
    let lease: Value = serde_json::from_str(&text(&r[1])).unwrap();
    assert_eq!(lease["granted"], true);
    // The release secret stays inside the server.
    assert!(lease.get("release_secret").is_none(), "{lease}");
    assert!(!text(&r[1]).contains("secret"), "{}", r[1]);
    // The exchange ended (EOF), so the server released the slot it held.
    assert!(
        engine.snapshot_state().slots.active.is_empty(),
        "{:?}",
        engine.snapshot_state().slots.active
    );
    // A session that is still open keeps its slot.
    let mut held = Session::new(&api);
    let lease = result(
        &held
            .handle(call(
                1,
                "acquire_build_slot",
                json!({ "label": "t", "ttl_secs": 30 }),
            ))
            .unwrap(),
    );
    let ticket = lease["ticket"].as_u64().unwrap();
    assert_eq!(engine.snapshot_state().slots.active[0].ttl_secs, Some(30));
    assert_eq!(engine.snapshot_state().slots.active[0].pid, None);
    let plan: Value = serde_json::from_str(&text(&r[2])).unwrap();
    assert_eq!(plan["ready"], json!([]));
    let d = one(&api, call(5, "diagnose", json!({})));
    let d: Value = serde_json::from_str(&text(&d)).unwrap();
    assert_eq!(d["source"], "engine");
    // Another session does not know the secret; the engine lets the full
    // token release it only as an administrative release, and says so.
    let r = one(
        &api,
        call(4, "release_build_slot", json!({ "ticket": ticket })),
    );
    if r["result"]["isError"] == false {
        assert!(text(&r).contains("\"admin\": true"), "{r}");
        assert!(engine
            .snapshot_state()
            .slots
            .active
            .iter()
            .all(|s| s.id != ticket));
    } else {
        assert_eq!(engine.snapshot_state().slots.active.len(), 1);
    }
    drop(held);
    assert!(engine.snapshot_state().slots.active.is_empty());
    // The session that took a slot releases it with the stored secret.
    // Tickets are numbered from 1, so the next one is ticket + 1.
    let next = ticket + 1;
    let r = exchange(
        &api,
        &[
            req(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-11-25", "capabilities": {} }),
            ),
            call(
                2,
                "acquire_build_slot",
                json!({ "label": "u", "ttl_secs": 30 }),
            ),
            call(3, "release_build_slot", json!({ "ticket": next })),
        ],
    );
    let got: Value = serde_json::from_str(&text(&r[1])).unwrap();
    assert_eq!(got["ticket"], next, "{got}");
    assert_eq!(r[2]["result"]["isError"], false, "{}", r[2]);
    // Released with its own secret, not administratively.
    assert!(!text(&r[2]).contains("\"admin\": true"), "{}", r[2]);
    assert!(engine
        .snapshot_state()
        .slots
        .active
        .iter()
        .all(|s| s.id != next));
    server.stop();
}

#[test]
fn prune_cache_previews_by_default() {
    let f = Fake::default()
        .with(
            "GET /v1/state",
            json!({ "diskmap": { "categories": [
                { "key": "npm", "name": "npm", "path": "/h/.npm", "bytes": 3_000_000_000u64, "hint": "npm cache clean", "prunable": true }
            ] } }),
        )
        .with("POST /v1/prune", json!({ "ok": true, "freed": 1 }));
    let v = result(&one(&f, call(1, "prune_cache", json!({ "key": "npm" }))));
    assert_eq!(v["preview"], true);
    assert_eq!(v["path"], "/h/.npm");
    assert!(f.posted("/v1/prune").is_empty(), "a preview must not prune");
    let r = one(&f, call(2, "prune_cache", json!({ "key": "nope" })));
    assert_eq!(r["result"]["isError"], true);
    result(&one(
        &f,
        call(3, "prune_cache", json!({ "key": "npm", "execute": true })),
    ));
    assert_eq!(f.posted("/v1/prune"), vec![json!({ "key": "npm" })]);
}

#[test]
fn read_only_mode_uses_the_read_only_token() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(&paths.home).unwrap();
    let engine =
        mulchie_core::Engine::new(paths.clone(), crate::commands::safe_test_config(&paths));
    let mut lk = engine.acquire_lock().unwrap();
    let server = mulchie_core::api::serve(engine.clone(), 0).unwrap();
    lk.set_port(Some(server.port())).unwrap();
    let api = EngineApi::read_only(paths.clone());
    // The engine itself refuses changes from this server, whatever tool
    // filtering does.
    for (path, body) in [
        ("/v1/process/execute", json!({ "items": [] })),
        ("/v1/sweep", json!({ "items": [] })),
        ("/v1/prune", json!({ "key": "npm" })),
        ("/v1/findings/dismiss", json!({ "id": "x" })),
    ] {
        let e = api.call("POST", path, Some(&body)).unwrap_err();
        assert_eq!(
            (e.status, e.code.as_str()),
            (403, "read_only_token"),
            "{path}"
        );
    }
    // Reads, plans and TTL build slots work.
    let mut s = Session::read_only(&api);
    let v = result(
        &s.handle(call(1, "acquire_build_slot", json!({ "ttl_secs": 30 })))
            .unwrap(),
    );
    let ticket = v["ticket"].as_u64().unwrap();
    assert_eq!(engine.snapshot_state().slots.active.len(), 1);
    result(
        &s.handle(call(2, "release_build_slot", json!({ "ticket": ticket })))
            .unwrap(),
    );
    assert!(engine.snapshot_state().slots.active.is_empty());
    result(&s.handle(call(3, "list_findings", json!({}))).unwrap());
    // The full-token server can.
    let full = EngineApi::new(paths.clone());
    full.call(
        "POST",
        "/v1/findings/dismiss",
        Some(&json!({ "id": "x", "days": 1 })),
    )
    .unwrap();
    server.stop();
}

#[test]
fn slots_are_released_when_the_session_ends() {
    let f = Fake::default()
        .with(
            "POST /v1/lease",
            json!({ "granted": true, "ticket": 11, "release_secret": "s11" }),
        )
        .with("POST /v1/release", json!({ "released": true }));
    // The client takes a slot and goes away (EOF) without releasing it.
    let r = exchange(&f, &[call(1, "acquire_build_slot", json!({}))]);
    assert_eq!(result(&r[0])["ticket"], 11);
    assert_eq!(
        f.posted("/v1/release"),
        vec![json!({ "ticket": 11, "secret": "s11" })]
    );
    // A slot released by the client is not released again.
    let f = Fake::default()
        .with(
            "POST /v1/lease",
            json!({ "granted": true, "ticket": 12, "release_secret": "s12" }),
        )
        .with("POST /v1/release", json!({ "released": true }));
    exchange(
        &f,
        &[
            call(1, "acquire_build_slot", json!({})),
            call(2, "release_build_slot", json!({ "ticket": 12 })),
        ],
    );
    assert_eq!(f.posted("/v1/release").len(), 1);
}

#[test]
fn a_slot_lasts_at_most_an_hour_per_renewal() {
    let f = Fake::default().with("POST /v1/lease", json!({ "granted": true, "ticket": 1 }));
    let r = one(
        &f,
        call(1, "acquire_build_slot", json!({ "ttl_secs": 7200 })),
    );
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(f.posted("/v1/lease").is_empty());
    let tools = one(&f, req(2, "tools/list", json!({})));
    let t = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "acquire_build_slot")
        .unwrap()
        .clone();
    assert_eq!(t["inputSchema"]["properties"]["ttl_secs"]["maximum"], 3600);
    let d = t["description"].as_str().unwrap();
    assert!(
        d.contains("3600") && d.contains("released when it exits"),
        "{d}"
    );
}

#[test]
fn a_renewal_that_gets_a_new_ticket_forgets_the_old_secret() {
    // The engine expired ticket 5 and handed out 6 on the renewal.
    let f = Fake::default()
        .with("POST /v1/lease", json!({ "granted": true, "ticket": 6 }))
        .with("POST /v1/release", json!({ "released": true }));
    {
        let mut s = Session::new(&f);
        crate::mcp_tools::lock_secrets(&s.secrets).insert(5, "old".into());
        result(
            &s.handle(call(1, "acquire_build_slot", json!({ "ticket": 5 })))
                .unwrap(),
        );
        assert_eq!(f.posted("/v1/lease")[0]["secret"], "old");
        assert!(crate::mcp_tools::lock_secrets(&s.secrets).is_empty());
    }
    // Nothing held, so nothing released at the end.
    assert!(f.posted("/v1/release").is_empty());
    // A release the engine answers (even "not released") forgets the secret.
    let f = Fake::default().with("POST /v1/release", json!({ "released": false }));
    {
        let mut s = Session::new(&f);
        crate::mcp_tools::lock_secrets(&s.secrets).insert(5, "old".into());
        result(
            &s.handle(call(1, "release_build_slot", json!({ "ticket": 5 })))
                .unwrap(),
        );
        assert!(crate::mcp_tools::lock_secrets(&s.secrets).is_empty());
    }
    assert_eq!(f.posted("/v1/release").len(), 1);
}

#[test]
fn an_execute_without_an_answer_says_the_engine_may_have_acted() {
    let f = Fake::default()
        .with("POST /v1/process/plan", process_plan(true))
        .failing(
            "POST /v1/process/execute",
            ApiError::new(0, "timeout", "the engine did not answer in time"),
        );
    let mut s = Session::new(&f);
    let shown = result(
        &s.handle(call(1, "plan_process_action", json!({ "pid": 42 })))
            .unwrap(),
    );
    let id = shown["plan_id"].as_str().unwrap().to_string();
    let r = s
        .handle(call(
            2,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    let t = text(&r);
    assert!(
        t.contains("may have acted") && t.contains("make a new plan"),
        "{t}"
    );
}

#[test]
fn an_execute_reply_that_is_not_a_list_is_an_error() {
    let f = Fake::default()
        .with("POST /v1/process/plan", process_plan(true))
        .with("POST /v1/process/execute", json!({ "ok": true }));
    let mut s = Session::new(&f);
    let shown = result(
        &s.handle(call(1, "plan_process_action", json!({ "pid": 42 })))
            .unwrap(),
    );
    let id = shown["plan_id"].as_str().unwrap().to_string();
    let r = s
        .handle(call(
            2,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(text(&r).contains("different versions"), "{r}");
}

#[test]
fn a_group_plan_is_all_or_nothing() {
    let mut p = process_plan(true);
    p["items"] = json!([
        { "ready": true, "target": "pid 42", "verdict": "ok",
          "request": { "action": "signal", "pid": 42, "signal": "term", "expected_start_time": 777 } },
        { "ready": true, "target": "pid 43", "verdict": "ok",
          "request": { "action": "signal", "pid": 43, "signal": "term", "expected_start_time": 778 } }
    ]);
    let f = Fake::default().with("POST /v1/process/plan", p).with(
        "POST /v1/process/execute",
        json!([{ "target": "pid 42", "status": "done" }, { "target": "pid 43", "status": "done" }]),
    );
    let mut s = Session::new(&f);
    let shown = result(
        &s.handle(call(
            1,
            "plan_process_action",
            json!({ "group": "app:/Applications/Slack.app", "action": "quit" }),
        ))
        .unwrap(),
    );
    assert_eq!(shown["all_or_nothing"], true);
    let id = shown["plan_id"].as_str().unwrap().to_string();
    let r = s
        .handle(call(
            2,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42"] }),
        ))
        .unwrap();
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("all"), "{r}");
    assert!(f.posted("/v1/process/execute").is_empty());
    // The plan is kept, and all of it can be executed.
    result(
        &s.handle(call(
            3,
            "execute_plan",
            json!({ "plan_id": id, "items": ["pid 42", "pid 43"] }),
        ))
        .unwrap(),
    );
    assert_eq!(
        f.posted("/v1/process/execute")[0]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn dismissing_an_unknown_finding_is_refused_unless_forced() {
    let f = Fake::default()
        .with(
            "GET /v1/findings?dismissed=1",
            json!({ "findings": [{ "id": "runaway_cpu:42:777" }] }),
        )
        .with("POST /v1/findings/dismiss", json!({ "ok": true }));
    let r = one(
        &f,
        call(1, "dismiss_finding", json!({ "id": "runaway_cpu:42" })),
    );
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("runaway_cpu:42:777"), "{r}");
    assert!(f.posted("/v1/findings/dismiss").is_empty());
    result(&one(
        &f,
        call(
            2,
            "dismiss_finding",
            json!({ "id": "runaway_cpu:42", "force": true }),
        ),
    ));
    assert_eq!(f.posted("/v1/findings/dismiss").len(), 1);
}

#[test]
fn read_only_diagnose_suggests_only_tools_it_offers() {
    let f = Fake::default()
        .with(
            "GET /v1/state",
            json!({ "health": "ok", "snapshot": { "disk_free": 1_000_000_000u64, "disk_total": 500_000_000_000u64 },
                "diskmap": { "categories": [{ "key": "npm", "name": "npm", "bytes": 4_000_000_000u64, "prunable": true }] } }),
        )
        .with("GET /v1/advice", json!({ "ok_to_start_heavy_work": true }));
    let full = result(&one(&f, call(1, "diagnose", json!({}))));
    assert!(full.to_string().contains("\"prune_cache\""), "{full}");
    let mut s = Session::read_only(&f);
    let v = result(&s.handle(call(2, "diagnose", json!({}))).unwrap());
    let t = v.to_string();
    for refused in ["\"prune_cache\"", "\"dismiss_finding\"", "\"execute_plan\""] {
        assert!(!t.contains(refused), "{refused} in {t}");
    }
    // The CLI command is kept: the user may run it.
    assert!(t.contains("mulchie prune npm"), "{t}");
}

#[test]
fn sessions_limits_updates_and_guard_tools() {
    let detail = json!({ "session": { "id": "s-500-100", "agent": "claude", "status": "running" },
        "processes": [], "breaches": [], "paused_by_mulchie": [] });
    let f = Fake::default()
        .with("GET /v1/sessions", json!({ "enabled": true, "sessions": [{ "id": "s-500-100" }] }))
        .with("GET /v1/sessions/s-500-100", detail)
        .with(
            "GET /v1/sessions/s-500-100/events?since=3&limit=5",
            json!({ "events": [{ "seq": 4, "t": 1, "type": "hook_pre_tool" }], "next_since": 4, "session_id": "s-500-100" }),
        )
        .with("GET /v1/limits/status", json!({ "report_only": true, "enforcing": true, "breaches": [] }))
        .with("GET /v1/updates", json!({ "enabled": true, "available": 1 }))
        .with(
            "GET /v1/findings",
            json!({ "findings": [{ "id": "u", "category": "update", "title": "claude 2 is out" }, { "id": "x", "category": "hygiene" }] }),
        )
        .with("POST /v1/guard/check", json!({ "decision": "warn", "rule": "sudo", "reason": "elevated" }))
        .with("POST /v1/process/plan", process_plan(true));
    assert_eq!(
        result(&one(&f, call(1, "list_sessions", json!({}))))["sessions"][0]["id"],
        "s-500-100"
    );
    let v = result(&one(
        &f,
        call(
            2,
            "get_session",
            json!({ "id": "s-500-100", "events": true, "since": 3, "limit": 5 }),
        ),
    ));
    assert_eq!(v["events"][0]["seq"], 4);
    assert_eq!(v["next_since"], 4);
    // An id that is not a session id never reaches a path.
    let r = one(
        &f,
        call(3, "get_session", json!({ "id": "s-1-2/../../v1/config" })),
    );
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(
        result(&one(&f, call(4, "limits_status", json!({}))))["report_only"],
        true
    );
    let v = result(&one(&f, call(5, "list_updates", json!({}))));
    assert_eq!(v["updates"].as_array().unwrap().len(), 1, "{v}");
    let v = result(&one(
        &f,
        call(
            6,
            "check_command",
            json!({ "command": "sudo ls", "cwd": "/tmp" }),
        ),
    ));
    assert_eq!(v["decision"], "warn");
    assert_eq!(
        f.posted("/v1/guard/check")[0],
        json!({ "command": "sudo ls", "cwd": "/tmp" })
    );
    // A session plan goes to the plan route with the session.
    result(&one(
        &f,
        call(
            7,
            "plan_process_action",
            json!({ "session": "s-500-100", "action": "pause", "include_agent": true }),
        ),
    ));
    let asked = &f.posted("/v1/process/plan")[0];
    assert_eq!(asked["session"], "s-500-100");
    assert_eq!(asked["include_agent"], true);
    assert_eq!(asked["caller_pid"], std::process::id());
    let r = one(
        &f,
        call(
            8,
            "plan_process_action",
            json!({ "session": "s-500-100", "pid": 4, "action": "quit" }),
        ),
    );
    assert_eq!(r["result"]["isError"], true);
    // check_updates: queued, or off explained.
    let f = Fake::default().with(
        "POST /v1/updates/check",
        json!({ "ok": true, "queued": true }),
    );
    assert_eq!(
        result(&one(&f, call(1, "check_updates", json!({}))))["queued"],
        true
    );
    let f = Fake::default().failing(
        "POST /v1/updates/check",
        ApiError::new(409, "updates_off", "update checks are off"),
    );
    let r = one(&f, call(1, "check_updates", json!({})));
    assert_eq!(r["result"]["isError"], true);
    assert!(text(&r).contains("Only the user"), "{r}");
}

#[test]
fn a_plan_marked_not_ready_gets_no_plan_id_even_with_a_ready_item() {
    let mut p = process_plan(true);
    p["ready"] = json!(false);
    let f = Fake::default().with("POST /v1/process/plan", p);
    let mut s = Session::new(&f);
    let v = result(
        &s.handle(call(1, "plan_process_action", json!({ "pid": 42 })))
            .unwrap(),
    );
    assert!(v.get("plan_id").is_none(), "{v}");
    assert!(
        v["next"].as_str().unwrap().contains("nothing to execute"),
        "{v}"
    );
}
