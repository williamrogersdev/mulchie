//! Translation per client and event, with inputs shaped like the clients'
//! documented examples, and the fail-open cases.

use super::*;
use std::cell::RefCell;
use HookClient::*;

/// Inputs per client and neutral event, following each client's hook
/// reference (field names and example values from the docs).
fn fixture(c: HookClient, event: &str) -> Value {
    match (c, event) {
        // code.claude.com/docs/en/hooks
        (ClaudeCode, "session_start") => json!({
            "session_id": "abc123", "transcript_path": "/Users/me/.claude/projects/x/t.jsonl",
            "cwd": "/Users/me/app", "hook_event_name": "SessionStart", "source": "resume", "model": "claude-opus-5"
        }),
        (ClaudeCode, "prompt") => json!({
            "session_id": "abc123", "transcript_path": "/t.jsonl", "cwd": "/Users/me/app",
            "permission_mode": "default", "hook_event_name": "UserPromptSubmit",
            "prompt": "Write a function to calculate the factorial of a number"
        }),
        (ClaudeCode, "pre_tool") => json!({
            "session_id": "abc123", "prompt_id": "550e8400-e29b-41d4-a716-446655440000",
            "transcript_path": "/home/user/.claude/projects/t.jsonl", "cwd": "/home/user/my-project",
            "permission_mode": "default", "hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": { "command": "npm test", "description": "Run test suite", "timeout": 120000, "run_in_background": false },
            "tool_use_id": "toolu_01ABC123"
        }),
        (ClaudeCode, "post_tool") => json!({
            "session_id": "abc123", "transcript_path": "/t.jsonl", "cwd": "/home/user/my-project",
            "permission_mode": "default", "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": { "command": "npm test" }, "tool_response": { "stdout": "ok" },
            "tool_use_id": "toolu_01ABC123", "duration_ms": 12
        }),
        (ClaudeCode, "stop") => json!({
            "session_id": "abc123", "transcript_path": "/t.jsonl", "cwd": "/home/user/my-project",
            "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": "Done."
        }),
        (ClaudeCode, "session_end") => json!({
            "session_id": "abc123", "transcript_path": "/t.jsonl", "cwd": "/home/user/my-project",
            "hook_event_name": "SessionEnd", "reason": "other"
        }),
        // learn.chatgpt.com/docs/hooks
        (Codex, ev) => {
            let mut v = json!({ "session_id": "019a-codex", "transcript_path": "/t.jsonl", "cwd": "/Users/me/app",
                "model": "gpt-5", "permission_mode": "default" });
            let extra = match ev {
                "session_start" => {
                    json!({ "hook_event_name": "SessionStart", "source": "startup" })
                }
                "prompt" => {
                    json!({ "hook_event_name": "UserPromptSubmit", "turn_id": "t1", "prompt": "fix the build" })
                }
                "pre_tool" => {
                    json!({ "hook_event_name": "PreToolUse", "turn_id": "t1", "tool_name": "Bash",
                    "tool_use_id": "call_1", "tool_input": { "command": "cargo build" } })
                }
                "post_tool" => {
                    json!({ "hook_event_name": "PostToolUse", "turn_id": "t1", "tool_name": "Bash",
                    "tool_use_id": "call_1", "tool_input": { "command": "cargo build" }, "tool_response": "ok" })
                }
                "stop" => {
                    json!({ "hook_event_name": "Stop", "turn_id": "t1", "stop_hook_active": false, "last_assistant_message": "Done" })
                }
                _ => json!({ "hook_event_name": "SessionEnd", "reason": "other" }),
            };
            merge(&mut v, extra);
            v
        }
        // cursor.com/docs/agent/hooks
        (Cursor, ev) => {
            let mut v = json!({ "conversation_id": "conv-1", "generation_id": "gen-1", "model": "m", "model_id": "m",
                "cursor_version": "2.0", "workspace_roots": ["/Users/me/app"], "user_email": null, "transcript_path": null });
            let extra = match ev {
                "session_start" => {
                    json!({ "hook_event_name": "sessionStart", "session_id": "sess-1",
                    "is_background_agent": false, "composer_mode": "agent" })
                }
                "prompt" => {
                    json!({ "hook_event_name": "beforeSubmitPrompt", "prompt": "fix it", "attachments": [] })
                }
                "pre_tool" => {
                    json!({ "hook_event_name": "beforeShellExecution", "command": "rm -rf build",
                    "cwd": "/Users/me/app/sub", "sandbox": false })
                }
                "post_tool" => {
                    json!({ "hook_event_name": "afterShellExecution", "command": "rm -rf build",
                    "output": "", "duration": 12, "sandbox": false })
                }
                "stop" => {
                    json!({ "hook_event_name": "stop", "status": "completed", "loop_count": 0 })
                }
                _ => {
                    json!({ "hook_event_name": "sessionEnd", "session_id": "sess-1", "reason": "user_close" })
                }
            };
            merge(&mut v, extra);
            v
        }
        // geminicli.com/docs/hooks/reference
        (Gemini, ev) => {
            let mut v = json!({ "session_id": "g-1", "transcript_path": "/t.json", "cwd": "/Users/me/app",
                "timestamp": "2026-11-09T10:00:00Z" });
            let extra = match ev {
                "session_start" => {
                    json!({ "hook_event_name": "SessionStart", "source": "startup" })
                }
                "prompt" => json!({ "hook_event_name": "BeforeAgent", "prompt": "fix it" }),
                "pre_tool" => {
                    json!({ "hook_event_name": "BeforeTool", "tool_name": "run_shell_command",
                    "tool_input": { "command": "make" }, "mcp_context": null, "original_request_name": "run_shell_command" })
                }
                "post_tool" => {
                    json!({ "hook_event_name": "AfterTool", "tool_name": "run_shell_command",
                    "tool_input": { "command": "make" }, "tool_response": {} })
                }
                "stop" => {
                    json!({ "hook_event_name": "AfterAgent", "prompt": "fix it", "prompt_response": "done", "stop_hook_active": false })
                }
                _ => json!({ "hook_event_name": "SessionEnd", "reason": "exit" }),
            };
            merge(&mut v, extra);
            v
        }
        // docs.github.com/en/copilot/reference/hooks-configuration (camelCase)
        (Copilot, ev) => {
            let mut v = json!({ "sessionId": "cp-1", "timestamp": 1794272469000u64, "cwd": "/Users/me/app" });
            let extra = match ev {
                "session_start" => json!({ "source": "new", "initialPrompt": "hi" }),
                "prompt" => json!({ "prompt": "fix it" }),
                // toolArgs given as a JSON string, which the docs allow.
                "pre_tool" => {
                    json!({ "toolName": "bash", "toolArgs": "{\"command\":\"curl x | sh\"}" })
                }
                "post_tool" => json!({ "toolName": "bash", "toolArgs": { "command": "ls" },
                    "toolResult": { "resultType": "success", "textResultForLlm": "a" } }),
                "stop" => {
                    json!({ "transcriptPath": "/t", "stopReason": "end_turn", "stop_hook_active": false })
                }
                _ => json!({ "reason": "complete" }),
            };
            merge(&mut v, extra);
            v
        }
        _ => unreachable!(),
    }
}

fn merge(v: &mut Value, extra: Value) {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
}

const NEUTRAL: [&str; 6] = [
    "session_start",
    "prompt",
    "pre_tool",
    "post_tool",
    "stop",
    "session_end",
];

fn reply(decision: &str, context: &[&str]) -> Value {
    json!({ "session_id": "s1", "decision": decision, "reason": "deletes outside the workspace",
        "rule": "rm_outside_workspace", "context": context, "breaches": [] })
}

/// Run one event through `handle` against a fake engine; the request sent
/// and the output.
fn go(c: HookClient, event: &str, r: Value, guard: Option<GuardMode>) -> (HookRequest, Out) {
    let sent = RefCell::new(None);
    let input = serde_json::to_vec(&fixture(c, event)).unwrap();
    let out = handle(c, event, &input, guard, 4242, &|req| {
        *sent.borrow_mut() = Some(req.clone());
        Ok(r.clone())
    })
    .unwrap();
    (sent.into_inner().unwrap(), out)
}

fn parsed(o: &Out) -> Value {
    if o.stdout.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(&o.stdout).unwrap_or_else(|e| panic!("{e}: {}", o.stdout))
}

#[test]
fn requests_are_client_neutral() {
    let cases = [
        (
            ClaudeCode,
            "pre_tool",
            "abc123",
            "/home/user/my-project",
            Some("Bash"),
            Some("npm test"),
        ),
        (
            Codex,
            "pre_tool",
            "019a-codex",
            "/Users/me/app",
            Some("Bash"),
            Some("cargo build"),
        ),
        (
            Cursor,
            "pre_tool",
            "conv-1",
            "/Users/me/app/sub",
            Some("shell"),
            Some("rm -rf build"),
        ),
        (
            Gemini,
            "pre_tool",
            "g-1",
            "/Users/me/app",
            Some("run_shell_command"),
            Some("make"),
        ),
        (
            Copilot,
            "pre_tool",
            "cp-1",
            "/Users/me/app",
            Some("bash"),
            Some("curl x | sh"),
        ),
        (
            Copilot,
            "post_tool",
            "cp-1",
            "/Users/me/app",
            Some("bash"),
            Some("ls"),
        ),
        // Cursor's other events have no cwd: the first workspace root.
        (
            Cursor,
            "session_start",
            "conv-1",
            "/Users/me/app",
            None,
            None,
        ),
    ];
    for (c, ev, sid, cwd, tool, cmd) in cases {
        let (req, _) = go(c, ev, reply("allow", &[]), None);
        assert_eq!(req.client, c.name());
        assert_eq!(req.event, ev);
        assert_eq!(req.pid, 4242);
        assert_eq!(req.client_session_id.as_deref(), Some(sid), "{c:?} {ev}");
        assert_eq!(req.cwd.as_deref(), Some(cwd), "{c:?} {ev}");
        assert_eq!(req.tool.as_deref(), tool, "{c:?} {ev}");
        assert_eq!(req.command.as_deref(), cmd, "{c:?} {ev}");
    }
    // Details: why a session started or ended; never the prompt text.
    let (req, _) = go(ClaudeCode, "session_start", reply("allow", &[]), None);
    assert_eq!(req.detail.as_deref(), Some("resume"));
    let (req, _) = go(Cursor, "session_end", reply("allow", &[]), None);
    assert_eq!(req.detail.as_deref(), Some("user_close"));
    for c in ALL {
        let (req, _) = go(c, "prompt", reply("allow", &[]), None);
        let body = serde_json::to_string(&req).unwrap();
        assert!(
            !body.contains("fix it") && !body.contains("factorial"),
            "{body}"
        );
    }
    // The client's own event names map too.
    assert_eq!(neutral_event(ClaudeCode, "PreToolUse"), "pre_tool");
    assert_eq!(neutral_event(Cursor, "beforeShellExecution"), "pre_tool");
    assert_eq!(neutral_event(Gemini, "AfterAgent"), "stop");
    assert_eq!(neutral_event(Copilot, "agentStop"), "stop");
    assert_eq!(neutral_event(Codex, "Interrupt"), "other");
}

/// Payloads as the clients' docs show them, fed straight to `request`.
/// Existing fixtures cover the same shapes with other values; these pin the
/// documented examples exactly.
#[test]
fn documented_pre_tool_payloads_parse() {
    let cases = [
        (
            Copilot,
            "preToolUse",
            r#"{"timestamp":1,"cwd":"/w","sessionId":"s1","toolName":"bash","toolArgs":"{\"command\":\"ls\"}"}"#,
            Some("s1"),
            "bash",
        ),
        // Old Copilot versions send no sessionId: the session id is absent
        // and the engine binds by the hook's process instead.
        (
            Copilot,
            "preToolUse",
            r#"{"timestamp":1,"cwd":"/w","toolName":"bash","toolArgs":"{\"command\":\"ls\"}"}"#,
            None,
            "bash",
        ),
        (
            Cursor,
            "beforeShellExecution",
            r#"{"conversation_id":"c1","workspace_roots":["/w"],"command":"ls","cwd":"/w","hook_event_name":"beforeShellExecution"}"#,
            Some("c1"),
            "shell",
        ),
        (
            Gemini,
            "BeforeTool",
            r#"{"session_id":"g1","cwd":"/w","hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"ls"}}"#,
            Some("g1"),
            "run_shell_command",
        ),
        (
            Codex,
            "PreToolUse",
            r#"{"session_id":"x1","cwd":"/w","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
            Some("x1"),
            "Bash",
        ),
    ];
    for (c, event, body, sid, tool) in cases {
        let input: Value = serde_json::from_str(body).unwrap();
        let req = request(c, event, &input, 7);
        assert_eq!(req.event, "pre_tool", "{c:?}");
        assert_eq!(req.client_session_id.as_deref(), sid, "{c:?} {body}");
        assert_eq!(req.cwd.as_deref(), Some("/w"), "{c:?}");
        assert_eq!(req.tool.as_deref(), Some(tool), "{c:?}");
        assert_eq!(req.command.as_deref(), Some("ls"), "{c:?}");
    }
}

const ALL: [HookClient; 5] = [ClaudeCode, Codex, Cursor, Gemini, Copilot];

#[test]
fn allow_with_nothing_to_say_prints_what_the_client_reads_as_allow() {
    for c in ALL {
        for ev in NEUTRAL {
            let (_, o) = go(c, ev, reply("allow", &[]), None);
            assert_eq!(o.code, 0, "{c:?} {ev}");
            assert_eq!(o, fail_open(c, ev), "{c:?} {ev}");
        }
    }
    assert_eq!(
        parsed(&fail_open(Cursor, "pre_tool")),
        json!({ "permission": "allow" })
    );
    assert_eq!(
        parsed(&fail_open(Cursor, "prompt")),
        json!({ "continue": true })
    );
    assert_eq!(parsed(&fail_open(Codex, "stop")), json!({}));
    assert_eq!(fail_open(ClaudeCode, "pre_tool"), Out::default());
}

#[test]
fn context_goes_where_each_client_reads_it() {
    let ctx = "Memory is tight; wrap builds in `mulchie run`.";
    let r = reply("allow", &[ctx]);
    let table: [(HookClient, &str, Value); 13] = [
        (
            ClaudeCode,
            "session_start",
            json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": ctx } }),
        ),
        (
            ClaudeCode,
            "prompt",
            json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": ctx } }),
        ),
        (
            ClaudeCode,
            "pre_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": ctx } }),
        ),
        (
            ClaudeCode,
            "post_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": ctx } }),
        ),
        (
            Codex,
            "session_start",
            json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": ctx } }),
        ),
        (
            Codex,
            "pre_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": ctx } }),
        ),
        (Codex, "stop", json!({})),
        (
            Cursor,
            "session_start",
            json!({ "additional_context": ctx }),
        ),
        (
            Cursor,
            "pre_tool",
            json!({ "permission": "allow", "agent_message": ctx }),
        ),
        (
            Gemini,
            "session_start",
            json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": ctx } }),
        ),
        (
            Gemini,
            "post_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "AfterTool", "additionalContext": ctx } }),
        ),
        (
            Copilot,
            "session_start",
            json!({ "additionalContext": ctx }),
        ),
        // Copilot drops userPromptSubmitted output: nothing to print.
        (Copilot, "prompt", Value::Null),
    ];
    for (c, ev, want) in table {
        let (_, o) = go(c, ev, r.clone(), None);
        assert_eq!(parsed(&o), want, "{c:?} {ev}");
        assert_eq!(o.code, 0);
    }
    // Stop and session end never carry context (it would keep the agent going).
    for c in ALL {
        for ev in ["stop", "session_end"] {
            let (_, o) = go(c, ev, r.clone(), None);
            assert_eq!(o, fail_open(c, ev), "{c:?} {ev}");
        }
    }
    // Breaches are context too.
    let r = json!({ "decision": "allow", "context": [], "breaches": [{ "message": "used 9 cores for 5 min (cap 4); deprioritized" }] });
    let (_, o) = go(ClaudeCode, "pre_tool", r, None);
    assert!(
        o.stdout.contains("Mulchie limit breach: used 9 cores"),
        "{}",
        o.stdout
    );
}

#[test]
fn warn_asks_where_the_client_can_and_advises_where_it_cannot() {
    let r = reply("warn", &[]);
    let why = "deletes outside the workspace (Mulchie guard rule rm_outside_workspace)";
    let (_, o) = go(ClaudeCode, "pre_tool", r.clone(), None);
    assert_eq!(
        parsed(&o),
        json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse",
        "permissionDecision": "ask", "permissionDecisionReason": why } })
    );
    let (_, o) = go(Cursor, "pre_tool", r.clone(), None);
    assert_eq!(
        parsed(&o),
        json!({ "permission": "ask", "user_message": why, "agent_message": why })
    );
    let (_, o) = go(Copilot, "pre_tool", r.clone(), None);
    assert_eq!(
        parsed(&o),
        json!({ "permissionDecision": "ask", "permissionDecisionReason": why })
    );
    assert_eq!(o.code, 0);
    // Codex documents no "ask": a message to the user and context for the agent.
    let (_, o) = go(Codex, "pre_tool", r.clone(), None);
    let v = parsed(&o);
    assert_eq!(v["systemMessage"], format!("Mulchie warns: {why}"));
    assert_eq!(
        v["hookSpecificOutput"]["additionalContext"],
        format!("Mulchie warns: {why}")
    );
    assert!(v["hookSpecificOutput"].get("permissionDecision").is_none());
    // Gemini documents no "ask" either.
    let (_, o) = go(Gemini, "pre_tool", r.clone(), None);
    assert_eq!(
        parsed(&o),
        json!({ "decision": "allow", "systemMessage": format!("Mulchie warns: {why}") })
    );
    // A warning on a prompt is advice.
    let (_, o) = go(ClaudeCode, "prompt", r, None);
    assert_eq!(
        parsed(&o)["hookSpecificOutput"]["additionalContext"],
        format!("Mulchie warns: {why}")
    );
}

#[test]
fn block_uses_each_clients_deny() {
    let r = reply("block", &[]);
    let why = "deletes outside the workspace (Mulchie guard rule rm_outside_workspace)";
    let table: [(HookClient, &str, Value); 7] = [
        (
            ClaudeCode,
            "pre_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why } }),
        ),
        (
            ClaudeCode,
            "prompt",
            json!({ "decision": "block", "reason": why }),
        ),
        (
            Codex,
            "pre_tool",
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why } }),
        ),
        (
            Cursor,
            "pre_tool",
            json!({ "permission": "deny", "user_message": why, "agent_message": why }),
        ),
        (
            Cursor,
            "prompt",
            json!({ "continue": false, "user_message": why }),
        ),
        (
            Gemini,
            "pre_tool",
            json!({ "decision": "deny", "reason": why }),
        ),
        (
            Copilot,
            "pre_tool",
            json!({ "permissionDecision": "deny", "permissionDecisionReason": why }),
        ),
    ];
    for (c, ev, want) in table {
        let (_, o) = go(c, ev, r.clone(), None);
        assert_eq!(parsed(&o), want, "{c:?} {ev}");
        // JSON answers exit 0, Copilot's pre-tool hook included.
        assert_eq!(o.code, 0, "{c:?} {ev}");
    }
    // Codex blocks a prompt by exit 2 with the reason on stderr.
    let (_, o) = go(Codex, "prompt", r.clone(), None);
    assert_eq!((o.code, o.stdout.as_str(), o.stderr.trim()), (2, "", why));
    // Nothing blocks after the fact or at the end of a session.
    for c in ALL {
        for ev in ["post_tool", "stop", "session_end"] {
            let (_, o) = go(c, ev, r.clone(), None);
            assert_eq!(o.code, 0, "{c:?} {ev}");
            assert!(
                !o.stdout.contains("deny") && !o.stdout.contains("block"),
                "{c:?} {ev}"
            );
        }
    }
}

#[test]
fn the_guard_ceiling_lowers_decisions() {
    assert_eq!(
        capped(Decision::Block, Some(GuardMode::Warn)),
        Decision::Warn
    );
    assert_eq!(
        capped(Decision::Block, Some(GuardMode::Off)),
        Decision::Allow
    );
    assert_eq!(
        capped(Decision::Warn, Some(GuardMode::Off)),
        Decision::Allow
    );
    assert_eq!(
        capped(Decision::Block, Some(GuardMode::Block)),
        Decision::Block
    );
    assert_eq!(capped(Decision::Block, None), Decision::Block);
    let (_, o) = go(
        ClaudeCode,
        "pre_tool",
        reply("block", &[]),
        Some(GuardMode::Warn),
    );
    let v = parsed(&o);
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "ask");
    assert!(v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .starts_with("Mulchie noted:"));
    let (_, o) = go(
        Cursor,
        "pre_tool",
        reply("block", &[]),
        Some(GuardMode::Off),
    );
    let v = parsed(&o);
    assert_eq!(v["permission"], "allow");
    assert!(v["agent_message"]
        .as_str()
        .unwrap()
        .contains("Mulchie noted"));
    // An unknown decision is read as allow.
    let (_, o) = go(Copilot, "pre_tool", json!({ "decision": "maybe" }), None);
    assert_eq!(o, Out::default());
}

#[test]
fn the_engines_reply_and_limits_fit() {
    // The reply as the core sends it (api_tests: sessions_hooks_and_updates_over_the_api).
    let r: HookReply = serde_json::from_str(
        r#"{"breaches":[],"context":[],"decision":"warn","reason":"pipes a download straight into sh","rule":"pipe_to_shell","session_id":null}"#,
    )
    .unwrap();
    assert_eq!(r.decision(), Decision::Warn);
    assert_eq!(r.rule.as_deref(), Some("pipe_to_shell"));
    // The engine counts bytes: a long multibyte session id stays within 512.
    let long = "é".repeat(400);
    let input =
        json!({ "session_id": long, "tool_name": "Bash", "tool_input": { "command": "ls" } });
    let req = request(ClaudeCode, "pre_tool", &input, 1);
    assert!(req.client_session_id.as_ref().unwrap().len() <= 512);
    assert!(!req.client.is_empty() && req.client.len() <= 64);
    // Absent fields are left out, not sent as null.
    let v = serde_json::to_value(request(ClaudeCode, "stop", &json!({}), 1)).unwrap();
    assert!(v.get("client_session_id").is_none(), "{v}");
}

#[test]
fn bad_input_and_errors_fail() {
    let ok = |_: &HookRequest| -> Result<Value, String> { Ok(json!({ "decision": "block" })) };
    let calls = RefCell::new(0);
    let counting = |_: &HookRequest| -> Result<Value, String> {
        *calls.borrow_mut() += 1;
        Ok(json!({}))
    };
    for bad in [&b"{not json"[..], b"[1,2]", b"\"x\""] {
        assert!(handle(ClaudeCode, "pre_tool", bad, None, 1, &counting).is_err());
    }
    assert_eq!(*calls.borrow(), 0, "bad input never reaches the engine");
    let big = vec![b' '; MAX_INPUT as usize + 1];
    assert!(handle(ClaudeCode, "pre_tool", &big, None, 1, &ok).is_err());
    // Empty input is an empty event, not an error.
    assert!(handle(ClaudeCode, "session_start", b"", None, 1, &counting).is_ok());
    let failing = |_: &HookRequest| -> Result<Value, String> { Err("no engine is running".into()) };
    assert!(handle(Copilot, "pre_tool", b"{}", None, 1, &failing).is_err());
    let odd = |_: &HookRequest| -> Result<Value, String> { Ok(json!("nope")) };
    assert!(handle(Cursor, "pre_tool", b"{}", None, 1, &odd).is_err());
}

#[test]
fn the_budget_is_kept_when_the_engine_stalls() {
    let start = Instant::now();
    let r: Result<Out, String> = with_budget(BUDGET, || {
        std::thread::sleep(Duration::from_secs(3));
        Ok(Out::default())
    });
    let took = start.elapsed();
    assert!(r.unwrap_err().contains("300 ms"));
    assert!(took < Duration::from_millis(450), "{took:?}");
    assert!(took >= BUDGET);
    // A quick answer comes through.
    assert_eq!(with_budget(BUDGET, || Ok(7)).unwrap(), 7);
}

#[test]
fn failures_are_logged_at_most_once_a_minute() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    log_failure(&paths, "first");
    log_failure(&paths, "second");
    let text = std::fs::read_to_string(paths.config_dir.join("hook.log")).unwrap();
    assert!(text.contains("first") && !text.contains("second"), "{text}");
    assert_eq!(text.lines().count(), 1);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(paths.config_dir.join("hook.log"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "private");
}
