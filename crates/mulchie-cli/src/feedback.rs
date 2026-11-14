//! `mulchie feedback`: report a problem, or read what was reported.
//!
//! A report goes through the engine when one runs (it adds the machine's
//! state) and straight to the feedback file when none does.

use crate::commands::{fail, print_json};
use crate::conn::{self, Conn};
use mulchie_core::feedback::{record_local, Report, Store};
use mulchie_core::Paths;
use serde_json::{json, Value};

pub struct ReportArgs<'a> {
    pub summary: &'a [String],
    pub category: Option<&'a str>,
    pub severity: Option<&'a str>,
    pub details: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub client: Option<&'a str>,
    pub json: bool,
}

pub fn report(paths: &Paths, a: &ReportArgs) -> i32 {
    let mut body = json!({
        "summary": a.summary.join(" "),
        "source": "cli",
        "cwd": std::env::current_dir().ok().map(|d| d.display().to_string()),
        "client": a.client,
    });
    for (k, v) in [
        ("category", a.category),
        ("severity", a.severity),
        ("details", a.details),
        ("tool", a.tool),
    ] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    let entry: Value = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "POST", "/v1/feedback", Some(&body)) {
            Ok(v) => v["feedback"].clone(),
            Err(e) => return fail(conn::describe(&e)),
        },
        // No usable engine: keep the report anyway.
        _ => {
            let r: Report = match serde_json::from_value(body) {
                Ok(r) => r,
                Err(e) => return fail(e),
            };
            match record_local(paths, r, "cli") {
                Ok(e) => serde_json::to_value(e).unwrap_or(Value::Null),
                Err(e) => return fail(e),
            }
        }
    };
    if a.json {
        print_json(&entry);
    } else {
        println!(
            "Saved feedback {} in {}",
            entry["id"].as_str().unwrap_or("?"),
            Store::new(paths).path().display()
        );
        println!("It stays on this machine. `mulchie feedback list` shows it.");
    }
    0
}

pub fn list(paths: &Paths, limit: usize, json_out: bool) -> i32 {
    let store = Store::new(paths);
    // Read from the file: the engine reads the same one.
    let (entries, total) = store.recent(limit);
    if json_out {
        print_json(&json!({ "feedback": entries, "total": total, "path": store.path() }));
        return 0;
    }
    print!(
        "{}",
        render(
            &serde_json::to_value(&entries).unwrap_or(Value::Null),
            total
        )
    );
    if total > 0 {
        println!("File: {}", store.path().display());
    }
    0
}

/// The list in words, newest first.
pub fn render(entries: &Value, total: usize) -> String {
    let list = entries.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No feedback yet. Agents report with the report_feedback tool; you can with `mulchie feedback report <what happened>`.\n".into();
    }
    let now = mulchie_core::monitor::now_unix();
    let mut out = format!(
        "Feedback: {} of {total} report(s), newest first\n",
        list.len()
    );
    for e in &list {
        let s = |k: &str| e[k].as_str().unwrap_or("");
        let age = now.saturating_sub(e["at_unix"].as_u64().unwrap_or(now));
        let ago = match age {
            0..=119 => format!("{age}s ago"),
            120..=7199 => format!("{}m ago", age / 60),
            7200..=172_799 => format!("{}h ago", age / 3600),
            _ => format!("{}d ago", age / 86_400),
        };
        out.push_str(&format!(
            "\n[{}] {} · {} · {} · {}\n  {}\n",
            s("severity"),
            s("category"),
            if s("client").is_empty() {
                s("source")
            } else {
                s("client")
            },
            ago,
            s("id"),
            s("summary"),
        ));
        if !s("tool").is_empty() {
            out.push_str(&format!("  about: {}\n", s("tool")));
        }
        for line in s("details").lines() {
            out.push_str(&format!("  | {line}\n"));
        }
        if !s("cwd").is_empty() {
            out.push_str(&format!("  in: {}\n", s("cwd")));
        }
        let m = &e["machine"];
        if m.is_object() {
            out.push_str(&format!(
                "  machine: health {}, CPU {:.0}%, load {:.0} on {} cores, memory {:.0}% free, {} session(s)\n",
                m["health"].as_str().unwrap_or("?"),
                m["cpu_pct"].as_f64().unwrap_or(0.0),
                m["load1"].as_f64().unwrap_or(0.0),
                m["cores"].as_u64().unwrap_or(0),
                m["mem_free_pct"].as_f64().unwrap_or(0.0),
                m["sessions"].as_u64().unwrap_or(0),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_reads_in_words() {
        assert!(render(&json!([]), 0).starts_with("No feedback yet"));
        let now = mulchie_core::monitor::now_unix();
        let out = render(
            &json!([{
                "id": "fb-1-abcd", "at_unix": now - 600, "source": "mcp", "client": "claude-code",
                "category": "wrong_advice", "severity": "high", "summary": "said good at load 50",
                "details": "one\ntwo", "tool": "diagnose", "cwd": "/code/a",
                "machine": { "health": "good", "cpu_pct": 85.0, "load1": 50.2, "cores": 16, "mem_free_pct": 80.0, "disk_free_gb": 200.0, "sessions": 31 }
            }]),
            3,
        );
        assert!(out.contains("1 of 3 report(s)"), "{out}");
        assert!(
            out.contains("[high] wrong_advice · claude-code · 10m ago · fb-1-abcd"),
            "{out}"
        );
        assert!(out.contains("  | two\n"), "{out}");
        assert!(out.contains("load 50 on 16 cores"), "{out}");
    }
}
