//! `mulchie slots`: list the build slots, and release one whose holder is
//! gone. Releasing uses the full API token, which the engine accepts without
//! the lease's release secret.

use crate::commands::{fail, print_json};
use crate::conn::{self, Conn};
use mulchie_core::api::ApiError;
use mulchie_core::Paths;
use serde_json::{json, Value};
use std::io::{BufRead, IsTerminal, Write};

/// A call to the engine's API.
type Call<'a> = dyn Fn(&str, &str, Option<&Value>) -> Result<Value, ApiError> + 'a;

fn engine(paths: &Paths, what: &str) -> Result<mulchie_core::api::Client, i32> {
    match conn::connect(paths) {
        Conn::Engine(c) => Ok(c),
        other => Err(fail(other.explain_required(what))),
    }
}

pub fn list(paths: &Paths, json_out: bool) -> i32 {
    let c = match engine(paths, "`mulchie slots`") {
        Ok(c) => c,
        Err(code) => return code,
    };
    let state = match conn::call(&c, "GET", "/v1/state", None) {
        Ok(v) => v,
        Err(e) => return fail(conn::describe(&e)),
    };
    let slots = &state["slots"];
    if json_out {
        print_json(slots);
    } else {
        print!("{}", render(slots, mulchie_core::monitor::now_unix()));
    }
    0
}

/// The slots as text.
pub fn render(slots: &Value, now: u64) -> String {
    let active = slots["active"].as_array().cloned().unwrap_or_default();
    let waiting = slots["waiting"].as_array().map_or(0, Vec::len);
    let mut s = format!(
        "{} of {} build slot(s) in use, {waiting} waiting.\n",
        active.len(),
        slots["capacity"].as_u64().unwrap_or(0)
    );
    if active.is_empty() {
        return s;
    }
    s.push_str(&format!(
        "  {:>7}  {:<28}  {:>7}  {:>8}  {}\n",
        "ticket", "label", "pid", "age", "ends"
    ));
    for l in &active {
        let age = now.saturating_sub(l["since_unix"].as_u64().unwrap_or(now));
        let ends = match (l["expires_unix"].as_u64(), l["pid"].as_u64()) {
            (Some(e), _) => format!("in {}", mins(e.saturating_sub(now))),
            (None, Some(_)) => "when the pid exits".into(),
            (None, None) => "when released".into(),
        };
        s.push_str(&format!(
            "  {:>7}  {:<28}  {:>7}  {:>8}  {ends}\n",
            l["id"],
            l["label"].as_str().unwrap_or(""),
            l["pid"].as_u64().map_or("-".into(), |p| p.to_string()),
            mins(age),
        ));
    }
    s.push_str("Release one whose holder is gone with `mulchie slots release <ticket>`.\n");
    s
}

fn mins(secs: u64) -> String {
    if secs < 120 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

pub fn release(paths: &Paths, ticket: u64, yes: bool, json_out: bool) -> i32 {
    let c = match engine(paths, "`mulchie slots release`") {
        Ok(c) => c,
        Err(code) => return code,
    };
    let call = |m: &str, p: &str, b: Option<&Value>| conn::call(&c, m, p, b);
    let mut ask = |q: &str| -> bool {
        eprint!("{q} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let interactive = std::io::stdin().is_terminal();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run_release(
        &call,
        ticket,
        yes,
        interactive,
        &mut ask,
        json_out,
        &mut out,
        &mut err,
    )
}

/// Release `ticket` after confirming. Exit status: 0 released or declined,
/// 1 failed or no such slot, 2 confirmation needed and impossible.
#[allow(clippy::too_many_arguments)]
pub fn run_release(
    call: &Call,
    ticket: u64,
    yes: bool,
    interactive: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let state = match call("GET", "/v1/state", None) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "mulchie: {}", conn::describe(&e));
            return 1;
        }
    };
    let Some(lease) = state["slots"]["active"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["id"].as_u64() == Some(ticket))
        .cloned()
    else {
        let _ = writeln!(
            err,
            "mulchie: no build slot has ticket {ticket} (it may have ended already; `mulchie slots` lists them)"
        );
        return 1;
    };
    let who = format!(
        "slot {ticket} ({}{})",
        lease["label"].as_str().unwrap_or("no label"),
        lease["pid"]
            .as_u64()
            .map_or(String::new(), |p| format!(", pid {p}"))
    );
    if !yes {
        if !interactive {
            let _ = writeln!(
                err,
                "mulchie: stdin is not a terminal, so nothing was changed; re-run with --yes to release {who}"
            );
            return 2;
        }
        if !ask(&format!(
            "Release {who}? Its holder loses the slot even if it is still building."
        )) {
            let _ = writeln!(err, "mulchie: nothing changed");
            return 0;
        }
    }
    match call("POST", "/v1/release", Some(&json!({ "ticket": ticket }))) {
        Ok(v) => {
            let released = v["released"] == true;
            if json_out {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "ticket": ticket, "released": released, "admin": v["admin"] })
                    )
                    .unwrap_or_default()
                );
            } else if released {
                let _ = writeln!(out, "Released {who}.");
            } else {
                let _ = writeln!(out, "Slot {ticket} had already ended.");
            }
            0
        }
        Err(e) if e.code == "release_secret_required" || e.code == "wrong_release_secret" => {
            let _ = writeln!(
                err,
                "mulchie: this engine needs the slot's release secret, which only its holder has, so it is older than this CLI; wait for the slot's TTL to pass, or restart the engine (or quit and reopen the app) to clear all slots [{}]",
                e.code
            );
            1
        }
        Err(e) => {
            let _ = writeln!(err, "mulchie: {}", conn::describe(&e));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn state() -> Value {
        json!({ "slots": { "capacity": 2, "jobs_per_slot": 4, "waiting": [],
            "active": [{ "id": 7, "label": "mcp agent", "pid": null, "since_unix": 1000,
                "jobs": 4, "ttl_secs": 900, "expires_unix": 1900 }] } })
    }

    fn go(
        reply: Result<Value, ApiError>,
        ticket: u64,
        yes: bool,
        interactive: bool,
        answer: bool,
    ) -> (i32, String, String, Vec<Value>) {
        let posted = RefCell::new(Vec::new());
        let call = |_m: &str, p: &str, b: Option<&Value>| -> Result<Value, ApiError> {
            if p == "/v1/release" {
                posted.borrow_mut().push(b.cloned().unwrap_or(Value::Null));
                return reply.clone();
            }
            Ok(state())
        };
        let mut ask = |_: &str| answer;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_release(
            &call,
            ticket,
            yes,
            interactive,
            &mut ask,
            false,
            &mut out,
            &mut err,
        );
        let sent = posted.borrow().clone();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            sent,
        )
    }

    fn api_err(status: u16, code: &str) -> ApiError {
        ApiError {
            status,
            code: code.into(),
            message: "x".into(),
        }
    }

    #[test]
    fn release_sends_no_secret_and_confirms_first() {
        let ok = || Ok(json!({ "released": true, "admin": true }));
        let (code, out, _, sent) = go(ok(), 7, true, false, false);
        assert_eq!(code, 0);
        assert!(out.contains("Released slot 7 (mcp agent)"), "{out}");
        assert_eq!(sent, vec![json!({ "ticket": 7 })]);
        // Declined: nothing sent.
        let (code, _, err, sent) = go(ok(), 7, false, true, false);
        assert_eq!(code, 0);
        assert!(err.contains("nothing changed") && sent.is_empty());
        // Not a terminal and no --yes: nothing sent, status 2.
        let (code, _, err, sent) = go(ok(), 7, false, false, true);
        assert_eq!(code, 2);
        assert!(err.contains("--yes") && sent.is_empty());
        // Unknown ticket: refused before anything is sent.
        let (code, _, err, sent) = go(ok(), 8, true, false, true);
        assert_eq!(code, 1);
        assert!(err.contains("no build slot has ticket 8") && sent.is_empty());
    }

    #[test]
    fn an_engine_that_wants_the_secret_is_explained() {
        let (code, _, err, _) = go(
            Err(api_err(403, "release_secret_required")),
            7,
            true,
            false,
            false,
        );
        assert_eq!(code, 1);
        assert!(
            err.contains("older than this CLI") && err.contains("restart the engine"),
            "{err}"
        );
    }

    #[test]
    fn the_list_shows_ticket_label_and_expiry() {
        let t = render(&state()["slots"], 1300);
        assert!(t.contains("1 of 2 build slot(s) in use"), "{t}");
        assert!(t.contains("mcp agent") && t.contains("in 10m"), "{t}");
    }
}
