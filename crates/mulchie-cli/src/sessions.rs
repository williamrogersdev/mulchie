//! `sessions`, `limits`, `updates` and `guard check`: what the engine knows
//! about agent sessions, their budgets, available updates, and what the guard
//! rules say about a command. All of it lives in the engine, so each needs
//! one running; none of it changes anything except `updates --check`, which
//! asks the engine to check now.

use crate::commands::{fail, print_json};
use crate::conn::{self, Conn};
use crate::hooks_install::sh_quote as q;
use mulchie_core::api::ApiError;
use mulchie_core::monitor::now_unix;
use mulchie_core::Paths;
use serde_json::{json, Value};

/// A call to the engine, or why there is none (said as a whole sentence).
fn engine(paths: &Paths, what: &str) -> Result<mulchie_core::api::Client, String> {
    match conn::connect(paths) {
        Conn::Engine(c) => Ok(c),
        other => Err(other.explain_required(what)),
    }
}

fn get(c: &mulchie_core::api::Client, path: &str) -> Result<Value, String> {
    conn::call(c, "GET", path, None).map_err(|e| conn::describe(&e))
}

/// Whether `id` has the shape of a session id: `s-<digits>-<digits>`.
pub fn valid_session_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("s-") else {
        return false;
    };
    let Some((a, b)) = rest.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|c| c.is_ascii_digit());
    digits(a) && digits(b)
}

pub fn check_session_id(id: &str) -> Result<(), String> {
    if valid_session_id(id) {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not a session id (they look like s-1700000000-4242; `mulchie sessions` lists them)"
        ))
    }
}

/// The events path for a session, with the query the engine checks.
pub fn events_path(id: &str, since: Option<u64>, limit: Option<u64>) -> String {
    let mut qs = Vec::new();
    if let Some(s) = since {
        qs.push(format!("since={s}"));
    }
    if let Some(l) = limit {
        qs.push(format!("limit={l}"));
    }
    let base = format!("/v1/sessions/{id}/events");
    if qs.is_empty() {
        base
    } else {
        format!("{base}?{}", qs.join("&"))
    }
}

// ------------------------------------------------------------------- words

pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn mb(v: &Value) -> String {
    match v.as_f64() {
        Some(m) if m >= 1024.0 => format!("{:.1} GB", m / 1024.0),
        Some(m) => format!("{m:.0} MB"),
        None => "-".into(),
    }
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// The usage of a session: `usage` in the full shape, the fields themselves
/// in the brief one in `GET /v1/state`.
fn usage(s: &Value) -> &Value {
    if s["usage"].is_object() {
        &s["usage"]
    } else {
        s
    }
}

fn cap_unit(cap: &str) -> &'static str {
    match cap {
        "cpu_cores" => " cores",
        "memory_mb" => " MB",
        "processes" => " processes",
        "write_mb_per_hour" => " MB/h",
        "runtime_hours" => " h",
        _ => "",
    }
}

fn num(v: &Value) -> String {
    match v.as_f64() {
        Some(x) if x.fract() == 0.0 => format!("{x:.0}"),
        Some(x) => format!("{x:.1}"),
        None => "?".into(),
    }
}

/// One breach in a sentence.
pub fn breach_line(b: &Value) -> String {
    let cap = st(b, "cap");
    let unit = cap_unit(cap);
    let mut s = format!(
        "{} {} over its {cap} cap: {}{unit} (max {}{unit}); action {}, {}",
        st(b, "agent"),
        st(b, "session_id"),
        num(&b["value"]),
        num(&b["max"]),
        st(b, "action"),
        st(b, "action_state").replace('_', " "),
    );
    if let Some(d) = b["action_detail"].as_str().filter(|d| !d.is_empty()) {
        s.push_str(&format!(" ({d})"));
    }
    if b["cleared_unix"].is_u64() {
        s.push_str(", cleared");
    }
    s
}

fn paused_line(p: &Value) -> String {
    format!(
        "pid {} of {} since {} ago",
        p["pid"],
        st(p, "session_id"),
        duration(now_unix().saturating_sub(p["since_unix"].as_u64().unwrap_or(0)))
    )
}

// ---------------------------------------------------------------- sessions

pub fn render_list(v: &Value) -> String {
    let mut s = String::new();
    if v["enabled"] == false {
        s.push_str("Session tracking is off (sessions.enabled in `mulchie config`).\n");
    }
    let list = v["sessions"].as_array().cloned().unwrap_or_default();
    let running: Vec<&Value> = list.iter().filter(|x| x["status"] == "running").collect();
    let ended: Vec<&Value> = list.iter().filter(|x| x["status"] != "running").collect();
    if list.is_empty() {
        s.push_str("No agent sessions.\n");
        return s;
    }
    s.push_str(&format!(
        "{} running, {} ended.\n",
        running.len(),
        ended.len()
    ));
    s.push_str(&format!(
        "  {:<22} {:<10} {:<8} {:>6} {:>9} {:>5} {:>8}  {}\n",
        "id", "agent", "status", "cpu", "memory", "procs", "runtime", "workspace"
    ));
    let line = |x: &Value| {
        let u = usage(x);
        format!(
            "  {:<22} {:<10} {:<8} {:>6} {:>9} {:>5} {:>8}  {}\n",
            st(x, "id"),
            crate::commands::truncate(st(x, "agent"), 10),
            st(x, "status"),
            format!("{:.1}", u["cpu_cores"].as_f64().unwrap_or(0.0)),
            mb(&u["memory_mb"]),
            u["processes"].as_u64().unwrap_or(0),
            duration(u["runtime_secs"].as_u64().unwrap_or(0)),
            x["workspace"].as_str().or(x["cwd"].as_str()).unwrap_or("-"),
        )
    };
    for x in &running {
        s.push_str(&line(x));
    }
    for x in ended.iter().take(10) {
        s.push_str(&line(x));
    }
    if ended.len() > 10 {
        s.push_str(&format!(
            "  … and {} more ended (--json lists them)\n",
            ended.len() - 10
        ));
    }
    if v["write_bytes_available"] == false {
        s.push_str("Disk writes per session cannot be read on this system.\n");
    }
    s.push_str("Details: `mulchie sessions <id>`.\n");
    s
}

pub fn render_detail(v: &Value) -> String {
    let x = &v["session"];
    let u = usage(x);
    let id = st(x, "id");
    let mut s = format!(
        "{} session {id} ({}, {}), pid {}\n",
        st(x, "agent"),
        st(x, "status"),
        st(x, "source"),
        x["root_pid"]
    );
    if let Some(w) = x["workspace"].as_str().or(x["cwd"].as_str()) {
        s.push_str(&format!("  workspace: {w}\n"));
    }
    if let Some(c) = x["command"].as_str().filter(|c| !c.is_empty()) {
        s.push_str(&format!("  command:   {c}\n"));
    }
    if let Some(p) = x["parent"].as_str() {
        s.push_str(&format!("  started by session {p}\n"));
    }
    s.push_str(&format!(
        "  using:     {:.1} cores, {}, {} processes, {} build slot(s), running {}\n",
        u["cpu_cores"].as_f64().unwrap_or(0.0),
        mb(&u["memory_mb"]),
        u["processes"].as_u64().unwrap_or(0),
        u["build_slots"].as_u64().unwrap_or(0),
        duration(u["runtime_secs"].as_u64().unwrap_or(0)),
    ));
    if let Some(w) = u["write_mb_per_hour"].as_f64() {
        s.push_str(&format!("  writing:   {w:.0} MB/h\n"));
    }
    let pk = &x["peak"];
    if pk.is_object() {
        s.push_str(&format!(
            "  peak:      {:.1} cores, {}, {} processes\n",
            pk["cpu_cores"].as_f64().unwrap_or(0.0),
            mb(&pk["memory_mb"]),
            pk["processes"].as_u64().unwrap_or(0)
        ));
    }
    let ports: Vec<String> = x["listening"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| p.to_string())
        .collect();
    if !ports.is_empty() {
        s.push_str(&format!("  listening: {}\n", ports.join(", ")));
    }
    let procs = v["processes"].as_array().cloned().unwrap_or_default();
    if !procs.is_empty() {
        s.push_str(&format!("Processes ({}):\n", procs.len()));
        for p in procs.iter().take(30) {
            s.push_str(&format!(
                "  {:>7} {:>5.0}% {:>9}  {}{}{}\n",
                p["pid"],
                p["cpu_pct"].as_f64().unwrap_or(0.0),
                mulchie_core::human_bytes(p["mem_bytes"].as_u64().unwrap_or(0)),
                crate::commands::truncate(
                    p["cmd"]
                        .as_str()
                        .filter(|c| !c.is_empty())
                        .unwrap_or(st(p, "name")),
                    70
                ),
                if p["root"] == true {
                    "  (the agent)"
                } else {
                    ""
                },
                if p["stopped"] == true {
                    "  (paused)"
                } else {
                    ""
                },
            ));
        }
        if procs.len() > 30 {
            s.push_str(&format!(
                "  … and {} more (--json lists them)\n",
                procs.len() - 30
            ));
        }
    }
    let br = v["breaches"].as_array().cloned().unwrap_or_default();
    if !br.is_empty() {
        s.push_str("Breaches:\n");
        for b in &br {
            s.push_str(&format!("  {}\n", breach_line(b)));
        }
    }
    let paused = v["paused_by_mulchie"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !paused.is_empty() {
        s.push_str("Paused by Mulchie:\n");
        for p in &paused {
            s.push_str(&format!("  {}\n", paused_line(p)));
        }
    }
    if x["status"] == "running" {
        s.push_str(&format!(
            "Stop it (shows the plan, then asks): mulchie sessions stop {}\n",
            q(id)
        ));
    }
    s.push_str(&format!("Activity: mulchie sessions {} --events\n", q(id)));
    s
}

pub fn render_events(v: &Value) -> String {
    let evs = v["events"].as_array().cloned().unwrap_or_default();
    if evs.is_empty() {
        return "No events.\n".into();
    }
    let mut s = String::new();
    for e in &evs {
        let mut rest: Vec<String> = Vec::new();
        for k in [
            "command", "tool", "rule", "decision", "cap", "action", "detail", "reason",
        ] {
            if let Some(x) = e[k].as_str().filter(|x| !x.is_empty()) {
                rest.push(format!("{k}={}", crate::commands::truncate(x, 80)));
            }
        }
        s.push_str(&format!(
            "  {:>5}  {}  {:<16} {}\n",
            e["seq"],
            e["t"]
                .as_u64()
                .map(|t| format!("{} ago", duration(now_unix().saturating_sub(t))))
                .unwrap_or_default(),
            st(e, "type"),
            rest.join(" ")
        ));
    }
    s.push_str(&format!(
        "More: --since {}\n",
        v["next_since"].as_u64().unwrap_or(0)
    ));
    s
}

pub fn sessions(
    paths: &Paths,
    id: Option<&str>,
    events: bool,
    since: Option<u64>,
    limit: Option<u64>,
    json_out: bool,
) -> i32 {
    if let Some(id) = id {
        if let Err(e) = check_session_id(id) {
            return fail(e);
        }
    }
    let c = match engine(paths, "`mulchie sessions`") {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let (v, text) = match (id, events) {
        (None, _) => match get(&c, "/v1/sessions") {
            Ok(v) => {
                let t = render_list(&v);
                (v, t)
            }
            Err(e) => return fail(e),
        },
        (Some(id), true) => match get(&c, &events_path(id, since, limit)) {
            Ok(v) => {
                let t = render_events(&v);
                (v, t)
            }
            Err(e) => return fail(e),
        },
        (Some(id), false) => match get(&c, &format!("/v1/sessions/{id}")) {
            Ok(v) => {
                let t = render_detail(&v);
                (v, t)
            }
            Err(e) => return fail(e),
        },
    };
    if json_out {
        print_json(&v);
    } else {
        print!("{text}");
    }
    0
}

// ------------------------------------------------------------------ limits

fn caps_line(b: &Value) -> String {
    let mut caps = Vec::new();
    for cap in [
        "cpu_cores",
        "memory_mb",
        "processes",
        "write_mb_per_hour",
        "runtime_hours",
    ] {
        let c = &b[cap];
        if c.is_object() {
            caps.push(format!(
                "{cap} {}{} for {}s then {}",
                num(&c["max"]),
                cap_unit(cap),
                c["sustain_secs"].as_u64().unwrap_or(60),
                c["action"].as_str().unwrap_or("notify")
            ));
        }
    }
    if caps.is_empty() {
        "no caps".into()
    } else {
        caps.join("; ")
    }
}

pub fn render_limits(v: &Value) -> String {
    let mut s = String::new();
    let cfg = &v["config"];
    let has_budget =
        cfg["default"].is_object() || cfg["workspaces"].as_array().is_some_and(|w| !w.is_empty());
    if !has_budget {
        s.push_str("No session budget is set: Mulchie only reports sessions that hog the machine (limits in `mulchie config`).\n");
    } else if v["report_only"] == true {
        s.push_str("Report only: every cap only notifies; nothing is paused or stopped.\n");
    } else {
        s.push_str(
            "Enforcing: caps act on sessions over budget (through the safety gate, journaled).\n",
        );
    }
    if v["enforcing"] == false {
        s.push_str("This engine does not hold the engine lock, so it reports and never acts.\n");
    }
    if cfg["default"].is_object() {
        s.push_str(&format!("Default budget: {}\n", caps_line(&cfg["default"])));
    }
    for w in cfg["workspaces"].as_array().into_iter().flatten() {
        s.push_str(&format!(
            "Budget for {}: {}\n",
            st(w, "path"),
            caps_line(&w["budget"])
        ));
    }
    if let (Some(p), Some(t)) = (
        cfg["hog_share_pct"].as_f64(),
        cfg["hog_sustain_secs"].as_u64(),
    ) {
        s.push_str(&format!(
            "A session using {p:.0}% of CPU or memory for {} while the machine is strained is a hog.\n",
            duration(t)
        ));
    }
    let br = v["breaches"].as_array().cloned().unwrap_or_default();
    if br.is_empty() {
        s.push_str("No session is over its budget.\n");
    } else {
        s.push_str(&format!("Breaches ({}):\n", br.len()));
        for b in &br {
            s.push_str(&format!("  {}\n", breach_line(b)));
        }
    }
    for h in v["hogs"].as_array().into_iter().flatten() {
        s.push_str(&format!(
            "Hog: {} {} uses {:.0}% of the machine's {} (for {})\n",
            st(h, "agent"),
            st(h, "session_id"),
            h["share_pct"].as_f64().unwrap_or(0.0),
            st(h, "resource"),
            duration(now_unix().saturating_sub(h["since_unix"].as_u64().unwrap_or(0)))
        ));
    }
    for (k, label) in [
        ("paused_by_mulchie", "Paused by Mulchie"),
        ("paused_left", "Paused by an earlier engine and not resumed (resume with `mulchie group session:<id> resume`)"),
    ] {
        let list = v[k].as_array().cloned().unwrap_or_default();
        if !list.is_empty() {
            s.push_str(&format!("{label}:\n"));
            for p in &list {
                s.push_str(&format!("  {}\n", paused_line(p)));
            }
        }
    }
    s
}

pub fn limits(paths: &Paths, json_out: bool) -> i32 {
    let v = match engine(paths, "`mulchie limits`").and_then(|c| get(&c, "/v1/limits/status")) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if json_out {
        print_json(&v);
    } else {
        print!("{}", render_limits(&v));
    }
    0
}

// ----------------------------------------------------------------- updates

/// Open update findings (category `update`) from a findings reply.
pub fn update_findings(findings: &Value) -> Vec<Value> {
    findings["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["category"] == "update")
        .cloned()
        .collect()
}

pub fn render_updates(status: &Value, found: &[Value]) -> String {
    let now = now_unix();
    let mut s = String::new();
    if status["enabled"] == false {
        s.push_str(
            "Update checks are off (updates.check in `mulchie config`); nothing is checked.\n",
        );
    } else {
        match status["checked_unix"].as_u64() {
            Some(t) => s.push_str(&format!(
                "Last checked {} ago",
                duration(now.saturating_sub(t))
            )),
            None => s.push_str("Not checked yet"),
        }
        if status["checking"] == true {
            s.push_str("; checking now");
        } else if let Some(n) = status["next_check_unix"].as_u64() {
            s.push_str(&format!(
                "; next check in {}",
                duration(n.saturating_sub(now))
            ));
        }
        s.push_str(".\n");
    }
    let failures = status["failures"].as_u64().unwrap_or(0);
    for e in status["errors"].as_array().into_iter().flatten() {
        s.push_str(&format!("  ! {}\n", e.as_str().unwrap_or("")));
    }
    if failures > 1 {
        s.push_str(&format!("  ({failures} checks in a row had errors)\n"));
    }
    if found.is_empty() {
        let n = status["available"].as_u64().unwrap_or(0);
        if n == 0 {
            s.push_str("Nothing to update.\n");
        } else {
            s.push_str(&format!(
                "{n} update(s) available (`mulchie findings` lists them).\n"
            ));
        }
    } else {
        s.push_str(&format!("Available ({}):\n", found.len()));
        for f in found {
            s.push_str(&format!("  [{}] {}\n", st(f, "severity"), st(f, "title")));
            if let Some(c) = f["evidence"]["detail"].as_str().filter(|c| !c.is_empty()) {
                s.push_str(&format!("       {c}\n"));
            }
        }
    }
    s
}

pub fn updates(paths: &Paths, check: bool, json_out: bool) -> i32 {
    let c = match engine(paths, "`mulchie updates`") {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mut queued = false;
    if check {
        match conn::call(&c, "POST", "/v1/updates/check", None) {
            Ok(_) => queued = true,
            Err(e) => return fail(explain_check(&e)),
        }
    }
    let status = match get(&c, "/v1/updates") {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let found = get(&c, "/v1/findings")
        .map(|f| update_findings(&f))
        .unwrap_or_default();
    if json_out {
        print_json(&json!({ "status": status, "queued": queued, "updates": found }));
    } else {
        if queued {
            println!(
                "A check was started; run `mulchie updates` again in a minute for its result."
            );
        }
        print!("{}", render_updates(&status, &found));
    }
    0
}

pub fn explain_check(e: &ApiError) -> String {
    match e.code.as_str() {
        "updates_off" => "update checks are off, so nothing was checked [updates_off]; turn them on with `mulchie config set updates.check on`".into(),
        "read_only_token" => {
            "only the full API token may start an update check [read_only_token]".into()
        }
        _ => conn::describe(e),
    }
}

// ------------------------------------------------------------------- guard

/// The command line from `guard check`'s words: one word is taken as the
/// whole command, several are joined shell-quoted.
pub fn command_line(words: &[String]) -> String {
    if words.len() == 1 {
        words[0].clone()
    } else {
        words.iter().map(|w| q(w)).collect::<Vec<_>>().join(" ")
    }
}

pub fn render_guard(v: &Value) -> String {
    let mut s = match st(v, "decision") {
        "block" => format!("Block: {}", st(v, "reason")),
        "warn" => format!("Warn: {}", st(v, "reason")),
        _ => "Allow: no guard rule matches.".into(),
    };
    if let Some(r) = v["rule"].as_str() {
        s.push_str(&format!(" [{r}]"));
    }
    s.push('\n');
    let m = v["matches"].as_array().cloned().unwrap_or_default();
    if m.len() > 1 {
        for x in &m {
            s.push_str(&format!(
                "  {} {}: {}\n",
                st(x, "mode"),
                st(x, "rule"),
                st(x, "reason")
            ));
        }
    }
    if let Some(a) = v["advice"].as_str() {
        s.push_str(&format!("{a}\n"));
    }
    s
}

/// Exit status of `guard check`: 0 allow, 3 block, 4 warn.
pub fn guard_status(v: &Value) -> i32 {
    match st(v, "decision") {
        "block" => 3,
        "warn" => 4,
        _ => 0,
    }
}

pub fn guard_check(
    paths: &Paths,
    words: &[String],
    cwd: Option<&std::path::Path>,
    json_out: bool,
) -> i32 {
    let cwd = cwd
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| "/".into());
    let body = json!({ "command": command_line(words), "cwd": cwd });
    let c = match engine(paths, "`mulchie guard check`") {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let v = match conn::call(&c, "POST", "/v1/guard/check", Some(&body)) {
        Ok(v) => v,
        Err(e) => return fail(conn::describe(&e)),
    };
    if json_out {
        print_json(&v);
    } else {
        print!("{}", render_guard(&v));
    }
    guard_status(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shapes from the core's `sessions_hooks_and_updates_over_the_api`.
    fn list() -> Value {
        json!({"enabled":true,"sessions":[{"agent":"claude","client":null,"client_session_id":null,"command":"claude","cwd":"/code/a","ended_unix":null,"id":"s-500-100","listening":[],"parent":null,"peak":{"cpu_cores":0.1,"memory_mb":2.0,"processes":2,"write_mb_per_hour":null},"root_pid":100,"root_start_time":500,"source":"detected","started_unix":1_794_277_830u64,"status":"running","usage":{"build_slots":0,"bytes_written":null,"cpu_cores":0.1,"memory_mb":2.0,"processes":2,"runtime_secs":3700,"write_mb_per_hour":null},"workspace":"/code/a"}],"write_bytes_available":false})
    }

    #[test]
    fn session_ids_are_checked_before_they_go_in_a_path() {
        assert!(valid_session_id("s-500-100"));
        for bad in [
            "s-500",
            "s-a-1",
            "500-100",
            "s-1-2/../x",
            "s-1-2?x=1",
            "s--1",
            "",
        ] {
            assert!(!valid_session_id(bad), "{bad}");
        }
        assert_eq!(
            events_path("s-1-2", Some(5), Some(10)),
            "/v1/sessions/s-1-2/events?since=5&limit=10"
        );
        assert_eq!(
            events_path("s-1-2", None, None),
            "/v1/sessions/s-1-2/events"
        );
    }

    #[test]
    fn the_list_and_detail_read_the_real_shapes() {
        let t = render_list(&list());
        assert!(t.contains("1 running, 0 ended"), "{t}");
        assert!(
            t.contains("s-500-100") && t.contains("claude") && t.contains("1h01m"),
            "{t}"
        );
        assert!(t.contains("/code/a") && t.contains("2 MB"), "{t}");
        assert!(t.contains("cannot be read"), "{t}");
        let detail = json!({"breaches":[{"id":"b1","session_id":"s-500-100","agent":"claude","cap":"memory_mb","max":1000.0,"value":1500.0,"action":"pause","action_state":"done","breached_unix":1}],
            "paused_by_mulchie":[{"pid":101,"start_time":600,"session_id":"s-500-100","breach_id":"b1","since_unix":1}],
            "processes":[{"cmd":"claude","cpu_pct":5.0,"mem_bytes":1_048_576,"name":"claude","pid":100,"ppid":1,"root":true,"start_time":500,"stopped":false},{"cmd":"node","cpu_pct":5.0,"mem_bytes":1_048_576,"name":"node","pid":101,"ppid":100,"root":false,"start_time":600,"stopped":true}],
            "session": list()["sessions"][0]});
        let t = render_detail(&detail);
        assert!(
            t.contains("claude session s-500-100 (running, detected), pid 100"),
            "{t}"
        );
        assert!(t.contains("(the agent)") && t.contains("(paused)"), "{t}");
        assert!(
            t.contains("over its memory_mb cap: 1500 MB (max 1000 MB); action pause, done"),
            "{t}"
        );
        assert!(
            t.contains("Paused by Mulchie:\n  pid 101 of s-500-100"),
            "{t}"
        );
        assert!(t.contains("mulchie sessions stop s-500-100"), "{t}");
        let ev = json!({"events":[{"agent":"claude","command":"claude","seq":1,"t":1,"type":"session_start"}],"next_since":1,"session_id":"s-500-100"});
        let t = render_events(&ev);
        assert!(
            t.contains("session_start") && t.contains("--since 1"),
            "{t}"
        );
    }

    #[test]
    fn limits_say_whether_they_act() {
        let none = json!({"breaches":[],"config":{"default":null,"hog_share_pct":50.0,"hog_sustain_secs":120,"workspaces":[]},"enforcing":true,"hogs":[],"paused_by_mulchie":[],"paused_left":[],"report_only":true});
        let t = render_limits(&none);
        assert!(t.contains("No session budget is set"), "{t}");
        assert!(t.contains("No session is over its budget"), "{t}");
        let mut on = none.clone();
        on["report_only"] = json!(false);
        on["enforcing"] = json!(false);
        on["config"]["default"] =
            json!({ "memory_mb": { "max": 4000.0, "sustain_secs": 60, "action": "pause" } });
        on["hogs"] = json!([{ "session_id": "s-1-2", "agent": "codex", "resource": "cpu", "share_pct": 70.0, "since_unix": 1 }]);
        on["paused_left"] = json!([{ "pid": 9, "session_id": "s-1-2", "since_unix": 1 }]);
        let t = render_limits(&on);
        assert!(
            t.contains("Enforcing") && t.contains("reports and never acts"),
            "{t}"
        );
        assert!(t.contains("memory_mb 4000 MB for 60s then pause"), "{t}");
        assert!(t.contains("Hog: codex s-1-2 uses 70%"), "{t}");
        assert!(t.contains("not resumed") && t.contains("pid 9"), "{t}");
    }

    #[test]
    fn updates_and_guard_in_words() {
        let status = json!({"available":0,"checked_unix":null,"checking":false,"enabled":true,"errors":[],"failures":0,"next_check_unix": now_unix() + 300});
        let t = render_updates(&status, &[]);
        assert!(
            t.contains("Not checked yet; next check in") && t.contains("Nothing to update"),
            "{t}"
        );
        let f = json!({ "findings": [
            { "id": "a", "category": "update", "severity": "low", "title": "claude 1.0 is out (1.2 installed? no: 0.9)" },
            { "id": "b", "category": "hygiene", "severity": "low", "title": "other" } ] });
        let found = update_findings(&f);
        assert_eq!(found.len(), 1);
        assert!(render_updates(&status, &found).contains("Available (1)"));
        let off = json!({ "enabled": false });
        assert!(render_updates(&off, &[]).contains("Update checks are off"));
        let e = ApiError::new(409, "updates_off", "update checks are off");
        assert!(explain_check(&e).contains("mulchie config set updates.check on"));

        let v = json!({"decision":"warn","matches":[{"mode":"warn","reason":"recursively deletes the root directory","rule":"recursive_delete"},{"mode":"warn","reason":"runs a command with elevated privileges (sudo/doas)","rule":"sudo"}],"reason":"recursively deletes the root directory","rule":"recursive_delete"});
        let t = render_guard(&v);
        assert!(
            t.starts_with("Warn: recursively deletes the root directory [recursive_delete]"),
            "{t}"
        );
        assert!(t.contains("warn sudo:"), "{t}");
        assert_eq!(guard_status(&v), 4);
        assert_eq!(guard_status(&json!({ "decision": "block" })), 3);
        assert_eq!(guard_status(&json!({ "decision": "allow" })), 0);
        assert_eq!(command_line(&["rm -rf /".into()]), "rm -rf /");
        assert_eq!(
            command_line(&["rm".into(), "-rf".into(), "it's".into()]),
            "rm -rf 'it'\\''s'"
        );
    }
}
