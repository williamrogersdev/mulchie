//! `mulchie timings`: how long builds and tests run with `mulchie run` took
//! ([`mulchie_core::timings`]).
//!
//! Works without an engine: it then reads `timings.jsonl` as the last
//! engine left it.

use crate::commands::{fail, print_json, s, truncate, u};
use crate::conn::{self, Conn};
use mulchie_core::api::query_escape;
use mulchie_core::perf::human_ms;
use mulchie_core::{Config, Engine, Paths};
use serde_json::Value;
use std::path::{Path, PathBuf};

const SAVED_NOTE: &str =
    "Read from the saved timings file: no engine is running, so nothing new has been recorded since it stopped.";

fn list_value(
    paths: &Paths,
    workspace: Option<&Path>,
    command: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    match conn::connect(paths) {
        Conn::Engine(c) => {
            let mut q = vec![format!("limit={limit}")];
            if let Some(w) = workspace {
                q.push(format!("workspace={}", query_escape(&w.to_string_lossy())));
            }
            if let Some(cmd) = command {
                q.push(format!("command={}", query_escape(cmd)));
            }
            let path = format!("/v1/timings?{}", q.join("&"));
            conn::call(&c, "GET", &path, None).map_err(|e| conn::describe(&e))
        }
        other => {
            other.note_local("reading the saved timings");
            let e = Engine::new(paths.clone(), Config::load_from(paths));
            let mut v = serde_json::to_value(e.timings(workspace, command, limit))
                .map_err(|e| e.to_string())?;
            v["local"] = Value::Bool(true);
            v["local_note"] = Value::String(SAVED_NOTE.into());
            Ok(v)
        }
    }
}

pub fn list(
    paths: &Paths,
    workspace: Option<&Path>,
    command: Option<&str>,
    limit: usize,
    json_out: bool,
) -> i32 {
    let ws: Option<PathBuf> = match workspace {
        None => None,
        Some(w) if w.is_absolute() => Some(w.to_path_buf()),
        Some(w) => match std::env::current_dir() {
            Ok(d) => Some(d.join(w)),
            Err(e) => return fail(format!("cannot resolve {}: {e}", w.display())),
        },
    };
    let v = match list_value(paths, ws.as_deref(), command, limit) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if json_out {
        print_json(&v);
    } else {
        print!("{}", render(&v));
    }
    0
}

pub fn render(v: &Value) -> String {
    let mut out = String::new();
    if let Some(n) = v["local_note"].as_str() {
        out.push_str(&format!("{n}\n"));
    }
    let list = v["timings"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        out.push_str(
            "No timings recorded. Run builds and tests as `mulchie run -- <command>` to record them.\n",
        );
        return out;
    }
    for t in &list {
        let code = match t["exit_code"].as_i64() {
            Some(0) => "ok".to_string(),
            Some(c) => format!("exit {c}"),
            None => "?".to_string(),
        };
        let place = t["workspace"]
            .as_str()
            .and_then(|w| Path::new(w).file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "-".into());
        let head = t["head"].as_str().map(|h| h.get(..10).unwrap_or(h)).unwrap_or("");
        let by = t["agent"]
            .as_str()
            .map(|a| format!(" · {a}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:>8}  {:<7} {:<16} {} {}{by}\n",
            human_ms(u(t, "duration_ms")),
            code,
            truncate(&place, 16),
            truncate(s(t, "command"), 70),
            head,
        ));
    }
    let total = u(v, "total");
    if total as usize > list.len() {
        out.push_str(&format!(
            "{} of {total} shown; use --limit for more.\n",
            list.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_runs_and_an_empty_list() {
        let v = json!({ "timings": [], "total": 0 });
        assert!(render(&v).contains("mulchie run"));
        let v = json!({ "total": 3, "timings": [
            { "duration_ms": 250000, "exit_code": 0, "command": "cargo test",
              "workspace": "/w/repo-x", "head": "0123456789abcdef", "agent": "claude" },
            { "duration_ms": 1200, "exit_code": 2, "command": "make" },
        ]});
        let r = render(&v);
        assert!(r.contains("4m10s") && r.contains("repo-x") && r.contains("0123456789 · claude"), "{r}");
        assert!(r.contains("exit 2"), "{r}");
        assert!(r.contains("2 of 3 shown"), "{r}");
    }

    #[test]
    fn without_an_engine_the_saved_file_is_read() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let v = list_value(&paths, None, None, 10).unwrap();
        assert_eq!(v["local"], true);
        assert_eq!(v["total"], 0);
    }
}
