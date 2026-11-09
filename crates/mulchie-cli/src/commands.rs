//! The read commands, `prune`, `signal`, `config` and `serve`.

use crate::cli::{SortKey, TopArgs};
use crate::conn::{self, Conn};
use mulchie_core::{human_bytes, Config, Engine, Monitor, Paths};
use serde_json::{json, Value};
use std::time::Duration;

// ------------------------------------------------------------------ helpers

pub fn print_json(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

pub fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

pub fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Two samples 600 ms apart, so CPU and I/O rates mean something.
pub fn sample() -> mulchie_core::Snapshot {
    let mut m = Monitor::new();
    m.sample();
    std::thread::sleep(Duration::from_millis(600));
    m.sample()
}

/// An engine for one command, without loops, lock or API.
pub fn local_engine(paths: &Paths, scan: bool) -> Engine {
    let engine = Engine::new(paths.clone(), Config::load_from(paths));
    engine.set_snapshot(sample());
    if scan {
        engine.run_scan();
    }
    engine
}

/// The engine's state, or a local one (said on stderr).
fn state(paths: &Paths, scan: bool, what: &str) -> Result<Value, String> {
    match conn::connect(paths) {
        Conn::Engine(c) => conn::call(&c, "GET", "/v1/state", None).map_err(|e| conn::describe(&e)),
        other => {
            other.note_local(what);
            serde_json::to_value(local_engine(paths, scan).snapshot_state())
                .map_err(|e| e.to_string())
        }
    }
}

fn fail(msg: impl std::fmt::Display) -> i32 {
    eprintln!("mulchie: {msg}");
    1
}

// ------------------------------------------------------------------ status

pub fn status(paths: &Paths, json_out: bool) -> i32 {
    let st = match state(paths, false, "sampling this machine locally") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if json_out {
        print_json(&st);
        return 0;
    }
    let snap = &st["snapshot"];
    let mem_free = if f(snap, "mem_total") > 0.0 {
        f(snap, "mem_available") * 100.0 / f(snap, "mem_total")
    } else {
        100.0
    };
    println!(
        "health {}  ·  disk {} free of {}  ·  memory {:.0}% free  ·  cpu {:.0}% ({} cores)  ·  swap {}",
        s(&st, "health"),
        human_bytes(u(snap, "disk_free")),
        human_bytes(u(snap, "disk_total")),
        mem_free,
        f(snap, "cpu_pct"),
        u(snap, "cpu_count"),
        human_bytes(u(snap, "swap_used")),
    );
    if let Some(e) = st["config_error"].as_str() {
        eprintln!("mulchie: {e}; running with safe defaults (see `mulchie doctor`)");
    }
    if s(&st, "health") == "stale" {
        eprintln!(
            "mulchie: the engine has not sampled for {}s; its figures are old",
            u(&st, "sample_age_secs")
        );
    }
    println!(
        "{} agent process(es), {} build process(es) across {} workspace(s)\n",
        u(snap, "agent_count"),
        u(snap, "build_count"),
        snap["workspaces"].as_array().map_or(0, Vec::len)
    );
    for w in snap["workspaces"].as_array().into_iter().flatten().take(15) {
        println!(
            "  {:<48} {:>6.0}% cpu  {:>9}  {} agent · {} build",
            truncate(s(w, "label"), 48),
            f(w, "cpu_pct"),
            human_bytes(u(w, "mem_bytes")),
            u(w, "agents"),
            u(w, "builds"),
        );
    }
    0
}

// -------------------------------------------------------------------- scan

/// The engine's last cache scan, waiting up to `wait` for its first one.
fn engine_scan(c: &mulchie_core::api::Client, wait: Duration) -> Result<Value, String> {
    let get = || {
        conn::call(c, "GET", "/v1/state", None)
            .map(|st| st["scan"].clone())
            .map_err(|e| conn::describe(&e))
    };
    let scan = get()?;
    if !scan.is_null() {
        return Ok(scan);
    }
    eprintln!("mulchie: the engine has not finished a scan yet; asking it for one and waiting");
    let _ = conn::call(c, "POST", "/v1/scan", None);
    let start = std::time::Instant::now();
    while start.elapsed() < wait {
        std::thread::sleep(Duration::from_secs(1));
        let scan = get()?;
        if !scan.is_null() {
            return Ok(scan);
        }
    }
    Err(format!(
        "the engine's scan did not finish within {}s; try again later",
        wait.as_secs()
    ))
}

pub fn scan(paths: &Paths, json_out: bool) -> i32 {
    let r = match conn::connect(paths) {
        Conn::Engine(c) => match engine_scan(&c, Duration::from_secs(120)) {
            Ok(v) => v,
            Err(e) => return fail(e),
        },
        other => {
            other.note_local("scanning locally");
            let engine = Engine::new(paths.clone(), Config::load_from(paths));
            serde_json::to_value(engine.run_scan()).unwrap_or(Value::Null)
        }
    };
    if json_out {
        print_json(&r);
        return 0;
    }
    let items = r["items"].as_array().cloned().unwrap_or_default();
    println!(
        "{} reclaimable in {} caches, {} of it stale (scan took {:.1}s)\n",
        human_bytes(u(&r, "total_bytes")),
        items.len(),
        human_bytes(u(&r, "stale_bytes")),
        f(&r, "took_ms") / 1000.0
    );
    for i in items.iter().take(30) {
        let tag = if i["in_use"] == true {
            "in use"
        } else if i["stale"] == true {
            "stale"
        } else {
            "recent"
        };
        println!(
            "  {:>9}  {:<7} {:<18} {}",
            human_bytes(u(i, "bytes")),
            tag,
            kind_label(s(i, "kind")),
            s(i, "path")
        );
    }
    if items.len() > 30 {
        println!("  … and {} more (--json lists all)", items.len() - 30);
    }
    0
}

fn kind_label(k: &str) -> &str {
    match k {
        "cargo_target" => "Rust build cache",
        "node_modules" => "node_modules",
        "next_cache" => "Next.js cache",
        "xcode_derived_data" => "Xcode DerivedData",
        other => other,
    }
}

// --------------------------------------------------------------------- top

pub fn top(paths: &Paths, a: &TopArgs) -> i32 {
    let st = match state(paths, false, "sampling processes locally") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let mut procs: Vec<Value> = st["snapshot"]["top"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let key = |p: &Value| match a.sort {
        SortKey::Mem => f(p, "mem_bytes"),
        SortKey::Io => f(p, "read_bps") + f(p, "write_bps"),
        SortKey::Cpu => f(p, "cpu_pct"),
    };
    procs.sort_by(|x, y| key(y).total_cmp(&key(x)));
    procs.truncate(a.limit);
    if a.out.json {
        print_json(&procs);
        return 0;
    }
    let snap = &st["snapshot"];
    let load = |i: usize| snap["load"][i].as_f64().unwrap_or(0.0);
    println!(
        "cpu {:.0}%  load {:.1} {:.1} {:.1}  mem {} used of {}  swap {}  disk io r {}/s w {}/s",
        f(snap, "cpu_pct"),
        load(0),
        load(1),
        load(2),
        human_bytes(u(snap, "mem_total").saturating_sub(u(snap, "mem_available"))),
        human_bytes(u(snap, "mem_total")),
        human_bytes(u(snap, "swap_used")),
        human_bytes(u(snap, "read_bps")),
        human_bytes(u(snap, "write_bps")),
    );
    println!(
        "{:>7} {:>6} {:>9} {:>9} {:>4} {:<8} {:<16} GROUP",
        "PID", "CPU%", "MEM", "IO/s", "NI", "KIND", "NAME"
    );
    for p in &procs {
        println!(
            "{:>7} {:>6.1} {:>9} {:>9} {:>4} {:<8} {:<16} {}",
            u(p, "pid"),
            f(p, "cpu_pct"),
            human_bytes(u(p, "mem_bytes")),
            human_bytes(u(p, "read_bps") + u(p, "write_bps")),
            p["nice"].as_i64().unwrap_or(0),
            s(p, "kind"),
            truncate(s(p, "name"), 16),
            truncate(s(p, "group"), 50),
        );
    }
    0
}

// --------------------------------------------------------------- worktrees

pub fn worktrees(paths: &Paths, json_out: bool) -> i32 {
    let st = match state(paths, true, "scanning worktrees locally") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let wts = st["diskmap"]["worktrees"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if json_out {
        print_json(&wts);
        return 0;
    }
    if st["diskmap"].is_null() {
        eprintln!("mulchie: the engine has not finished its first scan yet; try again shortly");
    } else if wts.is_empty() {
        println!("no git worktrees found under the scan roots");
    }
    for w in &wts {
        println!(
            "{:>9} {:>4}d {:>3} dirty  {:<44} {}",
            human_bytes(u(w, "bytes")),
            u(w, "idle_days"),
            u(w, "dirty_files"),
            truncate(&format!("{} › {}", s(w, "repo_name"), s(w, "name")), 44),
            s(w, "verdict")
        );
    }
    0
}

// ------------------------------------------------------------------ advice

pub fn advice(paths: &Paths, json_out: bool) -> i32 {
    let (v, local) = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "GET", "/v1/advice", None) {
            Ok(v) => (v, false),
            Err(e) => return fail(conn::describe(&e)),
        },
        other => {
            other.note_local("judging from a local sample; there is no build queue");
            (
                serde_json::to_value(local_engine(paths, false).advice()).unwrap_or(Value::Null),
                true,
            )
        }
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    let reasons: Vec<&str> = v["reasons"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if v["ok_to_start_heavy_work"] == true {
        if local {
            println!("ok: the machine has room (no queue without an engine)");
        } else {
            println!(
                "ok: {} free build slot(s), use -j{}",
                u(&v, "free_slots"),
                u(&v, "suggested_jobs")
            );
        }
    } else {
        // Locally there are no slots, so "all 0 build slots are taken" says nothing.
        let reasons: Vec<&str> = reasons
            .into_iter()
            .filter(|r| !(local && r.contains("build slots")))
            .collect();
        if reasons.is_empty() {
            println!("ok: the machine has room (no queue without an engine)");
        } else {
            println!("wait: {}", reasons.join("; "));
        }
    }
    0
}

// ------------------------------------------------------------------- prune

pub fn prune(paths: &Paths, key: &str, json_out: bool) -> i32 {
    let r = match conn::connect(paths) {
        Conn::Engine(c) => {
            match conn::call(&c, "POST", "/v1/prune", Some(&json!({ "key": key }))) {
                Ok(v) => v,
                Err(e) => return fail(conn::describe(&e)),
            }
        }
        Conn::NotRunning => {
            Conn::NotRunning.note_local("running the prune command here");
            let engine = Engine::new(paths.clone(), Config::load_from(paths));
            serde_json::to_value(engine.prune_cache(key, "cli")).unwrap_or(Value::Null)
        }
        other => return fail(other.explain_required("prune")),
    };
    if json_out {
        print_json(&r);
    } else {
        let out = s(&r, "output").trim_end();
        if !out.is_empty() {
            println!("{out}");
        }
        if r["ok"] == true {
            println!("freed {}", human_bytes(u(&r, "freed")));
        } else {
            let reason = s(&r, "reason");
            eprintln!(
                "mulchie: prune {key} did not run{}",
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            );
        }
    }
    i32::from(r["ok"] != true)
}

// ------------------------------------------------------------------ signal

pub fn signal(paths: &Paths, pid: u32, sig: &str) -> i32 {
    let c = match conn::connect(paths) {
        Conn::Engine(c) => c,
        other => return fail(other.explain_required("`mulchie signal`")),
    };
    match conn::call(
        &c,
        "POST",
        "/v1/process",
        Some(&json!({ "pid": pid, "action": sig })),
    ) {
        Ok(_) => {
            println!("sent {sig} to {pid}");
            0
        }
        Err(e) => fail(conn::describe(&e)),
    }
}

// ------------------------------------------------------------------ config

pub fn config(paths: &Paths, json_out: bool) -> i32 {
    let loaded = Config::load_from(paths);
    let file = paths.config_file();
    let exists = file.exists();
    let error = loaded.error.as_ref().map(|e| e.to_string());
    if json_out {
        print_json(&json!({
            "path": file,
            "exists": exists,
            "error": error,
            "config": loaded.config,
        }));
    } else {
        println!("{}", file.display());
        if !exists {
            println!("(no file yet; these are the defaults)");
        }
        if let Some(e) = &error {
            eprintln!("mulchie: {e}");
            eprintln!(
                "mulchie: running with safe defaults (automatic cleanup off) until the file is fixed"
            );
        }
        print_json(&loaded.config);
    }
    i32::from(error.is_some())
}

// ------------------------------------------------------------------- serve

pub fn serve(paths: &Paths) -> i32 {
    let engine = Engine::new(paths.clone(), Config::load_from(paths));
    if let Some(e) = engine.config_error() {
        eprintln!("mulchie: {e}; running with safe defaults (automatic cleanup off)");
    }
    let port = mulchie_core::config::effective_port(&engine.config());
    let mut lock = match engine.acquire_lock() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mulchie: {e}; not starting a second one");
            return 1;
        }
    };
    if let Err(e) = mulchie_core::shutdown::install_termination_handler() {
        eprintln!("mulchie: cannot handle SIGTERM/SIGINT: {e}");
    }
    engine.enable_notifications();
    engine.start();
    match mulchie_core::api::serve(engine.clone(), port) {
        Ok(server) => {
            if let Err(e) = lock.set_port(Some(server.port())) {
                eprintln!("mulchie: could not announce the port in the engine lock: {e}");
            }
            println!("mulchie serving on http://127.0.0.1:{}", server.port());
            mulchie_core::shutdown::wait_for_termination();
            eprintln!("mulchie: stopping");
            server.stop();
            engine.shutdown(Duration::from_secs(10));
            drop(lock);
            0
        }
        Err(e) => {
            eprintln!("mulchie: could not start the API on 127.0.0.1:{port}: {e}");
            engine.shutdown(Duration::from_secs(10));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_chars() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("ééé", 2), "é…");
    }

    #[test]
    fn config_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(dir.path());
        assert_eq!(config(&p, true), 0);
        std::fs::create_dir_all(&p.config_dir).unwrap();
        std::fs::write(p.config_file(), b"{ not json").unwrap();
        assert_eq!(config(&p, true), 1);
    }
}
