//! `signal` and `group`: ask the engine for a process plan, show it, confirm,
//! then execute exactly the requests of its ready items. Each process is
//! re-checked by its start time before it is touched, so a reused pid is
//! skipped. The same flow as `sweep`, with the same `--yes`, `--dry-run` and
//! `--json`.

use crate::cli::Confirm;
use crate::conn::{self, Conn};
use crate::plan::Ui;
use mulchie_core::Paths;
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};

/// What to plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// One process (and with `tree`, its descendants).
    Pid {
        pid: u32,
        signal: String,
        tree: bool,
    },
    /// A consumer group: `app:…`, `workspace:…` or `process:…`.
    Group { key: String, action: String },
    /// An agent session's processes; the agent itself with `include_agent`.
    Session {
        id: String,
        action: String,
        include_agent: bool,
    },
}

impl Target {
    fn body(&self) -> Value {
        // The core refuses groups holding this process or its ancestors (the
        // terminal it runs in, the agent app that started it).
        let me = std::process::id();
        match self {
            Target::Pid { pid, signal, tree } => {
                json!({ "pid": pid, "signal": signal, "tree": tree, "caller_pid": me })
            }
            Target::Group { key, action } => {
                json!({ "group": key, "action": action, "caller_pid": me })
            }
            Target::Session {
                id,
                action,
                include_agent,
            } => json!({
                "session": id, "action": action, "include_agent": include_agent, "caller_pid": me,
            }),
        }
    }

    /// A `mulchie group` key: `session:<id>` names a session, anything else
    /// a consumer group.
    pub fn from_key(key: &str, action: &str, include_agent: bool) -> Result<Target, String> {
        match key.strip_prefix("session:") {
            Some(id) => {
                crate::sessions::check_session_id(id)?;
                Ok(Target::Session {
                    id: id.into(),
                    action: action.into(),
                    include_agent,
                })
            }
            None if include_agent => Err("--include-agent goes with a session:<id> key".into()),
            None => Ok(Target::Group {
                key: key.into(),
                action: action.into(),
            }),
        }
    }

    fn what(&self) -> &'static str {
        match self {
            Target::Pid { .. } => "`mulchie signal`",
            Target::Group { .. } => "`mulchie group`",
            Target::Session { .. } => "`mulchie sessions stop`",
        }
    }
}

/// A call to the engine's API.
pub type Call<'a> = dyn Fn(&str, &str, Option<&Value>) -> Result<Value, String> + 'a;

/// The CLI entry point. Acting on processes needs the engine: it owns the
/// process table the plan was made from, and its journal records the action.
pub fn command(paths: &Paths, target: Target, confirm: Confirm, json_out: bool) -> i32 {
    let client = match conn::connect(paths) {
        Conn::Engine(c) => c,
        other => {
            eprintln!("mulchie: {}", other.explain_required(target.what()));
            return 1;
        }
    };
    let call = |m: &str, p: &str, b: Option<&Value>| {
        conn::call(&client, m, p, b).map_err(|e| {
            if p == "/v1/process/execute" {
                crate::plan::execute_error(&e)
            } else {
                conn::describe(&e)
            }
        })
    };
    let mut ask = |q: &str| -> bool {
        eprint!("{q} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut ui = Ui {
        confirm,
        json: json_out,
        interactive: std::io::stdin().is_terminal(),
        ask: &mut ask,
        out: &mut out,
        err: &mut err,
    };
    run(&call, &target, &mut ui)
}

/// Plan, show, confirm, execute exactly the ready items, report.
///
/// Exit status: 0 when everything approved was done, or for a dry run or a
/// declined plan; 1 when an item was skipped or a call failed; 2 when
/// confirmation was needed and impossible; 3 when the plan is refused or no
/// item in it is ready (with or without --dry-run), so nothing could be done.
pub fn run(call: &Call, target: &Target, ui: &mut Ui) -> i32 {
    let plan = match call("POST", "/v1/process/plan", Some(&target.body())) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(ui.err, "mulchie: {e}");
            return 1;
        }
    };
    let ready: Vec<Value> = plan["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["ready"] == true)
        .map(|i| i["request"].clone())
        .collect();
    let doable = plan["ready"] != false && !ready.is_empty();
    let emit_json = |ui: &mut Ui, results: Option<&Value>| {
        let v = json!({
            "source": "engine",
            "plan": plan,
            "executed": results.is_some(),
            "results": results,
        });
        let _ = writeln!(
            ui.out,
            "{}",
            serde_json::to_string_pretty(&v).unwrap_or_default()
        );
    };
    let show_text =
        !ui.json || (ui.interactive && !ui.confirm.yes && !ui.confirm.dry_run && doable);
    if show_text {
        let text = render_plan(&plan);
        let w: &mut dyn Write = if ui.json { &mut *ui.err } else { &mut *ui.out };
        let _ = w.write_all(text.as_bytes());
    }
    if !doable || ui.confirm.dry_run {
        if ui.json {
            emit_json(ui, None);
        }
        // The reason a plan is blocked is shown above (or in the JSON); the
        // status lets a script tell it from a plan that could be carried out.
        return if doable { 0 } else { 3 };
    }
    if !ui.confirm.yes {
        if !ui.interactive {
            let _ = writeln!(
                ui.err,
                "mulchie: stdin is not a terminal, so nothing was changed; re-run with --yes to carry out this plan, or --dry-run to only show it"
            );
            if ui.json {
                emit_json(ui, None);
            }
            return 2;
        }
        let q = format!(
            "{} {}?",
            capitalise(&action_words(&plan)),
            plan["name"].as_str().unwrap_or("these processes")
        );
        if !(ui.ask)(&q) {
            let _ = writeln!(ui.err, "mulchie: nothing changed");
            if ui.json {
                emit_json(ui, None);
            }
            return 0;
        }
    }
    let results = match call(
        "POST",
        "/v1/process/execute",
        Some(&json!({ "items": ready })),
    ) {
        Ok(r) if r.is_array() => r,
        Ok(_) => {
            let _ = writeln!(
                ui.err,
                "mulchie: unexpected reply from the engine; versions may differ. {}",
                crate::plan::MAY_HAVE_ACTED
            );
            return 1;
        }
        // The caller's error says whether the engine may have acted.
        Err(e) => {
            let _ = writeln!(ui.err, "mulchie: {e}");
            return 1;
        }
    };
    if ui.json {
        emit_json(ui, Some(&results));
    } else {
        let _ = ui.out.write_all(render_results(&plan, &results).as_bytes());
    }
    let all_done = results
        .as_array()
        .is_some_and(|r| r.iter().all(|r| r["status"] == "done"));
    i32::from(!all_done)
}

fn action_words(plan: &Value) -> String {
    plan["action"]
        .as_str()
        .unwrap_or("act on")
        .replace('_', " ")
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The plan as text: what would happen, to which processes, and why
/// anything is refused.
pub fn render_plan(plan: &Value) -> String {
    let mut s = String::new();
    let name = plan["name"].as_str().unwrap_or("");
    let target = plan["target"].as_str().unwrap_or("");
    s.push_str(&format!("Plan: {} {name} ({target})\n", action_words(plan)));
    if let Some(d) = plan["detail"].as_str().filter(|d| !d.is_empty()) {
        s.push_str(&format!("  {d}\n"));
    }
    let ready_none = plan["items"]
        .as_array()
        .is_none_or(|i| i.iter().all(|i| i["ready"] != true));
    if plan["ready"] == false || !plan["refused"].is_null() || ready_none {
        let why =
            crate::mcp_tools::blocked_reason(plan).unwrap_or_else(|| "nothing is ready".into());
        s.push_str(&format!("Blocked: {why}. Nothing would be done.\n"));
    }
    let affected = plan["affected"].as_array().cloned().unwrap_or_default();
    if !affected.is_empty() {
        s.push_str(&format!("Affected ({}):\n", affected.len()));
        for a in affected.iter().take(20) {
            let how = match a["how"].as_str().unwrap_or("") {
                "signalled" => "signalled",
                "exits_with_app" => "exits with the app",
                "reniced" => "lower priority",
                other => other,
            };
            s.push_str(&format!(
                "  {:>7}  {:<28}  {how}\n",
                a["pid"],
                a["name"].as_str().unwrap_or("?")
            ));
        }
        if affected.len() > 20 {
            s.push_str(&format!(
                "  … and {} more (--json lists them)\n",
                affected.len() - 20
            ));
        }
    }
    let not_ready: Vec<&Value> = plan["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["ready"] != true)
        .collect();
    if !not_ready.is_empty() {
        s.push_str(&format!("Not ready ({}):\n", not_ready.len()));
        for i in not_ready.iter().take(10) {
            s.push_str(&format!(
                "  skip   {}  {}\n",
                i["target"].as_str().unwrap_or(""),
                i["verdict"].as_str().unwrap_or("")
            ));
        }
    }
    if !plan["restart"].is_null() {
        s.push_str("Then it is opened again.\n");
    }
    s
}

/// Per-item results with skip reasons.
pub fn render_results(plan: &Value, results: &Value) -> String {
    let mut s = String::new();
    let (mut done, mut skipped) = (0, 0);
    for r in results.as_array().into_iter().flatten() {
        let target = r["target"].as_str().unwrap_or("");
        if r["status"] == "done" {
            done += 1;
            s.push_str(&format!("  done     {target}\n"));
        } else {
            skipped += 1;
            s.push_str(&format!(
                "  skipped  {target}: {} [{}]\n",
                r["detail"].as_str().unwrap_or(""),
                r["reason"].as_str().unwrap_or("")
            ));
        }
    }
    s.push_str(&format!(
        "{} {}: {done} done, {skipped} skipped.\n",
        capitalise(&action_words(plan)),
        plan["name"].as_str().unwrap_or("")
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn plan(ready: bool, refused: Option<&str>) -> Value {
        json!({
            "target": "pid 42", "name": "node", "action": "term", "ready": refused.is_none(),
            "refused": refused, "detail": "Ask node (42) to stop.",
            "items": [{ "ready": ready, "target": "pid 42", "verdict": if ready { "ok" } else { "protected" },
                "request": { "action": "signal", "pid": 42, "signal": "term", "expected_start_time": 5 } }],
            "affected": [{ "pid": 42, "name": "node", "start_time": 5, "how": "signalled" }],
        })
    }

    struct Rig {
        calls: RefCell<Vec<(String, Option<Value>)>>,
        plan: Value,
        reply: Option<Result<Value, String>>,
    }

    impl Rig {
        fn call(&self, _m: &str, p: &str, b: Option<&Value>) -> Result<Value, String> {
            self.calls.borrow_mut().push((p.into(), b.cloned()));
            if p == "/v1/process/execute" {
                if let Some(r) = &self.reply {
                    return r.clone();
                }
            }
            Ok(match p {
                "/v1/process/plan" => self.plan.clone(),
                _ => {
                    json!([{ "action": "signal", "target": "pid 42", "status": "done", "freed_bytes": 0, "backups": [] }])
                }
            })
        }
        fn executed(&self) -> Vec<Value> {
            self.calls
                .borrow()
                .iter()
                .filter(|(p, _)| p == "/v1/process/execute")
                .filter_map(|(_, b)| b.clone())
                .collect()
        }
    }

    fn go(
        rig: &Rig,
        confirm: Confirm,
        json: bool,
        interactive: bool,
        answer: bool,
    ) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut ask = |_: &str| answer;
        let mut ui = Ui {
            confirm,
            json,
            interactive,
            ask: &mut ask,
            out: &mut out,
            err: &mut err,
        };
        let t = Target::Pid {
            pid: 42,
            signal: "term".into(),
            tree: false,
        };
        let code = run(&|m, p, b| rig.call(m, p, b), &t, &mut ui);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn yes() -> Confirm {
        Confirm {
            yes: true,
            dry_run: false,
        }
    }

    #[test]
    fn executes_the_stored_requests_with_their_start_times() {
        let rig = Rig {
            calls: RefCell::default(),
            plan: plan(true, None),
            reply: None,
        };
        let (code, out, _) = go(&rig, yes(), false, false, false);
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("Ask node (42) to stop.") && out.contains("done     pid 42"),
            "{out}"
        );
        let sent = rig.executed();
        assert_eq!(sent[0]["items"][0]["expected_start_time"], 5);
        // The plan named the caller so the core can protect it.
        let asked = rig.calls.borrow()[0].1.clone().unwrap();
        assert_eq!(asked["caller_pid"], std::process::id());
    }

    #[test]
    fn dry_run_declined_and_non_interactive_change_nothing() {
        let rig = Rig {
            calls: RefCell::default(),
            plan: plan(true, None),
            reply: None,
        };
        let (code, _, _) = go(
            &rig,
            Confirm {
                yes: false,
                dry_run: true,
            },
            false,
            true,
            true,
        );
        assert_eq!(code, 0);
        let (code, _, err) = go(&rig, Confirm::default(), false, true, false);
        assert_eq!(code, 0);
        assert!(err.contains("nothing changed"));
        let (code, out, err) = go(&rig, Confirm::default(), true, false, true);
        assert_eq!(code, 2);
        assert!(err.contains("--yes"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["executed"], false);
        assert!(rig.executed().is_empty());
        // Confirmed interactively, it runs.
        let (code, _, _) = go(&rig, Confirm::default(), false, true, true);
        assert_eq!(code, 0);
        assert_eq!(rig.executed().len(), 1);
    }

    #[test]
    fn a_blocked_plan_shows_its_reason_and_is_not_executed() {
        let rig = Rig {
            calls: RefCell::default(),
            plan: plan(false, Some("protected")),
            reply: None,
        };
        let (code, out, _) = go(&rig, yes(), false, false, false);
        assert_eq!(code, 3);
        assert!(
            out.contains("Blocked: Ask node (42) to stop. [protected]"),
            "{out}"
        );
        assert!(rig.executed().is_empty());
        let (code, out, _) = go(&rig, yes(), true, false, false);
        assert_eq!(code, 3);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["plan"]["refused"], "protected");
        assert_eq!(v["executed"], false);
        // A dry run of a refused plan says so too.
        let dry = Confirm {
            yes: false,
            dry_run: true,
        };
        assert_eq!(go(&rig, dry, true, false, false).0, 3);
    }

    #[test]
    fn a_session_key_plans_the_session() {
        let t = Target::from_key("session:s-500-100", "pause", true).unwrap();
        let b = t.body();
        assert_eq!(b["session"], "s-500-100");
        assert_eq!(b["action"], "pause");
        assert_eq!(b["include_agent"], true);
        assert_eq!(b["caller_pid"], std::process::id());
        assert!(b.get("group").is_none());
        assert!(Target::from_key("session:../x", "quit", false).is_err());
        assert!(Target::from_key("app:/A.app", "quit", true).is_err());
        assert_eq!(
            Target::from_key("app:/A.app", "quit", false).unwrap(),
            Target::Group {
                key: "app:/A.app".into(),
                action: "quit".into()
            }
        );
    }

    #[test]
    fn a_plan_marked_not_ready_is_not_executed_even_with_a_ready_item() {
        let mut p = plan(true, None);
        p["ready"] = json!(false);
        let rig = Rig {
            calls: RefCell::default(),
            plan: p,
            reply: None,
        };
        let (code, out, _) = go(&rig, yes(), false, false, false);
        assert_eq!(code, 3, "{out}");
        assert!(rig.executed().is_empty());
    }

    #[test]
    fn a_skipped_result_exits_1() {
        let rig = Rig {
            calls: RefCell::default(),
            plan: plan(true, None),
            reply: Some(Ok(
                json!([{ "action": "signal", "target": "pid 42", "status": "skipped",
                "reason": "process_changed", "detail": "it is a different process now" }]),
            )),
        };
        let (code, out, _) = go(&rig, yes(), false, false, false);
        assert_eq!(code, 1);
        assert!(out.contains("skipped  pid 42"), "{out}");
    }

    #[test]
    fn a_failed_or_odd_execute_says_the_engine_may_have_acted() {
        use mulchie_core::api::ApiError;
        let timeout = ApiError::new(0, "timeout", "the engine did not answer in time");
        for reply in [
            Err(crate::plan::execute_error(&timeout)),
            Ok(json!({ "ok": true })),
        ] {
            let odd = reply.is_ok();
            let rig = Rig {
                calls: RefCell::default(),
                plan: plan(true, None),
                reply: Some(reply),
            };
            let (code, _, err) = go(&rig, yes(), false, false, false);
            assert_eq!(code, 1);
            assert!(
                err.contains("may have acted") && err.contains("mulchie status"),
                "{err}"
            );
            if odd {
                assert!(
                    err.contains("unexpected reply from the engine; versions may differ"),
                    "{err}"
                );
            }
        }
    }

    #[test]
    fn only_errors_after_which_the_engine_may_have_run_say_so() {
        use crate::plan::{execute_error, may_have_acted};
        use mulchie_core::api::ApiError;
        for (status, code, acted) in [
            (0, "timeout", true),
            (0, "io_error", true),
            (500, "internal", true),
            (0, "unreachable", false),
            (0, "engine_not_running", false),
            (400, "items_required", false),
            (401, "unauthorized", false),
            (403, "read_only_token", false),
            (409, "config_changed", false),
        ] {
            let e = ApiError::new(status, code, "x");
            assert_eq!(may_have_acted(&e), acted, "{status} {code}");
            assert_eq!(
                execute_error(&e).contains("may have acted"),
                acted,
                "{status} {code}"
            );
        }
    }
}
