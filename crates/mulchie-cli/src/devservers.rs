//! `mulchie devservers`: each project's dev server, run by run
//! ([`mulchie_core::devservers`]).
//!
//! Works without an engine: it then reads `devservers.jsonl` as the last
//! engine left it (the newest record of each run).

use crate::commands::{fail, print_json, s, truncate, u};
use crate::conn::{self, Conn};
use mulchie_core::api::query_escape;
use mulchie_core::perf::human_ms;
use mulchie_core::{human_bytes, Config, Engine, Paths};
use serde_json::Value;
use std::path::{Path, PathBuf};

const SAVED_NOTE: &str =
    "Read from the saved dev server file: no engine is running, so nothing new has been recorded since it stopped.";

fn list_value(paths: &Paths, workspace: Option<&Path>, limit: usize) -> Result<Value, String> {
    match conn::connect(paths) {
        Conn::Engine(c) => {
            let mut q = vec![format!("limit={limit}")];
            if let Some(w) = workspace {
                q.push(format!("workspace={}", query_escape(&w.to_string_lossy())));
            }
            let path = format!("/v1/devservers?{}", q.join("&"));
            conn::call(&c, "GET", &path, None).map_err(|e| conn::describe(&e))
        }
        other => {
            other.note_local("reading the saved dev server records");
            let e = Engine::new(paths.clone(), Config::load_from(paths));
            let mut v =
                serde_json::to_value(e.dev_servers(workspace, limit)).map_err(|e| e.to_string())?;
            v["local"] = Value::Bool(true);
            v["local_note"] = Value::String(SAVED_NOTE.into());
            Ok(v)
        }
    }
}

pub fn list(paths: &Paths, workspace: Option<&Path>, limit: usize, json_out: bool) -> i32 {
    let ws: Option<PathBuf> = match workspace {
        None => None,
        Some(w) if w.is_absolute() => Some(w.to_path_buf()),
        Some(w) => match std::env::current_dir() {
            Ok(d) => Some(d.join(w)),
            Err(e) => return fail(format!("cannot resolve {}: {e}", w.display())),
        },
    };
    let v = match list_value(paths, ws.as_deref(), limit) {
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
    let list = v["dev_servers"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        out.push_str(
            "No dev servers recorded. A dev server is a process in a workspace that listens on a TCP port; it is recorded once it has been up for a minute.\n",
        );
        return out;
    }
    for d in &list {
        let state = match s(d, "end") {
            "exited" => "exited",
            _ => "running",
        };
        let listen = match d["listen_ms"].as_u64() {
            Some(ms) => format!(
                "listen <={} (+{})",
                human_ms(ms),
                human_ms(u(d, "listen_uncertainty_ms"))
            ),
            None => "listen ?".to_string(),
        };
        let mem = match d["settled_mem_bytes"].as_u64() {
            Some(b) => format!("settled {}", human_bytes(b)),
            None => "settled ?".to_string(),
        };
        let slope = match d["mem_slope_mb_per_hour"].as_f64() {
            Some(m) => format!("{m:+.0} MB/h"),
            None => "- MB/h".to_string(),
        };
        let place = d["workspace"]
            .as_str()
            .and_then(|w| Path::new(w).file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "-".into());
        let ports = d["ports"]
            .as_array()
            .map(|p| {
                p.iter()
                    .filter_map(Value::as_u64)
                    .map(|p| format!(":{p}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let head = d["head"]
            .as_str()
            .map(|h| h.get(..10).unwrap_or(h))
            .unwrap_or("");
        let by = d["agent"]
            .as_str()
            .map(|a| format!(" · {a}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:<7} {:<22} {:<16} {:<9} {:>9} {:>5.0}% cpu  {:<14} {} {}{by}\n",
            state,
            listen,
            mem,
            slope,
            human_ms(u(d, "run_secs") * 1000),
            d["peak_cpu_pct"].as_f64().unwrap_or(0.0),
            truncate(&place, 14),
            truncate(s(d, "command"), 50),
            format!("{ports} {head}").trim_end(),
        ));
    }
    let total = u(v, "total");
    if total as usize > list.len() {
        out.push_str(&format!(
            "{} of {total} shown; use --limit for more.\n",
            list.len()
        ));
    }
    out.push_str(
        "Time to listen is an upper bound, read from the watch run that first saw the port.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_runs_and_an_empty_list() {
        let v = json!({ "dev_servers": [], "total": 0 });
        assert!(render(&v).contains("No dev servers"));
        let v = json!({ "total": 3, "dev_servers": [
            { "end": "running", "listen_ms": 18000, "listen_uncertainty_ms": 61000,
              "settled_mem_bytes": 640000000, "mem_slope_mb_per_hour": 12.0,
              "peak_cpu_pct": 180.0, "run_secs": 7200, "command": "next dev",
              "workspace": "/w/repo-x", "ports": [3000], "head": "0123456789abcdef",
              "agent": "claude" },
            { "end": "exited", "command": "vite", "run_secs": 90 },
        ]});
        let r = render(&v);
        assert!(
            r.contains("listen <=18s (+1m1s)") && r.contains("640 MB") && r.contains("+12 MB/h"),
            "{r}"
        );
        assert!(
            r.contains("repo-x") && r.contains(":3000 0123456789 · claude"),
            "{r}"
        );
        assert!(r.contains("exited") && r.contains("listen ?"), "{r}");
        assert!(
            r.contains("2 of 3 shown") && r.contains("upper bound"),
            "{r}"
        );
    }

    #[test]
    fn without_an_engine_the_saved_file_is_read() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::under(t.path());
        let v = list_value(&paths, None, 10).unwrap();
        assert_eq!(v["local"], true);
        assert_eq!(v["total"], 0);
    }
}
