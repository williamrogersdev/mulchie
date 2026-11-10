//! `mulchie hook <client> <event>`: the command an agent's hook system runs.
//!
//! It reads the client's JSON event on stdin, reports it to the engine with
//! the read-only token, and prints that client's documented answer. It fails
//! open: with no engine, a slow engine (300 ms budget for everything), bad
//! input or any error it allows the action, exits 0, prints only what that
//! client reads as "allow" (nothing, or the JSON some clients require), and
//! notes the problem in `<config dir>/hook.log` at most once a minute.

use crate::cli::{GuardMode, HookClient};
use crate::conn::{self, Conn};
use crate::hook_wire::{Decision, HookReply, HookRequest, EVENTS, ROUTE};
use crate::hooks_install::events_for;
use mulchie_core::Paths;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// Everything, from reading stdin to the engine's reply.
pub const BUDGET: Duration = Duration::from_millis(300);
/// Larger events are not read (prompts and tool output can be big; the
/// fields Mulchie needs are small).
pub const MAX_INPUT: u64 = 1 << 20;
/// Context text handed to an agent at most.
const MAX_CONTEXT: usize = 9_000;
const LOG_EVERY: Duration = Duration::from_secs(60);
const LOG_MAX: u64 = 256 * 1024;

/// What to print and how to exit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Out {
    fn json(v: Value) -> Self {
        Out {
            stdout: format!("{v}\n"),
            ..Out::default()
        }
    }
}

/// The client-neutral name of `event`: one of [`EVENTS`] as given, or the
/// client's own event name.
pub fn neutral_event(client: HookClient, event: &str) -> &'static str {
    if let Some(e) = EVENTS.iter().find(|e| **e == event) {
        return e;
    }
    events_for(client)
        .iter()
        .find(|s| s.native.eq_ignore_ascii_case(event))
        .map(|s| s.neutral)
        .unwrap_or("other")
}

fn str_at<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// At most `n` bytes, the ellipsis included: the engine counts bytes.
pub fn cap(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Map a client's event to the request for the engine.
pub fn request(client: HookClient, event: &str, input: &Value, pid: u32) -> HookRequest {
    let event = neutral_event(client, event);
    let session = match client {
        HookClient::Cursor => str_at(input, &["conversation_id", "session_id"]),
        HookClient::Copilot => str_at(input, &["sessionId", "session_id"]),
        _ => str_at(input, &["session_id"]),
    };
    let cwd = str_at(input, &["cwd"])
        .map(String::from)
        .or_else(|| {
            input["workspace_roots"][0]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.display().to_string())
        });
    let tool_event = matches!(event, "pre_tool" | "post_tool");
    let (tool, command) = if !tool_event {
        (None, None)
    } else if client == HookClient::Cursor {
        // beforeShellExecution / afterShellExecution carry the command itself.
        (
            Some("shell".to_string()),
            str_at(input, &["command"]).map(String::from),
        )
    } else {
        let tool = str_at(input, &["tool_name", "toolName"]).map(String::from);
        let mut args = input
            .get("tool_input")
            .or_else(|| input.get("toolArgs"))
            .cloned()
            .unwrap_or(Value::Null);
        // Copilot's toolArgs may be a JSON string.
        if let Value::String(s) = &args {
            args = serde_json::from_str(s).unwrap_or(Value::Null);
        }
        (tool, str_at(&args, &["command"]).map(String::from))
    };
    let detail = match event {
        "session_start" | "session_end" | "stop" | "other" => {
            str_at(input, &["source", "reason", "status", "stopReason"]).map(String::from)
        }
        _ => None,
    };
    HookRequest {
        client: client.name().to_string(),
        client_session_id: session.map(|s| cap(s, 512)),
        event: event.to_string(),
        pid,
        cwd,
        tool: tool.map(|t| cap(&t, 128)),
        command: command.map(|c| cap(&c, 8_192)),
        detail: detail.map(|d| cap(&d, 256)),
    }
}

/// The decision after this client's guard ceiling.
pub fn capped(d: Decision, guard: Option<GuardMode>) -> Decision {
    match (guard, d) {
        (Some(GuardMode::Off), _) => Decision::Allow,
        (Some(GuardMode::Warn), Decision::Block) => Decision::Warn,
        _ => d,
    }
}

fn breach_text(b: &Value) -> String {
    match b {
        Value::String(s) => s.clone(),
        Value::Object(_) => str_at(b, &["message", "text", "detail", "summary"])
            .map(String::from)
            .unwrap_or_else(|| b.to_string()),
        other => other.to_string(),
    }
}

/// The text to give the agent: context, breaches, and (when the decision
/// was lowered or the client cannot ask) the guard's reason.
fn context_text(r: &HookReply, note: Option<String>) -> String {
    let mut parts: Vec<String> = r
        .context
        .iter()
        .filter(|c| !c.is_empty())
        .cloned()
        .collect();
    for b in &r.breaches {
        parts.push(format!("Mulchie limit breach: {}", breach_text(b)));
    }
    if let Some(n) = note {
        parts.push(n);
    }
    cap(&parts.join("\n"), MAX_CONTEXT)
}

fn reason_of(r: &HookReply) -> String {
    let base = r
        .reason
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mulchie's guard flagged this command".into());
    match &r.rule {
        Some(rule) if !rule.is_empty() => format!("{base} (Mulchie guard rule {rule})"),
        _ => base,
    }
}

/// What a client reads as "go ahead, nothing to add".
pub fn fail_open(client: HookClient, event: &str) -> Out {
    match (client, neutral_event(client, event)) {
        // Cursor blocks permission hooks that print invalid JSON.
        (HookClient::Cursor, "pre_tool") => Out::json(json!({ "permission": "allow" })),
        (HookClient::Cursor, "prompt") => Out::json(json!({ "continue": true })),
        // Codex requires JSON on stdout from Stop.
        (HookClient::Codex, "stop") => Out::json(json!({})),
        _ => Out::default(),
    }
}

fn specific(event_name: &str, extra: Value) -> Value {
    let mut h = json!({ "hookEventName": event_name });
    if let (Some(o), Some(e)) = (h.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    json!({ "hookSpecificOutput": h })
}

/// Translate the engine's reply into this client's documented answer.
pub fn translate(
    client: HookClient,
    event: &str,
    reply: &HookReply,
    guard: Option<GuardMode>,
) -> Out {
    use Decision::*;
    let ev = neutral_event(client, event);
    let asked = reply.decision();
    let d = capped(asked, guard);
    let reason = reason_of(reply);
    // A lowered decision, or a warning a client cannot ask about, becomes
    // advice for the agent.
    let lowered = (d < asked).then(|| format!("Mulchie noted: {reason}"));
    let can_ask = matches!(
        (client, ev),
        (
            HookClient::ClaudeCode | HookClient::Cursor | HookClient::Copilot,
            "pre_tool"
        )
    );
    let warn_note = (d == Warn && !can_ask).then(|| format!("Mulchie warns: {reason}"));
    let ctx = context_text(reply, lowered.or(warn_note));
    let has_ctx = !ctx.is_empty();
    let native = events_for(client)
        .iter()
        .find(|s| s.neutral == ev)
        .map(|s| s.native)
        .unwrap_or("");
    match client {
        HookClient::ClaudeCode | HookClient::Codex => {
            let codex = client == HookClient::Codex;
            match ev {
                "session_start" | "post_tool" if has_ctx => {
                    Out::json(specific(native, json!({ "additionalContext": ctx })))
                }
                "prompt" if d == Block && codex => Out {
                    stderr: format!("{reason}\n"),
                    code: 2,
                    ..Out::default()
                },
                "prompt" if d == Block => {
                    Out::json(json!({ "decision": "block", "reason": reason }))
                }
                "prompt" if has_ctx => {
                    Out::json(specific(native, json!({ "additionalContext": ctx })))
                }
                "pre_tool" => {
                    let mut extra = serde_json::Map::new();
                    match d {
                        Block => {
                            extra.insert("permissionDecision".into(), json!("deny"));
                            extra.insert("permissionDecisionReason".into(), json!(reason));
                        }
                        // Codex has no "ask": the warning goes to the user
                        // and the agent as text.
                        Warn if !codex => {
                            extra.insert("permissionDecision".into(), json!("ask"));
                            extra.insert("permissionDecisionReason".into(), json!(reason));
                        }
                        _ => {}
                    }
                    if has_ctx {
                        extra.insert("additionalContext".into(), json!(ctx));
                    }
                    if extra.is_empty() {
                        return Out::default();
                    }
                    let mut v = specific(native, Value::Object(extra));
                    if codex && d == Warn {
                        v["systemMessage"] = json!(format!("Mulchie warns: {reason}"));
                    }
                    Out::json(v)
                }
                _ => fail_open(client, event),
            }
        }
        HookClient::Cursor => match ev {
            "session_start" if has_ctx => Out::json(json!({ "additional_context": ctx })),
            "prompt" if d == Block => {
                Out::json(json!({ "continue": false, "user_message": reason }))
            }
            "pre_tool" => {
                let permission = match d {
                    Block => "deny",
                    Warn => "ask",
                    Allow => "allow",
                };
                let mut v = json!({ "permission": permission });
                if d != Allow {
                    v["user_message"] = json!(reason);
                }
                let agent = match (d != Allow, has_ctx) {
                    (true, true) => format!("{reason}\n{ctx}"),
                    (true, false) => reason.clone(),
                    (false, true) => ctx.clone(),
                    (false, false) => String::new(),
                };
                if !agent.is_empty() {
                    v["agent_message"] = json!(agent);
                }
                Out::json(v)
            }
            _ => fail_open(client, event),
        },
        HookClient::Gemini => match ev {
            "session_start" | "prompt" | "post_tool" if d != Block && has_ctx => {
                Out::json(specific(native, json!({ "additionalContext": ctx })))
            }
            "prompt" | "pre_tool" if d == Block => {
                Out::json(json!({ "decision": "deny", "reason": reason }))
            }
            // No "ask" and no agent context before a tool: tell the user.
            // Plain advice waits for AfterTool, which can add context.
            "pre_tool" if d == Warn => Out::json(json!({
                "decision": "allow",
                "systemMessage": format!("Mulchie warns: {reason}"),
            })),
            _ => Out::default(),
        },
        // Copilot: a non-zero exit from preToolUse denies the call, so the
        // answer is always JSON with exit 0.
        HookClient::Copilot => match ev {
            "session_start" if has_ctx => Out::json(json!({ "additionalContext": ctx })),
            "pre_tool" if d != Allow => Out::json(json!({
                "permissionDecision": if d == Block { "deny" } else { "ask" },
                "permissionDecisionReason": reason,
            })),
            _ => Out::default(),
        },
    }
}

/// Parse the input, ask the engine through `call`, translate.
pub fn handle(
    client: HookClient,
    event: &str,
    input: &[u8],
    guard: Option<GuardMode>,
    pid: u32,
    call: &dyn Fn(&HookRequest) -> Result<Value, String>,
) -> Result<Out, String> {
    if input.len() as u64 > MAX_INPUT {
        return Err(format!("input over {MAX_INPUT} bytes"));
    }
    let v: Value = if input.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(input).map_err(|e| format!("input is not JSON: {e}"))?
    };
    if !v.is_object() {
        return Err("input is not a JSON object".into());
    }
    let req = request(client, event, &v, pid);
    let reply = call(&req)?;
    let reply: HookReply =
        serde_json::from_value(reply).map_err(|e| format!("unexpected reply: {e}"))?;
    Ok(translate(client, event, &reply, guard))
}

/// Run `work` on its own thread and wait at most `budget` for it.
pub fn with_budget<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    match rx.recv_timeout(budget) {
        Ok(r) => r,
        Err(_) => Err(format!("no answer within {} ms", budget.as_millis())),
    }
}

/// One line in `<config dir>/hook.log`, at most once a minute; the file is
/// started again when it grows past 256 KiB.
pub fn log_failure(paths: &Paths, line: &str) {
    let path = paths.config_dir.join("hook.log");
    if let Ok(m) = std::fs::metadata(&path) {
        let recent = m
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < LOG_EVERY);
        if recent {
            return;
        }
        if m.len() > LOG_MAX {
            let _ = std::fs::remove_file(&path);
        }
    }
    if mulchie_core::config::create_private_dir(&paths.config_dir).is_err() {
        return;
    }
    append(
        &path,
        &format!("{} {line}\n", mulchie_core::monitor::now_unix()),
    );
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// The CLI entry point.
pub fn command(paths: &Paths, client: HookClient, event: &str, guard: Option<GuardMode>) -> i32 {
    let started = Instant::now();
    let p = paths.clone();
    let ev = event.to_string();
    // SAFETY: getppid has no preconditions.
    let pid = unsafe { libc::getppid() } as u32;
    let r = with_budget(BUDGET, move || {
        let mut input = Vec::new();
        let mut stdin = std::io::stdin().lock();
        (&mut stdin)
            .take(MAX_INPUT + 1)
            .read_to_end(&mut input)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        if input.len() as u64 > MAX_INPUT {
            // Read the rest and drop it, so the client is not left writing
            // into a closed pipe; the budget still bounds the time.
            let _ = std::io::copy(&mut stdin, &mut std::io::sink());
        }
        let call = |req: &HookRequest| -> Result<Value, String> {
            let c = match conn::connect_as(&p, true) {
                Conn::Engine(c) => c,
                other => return Err(other.why_not()),
            };
            let body = serde_json::to_value(req).map_err(|e| e.to_string())?;
            conn::call(&c, "POST", ROUTE, Some(&body)).map_err(|e| conn::describe(&e))
        };
        handle(client, &ev, &input, guard, pid, &call)
    });
    let out = match r {
        Ok(o) => o,
        Err(e) => {
            log_failure(
                paths,
                &format!(
                    "{} {event}: allowed without the engine after {} ms: {e}",
                    client.name(),
                    started.elapsed().as_millis()
                ),
            );
            fail_open(client, event)
        }
    };
    emit(&out)
}

/// `mulchie hook …` with arguments this build cannot read: settings written
/// by a newer Mulchie, run by an older binary. A usage error would exit 2,
/// which Claude Code and Codex read as a block, so instead log it, print the
/// client's fail-open output when the client is known (nothing otherwise),
/// and exit 0. `args` is the whole command line, program name first.
pub fn usage_error(paths: &Paths, args: &[String], err: &str) -> Out {
    use clap::ValueEnum;
    let client = args.get(2).and_then(|c| HookClient::from_str(c, true).ok());
    let event = args.get(3).map(String::as_str).unwrap_or("");
    let first = err.lines().next().unwrap_or("").trim();
    log_failure(
        paths,
        &format!(
            "hook {}: allowed; this mulchie cannot read these arguments ({first}); is an older mulchie first on the PATH?",
            args.get(2..).map(|a| a.join(" ")).unwrap_or_default()
        ),
    );
    match client {
        Some(c) => fail_open(c, event),
        None => Out::default(),
    }
}

/// Print `out` and return its exit status.
pub fn emit(out: &Out) -> i32 {
    use std::io::Write;
    let _ = std::io::stdout().write_all(out.stdout.as_bytes());
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(out.stderr.as_bytes());
    out.code
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
