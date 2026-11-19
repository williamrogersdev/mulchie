//! `mulchie issues`: findings grouped into issues across processes and
//! restarts ([`mulchie_core::issues`]).
//!
//! Listing works without an engine (it reads the saved book, as the last
//! engine left it); changing an issue needs the engine, which keeps the book.

use crate::cli::IssuesCommand;
use crate::commands::{fail, print_json, s, truncate, u};
use crate::conn::{self, Conn};
use mulchie_core::issues::{IssueBook, IssueStatus};
use mulchie_core::Paths;
use serde_json::{json, Value};

/// Said when the list comes from the saved file rather than the engine.
const SAVED_NOTE: &str = "Read from the saved issues file: no engine is running, so nothing new has been counted since it stopped.";

fn list_value(paths: &Paths, status: Option<&str>, kind: Option<&str>) -> Result<Value, String> {
    if let Some(k) = kind {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(format!(
                "--kind {k:?} is not a finding kind (snake_case, e.g. runaway_cpu)"
            ));
        }
    }
    match conn::connect(paths) {
        Conn::Engine(c) => {
            let mut q = Vec::new();
            if let Some(st) = status {
                q.push(format!("status={st}"));
            }
            if let Some(k) = kind {
                q.push(format!("kind={k}"));
            }
            let path = if q.is_empty() {
                "/v1/issues".to_string()
            } else {
                format!("/v1/issues?{}", q.join("&"))
            };
            conn::call(&c, "GET", &path, None).map_err(|e| conn::describe(&e))
        }
        other => {
            other.note_local("reading the saved issues");
            let book = IssueBook::load(paths);
            let all = status == Some("all");
            let st: Option<IssueStatus> = match status.filter(|s| *s != "all") {
                Some(x) => Some(serde_json::from_value(json!(x)).map_err(|e| e.to_string())?),
                None => None,
            };
            let k = kind
                .map(|k| {
                    serde_json::from_value(json!(k))
                        .map_err(|_| format!("--kind {k:?} is not a finding kind"))
                })
                .transpose()?;
            Ok(json!({
                "issues": book.list(st, all, k),
                "counts": book.counts(),
                "local": true,
                "local_note": SAVED_NOTE,
            }))
        }
    }
}

pub fn list(paths: &Paths, status: Option<&str>, kind: Option<&str>, json_out: bool) -> i32 {
    let v = match list_value(paths, status, kind) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    print!("{}", render_list(&v, mulchie_core::monitor::now_unix()));
    0
}

/// `3m`, `5h`, `2d`.
fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

pub fn render_list(v: &Value, now: u64) -> String {
    let mut out = String::new();
    if let Some(n) = v["local_note"].as_str() {
        out.push_str(&format!("{n}\n"));
    }
    let list = v["issues"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        out.push_str("No issues.\n");
        return out;
    }
    for i in &list {
        out.push_str(&render_one(i, now));
    }
    out
}

fn render_one(i: &Value, now: u64) -> String {
    let active = if i["active"] == true {
        ", happening now"
    } else {
        ""
    };
    let mut out = format!(
        "[{}] {}{active}\n    {} · seen {}× · first {} ago · last {} ago\n",
        s(i, "status"),
        truncate(s(i, "title"), 100),
        s(i, "kind"),
        u(i, "occurrences"),
        ago(now.saturating_sub(u(i, "first_seen_unix"))),
        ago(now.saturating_sub(u(i, "last_seen_unix"))),
    );
    out.push_str(&format!("    about: {}\n", truncate(s(i, "culprit"), 100)));
    if let Some(w) = i["workspace"].as_str() {
        out.push_str(&format!("    workspace: {w}\n"));
    }
    if u(i, "regressions") > 0 {
        out.push_str(&format!(
            "    came back {} time(s) after it was resolved\n",
            u(i, "regressions")
        ));
    }
    out.push_str(&format!("    id: {}\n", s(i, "id")));
    out
}

fn check_id(id: &str) -> Result<(), String> {
    if id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not an issue id (16 hex digits, from `mulchie issues`)"
        ))
    }
}

pub fn action(paths: &Paths, a: &IssuesCommand) -> i32 {
    let (id, json_out) = match a {
        IssuesCommand::Show { id, out }
        | IssuesCommand::Resolve { id, out }
        | IssuesCommand::Ignore { id, out }
        | IssuesCommand::Reopen { id, out } => (id.as_str(), out.json),
    };
    if let Err(e) = check_id(id) {
        return fail(e);
    }
    let now = mulchie_core::monitor::now_unix();
    if let IssuesCommand::Show { .. } = a {
        let v = match conn::connect(paths) {
            Conn::Engine(c) => match conn::call(&c, "GET", &format!("/v1/issues/{id}"), None) {
                Ok(v) => v,
                Err(e) if e.status == 404 => return fail(format!("no issue {id}")),
                Err(e) => return fail(conn::describe(&e)),
            },
            other => {
                other.note_local("reading the saved issues");
                match IssueBook::load(paths).get(id) {
                    Some(i) => serde_json::to_value(i).unwrap_or(Value::Null),
                    None => return fail(format!("no issue {id}")),
                }
            }
        };
        if json_out {
            print_json(&v);
        } else {
            print!("{}", render_one(&v, now));
        }
        return 0;
    }
    let path = match a {
        IssuesCommand::Resolve { .. } => "/v1/issues/resolve",
        IssuesCommand::Ignore { .. } => "/v1/issues/ignore",
        _ => "/v1/issues/reopen",
    };
    let c = match conn::connect(paths) {
        Conn::Engine(c) => c,
        other => return fail(other.explain_required("changing an issue")),
    };
    let r = match conn::call(&c, "POST", path, Some(&json!({ "id": id }))) {
        Ok(v) => v,
        Err(e) if e.status == 404 => return fail(format!("no issue {id}")),
        Err(e) => return fail(conn::describe(&e)),
    };
    if json_out {
        print_json(&r);
    } else {
        println!(
            "{id} is now {}: {}",
            s(&r["issue"], "status"),
            s(&r["issue"], "title")
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_an_issue_in_words() {
        let v = json!({ "issues": [{
            "id": "0123456789abcdef", "kind": "runaway_cpu", "status": "regressed",
            "title": "node next dev used a full core for 20 minutes", "culprit": "node next dev",
            "workspace": "/code/site", "occurrences": 4, "regressions": 1, "active": true,
            "first_seen_unix": 1_000_000 - 2 * 86_400, "last_seen_unix": 1_000_000 - 120,
        }]});
        let t = render_list(&v, 1_000_000);
        assert!(t.contains("[regressed] node next dev used"), "{t}");
        assert!(t.contains("happening now"), "{t}");
        assert!(t.contains("seen 4× · first 2d ago · last 2m ago"), "{t}");
        assert!(t.contains("came back 1 time(s)"), "{t}");
        assert!(t.contains("id: 0123456789abcdef"), "{t}");
        assert_eq!(render_list(&json!({ "issues": [] }), 0), "No issues.\n");
    }

    #[test]
    fn ids_are_checked_before_any_call() {
        assert!(check_id("0123456789abcdef").is_ok());
        assert!(check_id("../../state").is_err());
        assert!(check_id("0123").is_err());
    }
}
