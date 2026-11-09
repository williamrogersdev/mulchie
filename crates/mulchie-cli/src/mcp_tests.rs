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

#[test]
fn tools_have_titles_and_annotations() {
    let f = Fake::default();
    let r = one(&f, req(1, "tools/list", json!({})));
    let tools = r["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 15);
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
    for n in [
        "clean_caches",
        "remove_worktrees",
        "sweep_worktrees",
        "prune_cache",
        "workspace_action",
        "process_action",
    ] {
        assert_eq!(ann(n)["destructiveHint"], true, "{n}");
        assert_eq!(ann(n)["readOnlyHint"], false, "{n}");
    }
    for n in [
        "get_status",
        "can_i_build",
        "top_processes",
        "disk_breakdown",
    ] {
        assert_eq!(ann(n)["readOnlyHint"], true, "{n}");
    }
    assert_eq!(ann("acquire_build_slot")["destructiveHint"], false);
    let acquire = tools
        .iter()
        .find(|t| t["name"] == "acquire_build_slot")
        .unwrap();
    let d = acquire["description"].as_str().unwrap();
    assert!(d.contains("renew") && d.contains("ttl_secs") && d.contains("release_build_slot"));
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
    let r = one(&f, req(7, "resources/list", json!({})));
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
        ("remove_worktrees", json!({ "paths": [] })),
        ("remove_worktrees", json!({ "paths": [3] })),
        ("acquire_build_slot", json!({ "ttl_secs": 0 })),
        ("acquire_build_slot", json!({ "ttl_secs": 100000 })),
        ("process_action", json!({ "pid": 1, "action": "term" })),
        ("clean_caches", json!({})),
        ("clean_caches", json!({ "stale": true, "paths": ["/x"] })),
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

#[test]
fn sweep_executes_exactly_the_plan() {
    let plan = plan_with(&["/w/a", "/w/b"], &["/w/c"]);
    let f = Fake::default()
        .with("GET /v1/sweep", plan.clone())
        .with("POST /v1/sweep", json!([]));
    // A dry run returns the plan and changes nothing.
    let r = one(&f, call(1, "sweep_worktrees", json!({ "dry_run": true })));
    assert_eq!(r["result"]["isError"], false);
    let shown: Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(shown, plan);
    assert!(f.posted("/v1/sweep").is_empty());
    // A real run posts the ready requests and nothing else.
    let r = one(&f, call(2, "sweep_worktrees", json!({})));
    assert_eq!(r["result"]["isError"], false);
    let posted = f.posted("/v1/sweep");
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0]["items"],
        json!([plan["items"][0]["request"], plan["items"][1]["request"]])
    );
    let out: Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(out["not_ready"][0]["target"], "/w/c");
}

#[test]
fn nothing_ready_posts_nothing() {
    let f = Fake::default().with("GET /v1/clean/plan", plan_with(&[], &["/c"]));
    let r = one(&f, call(1, "clean_caches", json!({ "stale": true })));
    assert_eq!(r["result"]["isError"], false);
    assert!(f.posted("/v1/clean").is_empty());
}

#[test]
fn remove_and_clean_plan_first() {
    let f = Fake::default()
        .with("POST /v1/worktree/plan", plan_with(&["/w/a"], &[]))
        .with("POST /v1/worktree/remove", json!([]))
        .with("POST /v1/clean/plan", plan_with(&["/p/target"], &[]))
        .with("POST /v1/clean", json!([]));
    one(
        &f,
        call(
            1,
            "remove_worktrees",
            json!({ "paths": ["/w/a"], "allow_dirty": true }),
        ),
    );
    assert_eq!(
        f.posted("/v1/worktree/plan")[0],
        json!({ "paths": ["/w/a"], "allow_dirty": true })
    );
    assert_eq!(
        f.posted("/v1/worktree/remove")[0]["items"][0]["path"],
        "/w/a"
    );
    one(
        &f,
        call(
            2,
            "clean_caches",
            json!({ "paths": ["/p/target"], "dry_run": true }),
        ),
    );
    assert!(f.posted("/v1/clean").is_empty());
    one(
        &f,
        call(3, "clean_caches", json!({ "paths": ["/p/target"] })),
    );
    assert_eq!(f.posted("/v1/clean").len(), 1);
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
    assert!(res["supportedVersions"]
        .as_array()
        .unwrap()
        .contains(&json!("2025-06-18")));
    assert!(res["capabilities"]["tools"].is_object());
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
    assert!(r["error"]["data"]["supported"].is_array());

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
    let engine = mulchie_core::Engine::new(paths.clone(), mulchie_core::Config::load_from(&paths));
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
            call(3, "sweep_worktrees", json!({ "dry_run": true })),
        ],
    );
    assert_eq!(r[1]["result"]["isError"], false, "{}", r[1]);
    let lease: Value = serde_json::from_str(&text(&r[1])).unwrap();
    assert_eq!(lease["granted"], true);
    let ticket = lease["ticket"].as_u64().unwrap();
    assert_eq!(engine.snapshot_state().slots.active[0].ttl_secs, Some(30));
    assert_eq!(engine.snapshot_state().slots.active[0].pid, None);
    let plan: Value = serde_json::from_str(&text(&r[2])).unwrap();
    assert_eq!(plan["items"], json!([]));
    let r = one(
        &api,
        call(4, "release_build_slot", json!({ "ticket": ticket })),
    );
    assert_eq!(r["result"]["isError"], false);
    assert!(engine.snapshot_state().slots.active.is_empty());
    server.stop();
}
