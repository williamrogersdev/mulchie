//! `findings`, `inspect`, the grouped `top`, and `config` through the engine.
//!
//! Everything shown here comes from the core, which redacts secrets in
//! command lines at the source (the monitor, the watch and process detail),
//! so the local fallbacks, which run the same core code, are redacted too.

use crate::cli::{FindingsCommand, Severity};
use crate::commands::{f, fail, local_engine, print_json, s, truncate, u};
use crate::conn::{self, Conn};
use crate::diagnose::{finding_brief, FINDINGS_NOTE};
use mulchie_core::{human_bytes, Config, Engine, Paths};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------- findings

/// `GET /v1/findings`, or a one-off local watch run when no engine runs.
fn findings_value(
    paths: &Paths,
    severity: Option<Severity>,
    kind: Option<&str>,
    all: bool,
) -> Result<Value, String> {
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
            if let Some(sv) = severity {
                q.push(format!("severity={}", sv.as_str()));
            }
            if let Some(k) = kind {
                q.push(format!("kind={k}"));
            }
            if all {
                q.push("dismissed=1".into());
            }
            let path = if q.is_empty() {
                "/v1/findings".into()
            } else {
                format!("/v1/findings?{}", q.join("&"))
            };
            conn::call(&c, "GET", &path, None).map_err(|e| conn::describe(&e))
        }
        other => {
            other.note_local("looking at processes locally, once");
            let engine = local_engine(paths, false);
            engine.run_watch();
            let sev = severity
                .map(|sv| serde_json::from_value(json!(sv.as_str())))
                .transpose()
                .map_err(|e| e.to_string())?;
            let k = kind
                .map(|k| {
                    serde_json::from_value(json!(k))
                        .map_err(|_| format!("--kind {k:?} is not a finding kind"))
                })
                .transpose()?;
            let list = engine.findings(sev, k, all);
            Ok(json!({ "findings": list, "local": true }))
        }
    }
}

pub fn findings(
    paths: &Paths,
    severity: Option<Severity>,
    kind: Option<&str>,
    all: bool,
    json_out: bool,
) -> i32 {
    let v = match findings_value(paths, severity, kind, all) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    if json_out {
        let mut v = v;
        v["note"] = json!(FINDINGS_NOTE);
        print_json(&v);
        return 0;
    }
    print!("{}", render_findings(&v));
    0
}

pub fn render_findings(v: &Value) -> String {
    let list = v["findings"].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    if v["enabled"] == false {
        out.push_str("The watch is turned off (watch.enabled in `mulchie config`).\n");
    }
    if list.is_empty() {
        out.push_str("Nothing noticed.\n");
        return out;
    }
    out.push_str(&format!("{FINDINGS_NOTE}\n\n"));
    for f in &list {
        let b = finding_brief(f);
        let dismissed = if f["dismissed"] == true {
            " (dismissed)"
        } else {
            ""
        };
        out.push_str(&format!(
            "[{}] {}{dismissed}\n    {}\n",
            s(&b, "severity"),
            s(&b, "title"),
            s(&b, "why")
        ));
        let ev = &b["evidence"];
        if let Some(cmd) = ev["cmd"].as_array() {
            let words: Vec<&str> = cmd.iter().filter_map(Value::as_str).collect();
            out.push_str(&format!("    cmd: {}\n", truncate(&words.join(" "), 100)));
        }
        if let Some(cwd) = ev["cwd"].as_str() {
            out.push_str(&format!("    cwd: {cwd}\n"));
        }
        for c in b["cli"].as_array().into_iter().flatten() {
            out.push_str(&format!("    next: {}\n", c.as_str().unwrap_or("")));
        }
        out.push_str(&format!("    id: {}\n", s(&b, "id")));
    }
    out
}

pub fn findings_action(paths: &Paths, action: &FindingsCommand) -> i32 {
    let (id, json_out) = match action {
        FindingsCommand::Dismiss { id, out, .. } | FindingsCommand::Undismiss { id, out } => {
            (id.as_str(), out.json)
        }
    };
    let r = match conn::connect(paths) {
        Conn::Engine(c) => {
            let (path, body) = match action {
                FindingsCommand::Dismiss { days, .. } => {
                    let mut b = json!({ "id": id });
                    if let Some(d) = days {
                        b["days"] = json!(d);
                    }
                    ("/v1/findings/dismiss", b)
                }
                FindingsCommand::Undismiss { .. } => {
                    ("/v1/findings/undismiss", json!({ "id": id }))
                }
            };
            match conn::call(&c, "POST", path, Some(&body)) {
                Ok(v) => v,
                Err(e) => return fail(conn::describe(&e)),
            }
        }
        Conn::NotRunning => {
            Conn::NotRunning.note_local("saving it in the config file");
            let engine = Engine::new(paths.clone(), Config::load_from(paths));
            let r = match action {
                FindingsCommand::Dismiss { days, .. } => engine
                    .dismiss_finding(id, days.map(u64::from))
                    .map(|until| json!({ "ok": true, "id": id, "until_unix": until })),
                FindingsCommand::Undismiss { .. } => engine
                    .undismiss_finding(id)
                    .map(|was| json!({ "ok": true, "id": id, "was_dismissed": was })),
            };
            match r {
                Ok(v) => v,
                Err((code, m)) => return fail(format!("{m} [{code}]")),
            }
        }
        other => return fail(other.explain_required("`mulchie findings dismiss`")),
    };
    if json_out {
        print_json(&r);
    } else if let Some(until) = r["until_unix"].as_u64() {
        let days = until.saturating_sub(mulchie_core::monitor::now_unix()) / 86_400;
        println!("dismissed {id} for about {days} day(s)");
    } else if r["was_dismissed"] == false {
        println!("{id} was not dismissed");
    } else {
        println!("{id} is shown again");
    }
    0
}

// ----------------------------------------------------------------- inspect

pub fn inspect(paths: &Paths, pid: u32, json_out: bool) -> i32 {
    let v = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "GET", &format!("/v1/process/{pid}"), None) {
            Ok(v) => v,
            Err(e) if e.status == 404 => return fail(format!("no process {pid}")),
            Err(e) => return fail(conn::describe(&e)),
        },
        other => {
            other.note_local("reading the process table locally");
            match local_engine(paths, false).process_detail(pid) {
                Some(d) => serde_json::to_value(d).unwrap_or(Value::Null),
                None => return fail(format!("no process {pid}")),
            }
        }
    };
    if json_out {
        print_json(&v);
        return 0;
    }
    print!("{}", render_inspect(&v));
    0
}

fn words(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

pub fn render_inspect(v: &Value) -> String {
    let mut o = String::new();
    let line = |o: &mut String, k: &str, val: String| {
        if !val.is_empty() {
            o.push_str(&format!("{k:<11} {val}\n"));
        }
    };
    line(
        &mut o,
        "process",
        format!("{} ({})", s(v, "name"), u(v, "pid")),
    );
    line(&mut o, "command", words(&v["cmd"]));
    line(&mut o, "executable", s(v, "exe").into());
    line(&mut o, "directory", s(v, "cwd").into());
    line(&mut o, "workspace", s(v, "workspace").into());
    line(&mut o, "group", s(v, "group").into());
    line(&mut o, "kind", s(v, "kind").into());
    line(
        &mut o,
        "uses",
        format!(
            "{:.1}% cpu, {}",
            f(v, "cpu_pct"),
            human_bytes(u(v, "mem_bytes"))
        ),
    );
    line(&mut o, "age", age(u(v, "age_secs")));
    if v["stopped"] == true {
        line(&mut o, "state", "paused (stopped)".into());
    }
    if v["protected"] == true {
        line(
            &mut o,
            "protected",
            "yes: Mulchie will not act on it".into(),
        );
    }
    if v["own"] == false {
        line(&mut o, "owner", "another user".into());
    }
    let parents: Vec<String> = v["parents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| format!("{} ({})", s(p, "name"), u(p, "pid")))
        .collect();
    line(&mut o, "parents", parents.join(" ← "));
    let kids = v["children"].as_array().map_or(0, Vec::len);
    if kids > 0 {
        line(&mut o, "children", kids.to_string());
    }
    let listening: Vec<String> = v["listening"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| match l {
            Value::String(x) => x.clone(),
            other => other.to_string(),
        })
        .collect();
    line(&mut o, "listening", listening.join(", "));
    if !v["signature"].is_null() {
        line(&mut o, "signature", compact(&v["signature"]));
    }
    if v["quarantined"] == true {
        line(
            &mut o,
            "quarantine",
            "downloaded file, not yet opened by the user".into(),
        );
    }
    let fs = v["findings"].as_array().cloned().unwrap_or_default();
    if !fs.is_empty() {
        o.push_str("findings    (observations, not verdicts)\n");
        for f in &fs {
            o.push_str(&format!(
                "  [{}] {}\n      {}\n",
                s(f, "severity"),
                s(f, "title"),
                s(f, "why")
            ));
        }
    }
    o
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Object(m) => m
            .iter()
            .filter(|(_, x)| !x.is_null())
            .map(|(k, x)| match x {
                Value::String(t) => format!("{k} {t}"),
                other => format!("{k} {other}"),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn age(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{:.1} h", s as f64 / 3600.0),
        s => format!("{:.1} days", s as f64 / 86_400.0),
    }
}

// ------------------------------------------------------------- grouped top

/// The grouped view: the core's own sentences, then groups by CPU and by
/// memory.
pub fn render_groups(snap: &Value, limit: usize) -> String {
    let mut o = String::new();
    for k in ["cpu_summary", "mem_summary"] {
        let sm = &snap[k];
        let head = s(sm, "headline");
        if !head.is_empty() {
            o.push_str(head);
            let d = s(sm, "detail");
            if !d.is_empty() {
                o.push(' ');
                o.push_str(d);
            }
            o.push('\n');
        }
    }
    let c = &snap["consumers"];
    for (title, list) in [("CPU", "by_cpu"), ("Memory", "by_mem")] {
        let groups = c[list].as_array().cloned().unwrap_or_default();
        if groups.is_empty() {
            continue;
        }
        o.push_str(&format!(
            "\n{title}\n{:>7} {:>9} {:>5}  {:<10} {:<28} KEY\n",
            "CPU%", "MEM", "PROCS", "KIND", "NAME"
        ));
        for g in groups.iter().take(limit) {
            let flag = if g["protected"] == true { "*" } else { "" };
            o.push_str(&format!(
                "{:>7.1} {:>9} {:>5}  {:<10} {:<28} {}\n",
                f(g, "cpu_pct"),
                human_bytes(u(g, "mem_bytes")),
                u(g, "procs"),
                s(g, "kind"),
                truncate(&format!("{}{flag}", s(g, "name")), 28),
                s(g, "key"),
            ));
        }
    }
    if let Some(n) = c["memory_note"].as_str() {
        o.push_str(&format!("\n{n}\n"));
    }
    if c.is_null() {
        o.push_str("No grouped view yet (the engine has not sampled); try --processes.\n");
    } else {
        o.push_str("\n* protected: Mulchie will not act on it. Act on a group with `mulchie group <KEY> <action>`.\n");
    }
    o
}

pub fn groups_json(snap: &Value, limit: usize) -> Value {
    let c = &snap["consumers"];
    let take = |k: &str| -> Vec<Value> {
        c[k].as_array()
            .into_iter()
            .flatten()
            .take(limit)
            .cloned()
            .collect()
    };
    json!({
        "cpu_summary": snap["cpu_summary"],
        "mem_summary": snap["mem_summary"],
        "by_cpu": take("by_cpu"),
        "by_mem": take("by_mem"),
        "memory_note": c["memory_note"],
    })
}

// ------------------------------------------------------------------ config

pub fn config_show(paths: &Paths, json_out: bool) -> i32 {
    let (cfg, error, path, source) = match conn::connect(paths) {
        Conn::Engine(c) => match conn::call(&c, "GET", "/v1/config", None) {
            Ok(v) => (
                v["config"].clone(),
                v["config_error"].as_str().map(String::from),
                v["path"].as_str().unwrap_or("").to_string(),
                "engine",
            ),
            Err(e) => return fail(conn::describe(&e)),
        },
        other => {
            other.note_local("reading the config file");
            let loaded = Config::load_from(paths);
            (
                serde_json::to_value(&loaded.config).unwrap_or(Value::Null),
                loaded.error.map(|e| e.to_string()),
                paths.config_file().display().to_string(),
                "file",
            )
        }
    };
    let exists = std::path::Path::new(&path).exists();
    if json_out {
        print_json(&json!({
            "path": path,
            "exists": exists,
            "error": error,
            "source": source,
            "config": cfg,
        }));
    } else {
        println!("{path}");
        if !exists {
            println!("(no file yet; these are the defaults)");
        }
        if let Some(e) = &error {
            eprintln!("mulchie: {e}");
            eprintln!(
                "mulchie: running with safe defaults (automatic cleanup off) until the file is fixed"
            );
        }
        print_json(&cfg);
    }
    i32::from(error.is_some())
}

/// Set `key` (dotted) in a config value to `raw` (JSON, or a bare string).
pub fn set_key(cfg: &Value, key: &str, raw: &str) -> Result<Config, String> {
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let parts: Vec<&str> = key.split('.').collect();
    if key.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("{key:?} is not a setting name"));
    }
    let mut new = cfg.clone();
    let mut at = &mut new;
    for p in &parts[..parts.len() - 1] {
        at = match at.get_mut(*p) {
            Some(v @ Value::Object(_)) => v,
            _ => return Err(format!("unknown setting {key} (see `mulchie config`)")),
        };
    }
    let Some(obj) = at.as_object_mut() else {
        return Err(format!("unknown setting {key}"));
    };
    obj.insert(parts[parts.len() - 1].to_string(), value.clone());
    let parsed: Config =
        serde_json::from_value(new).map_err(|e| format!("{key} = {raw} does not fit: {e}"))?;
    // A misspelt key would be dropped by the parse: check it survived.
    let back = serde_json::to_value(&parsed).unwrap_or(Value::Null);
    let mut got = &back;
    for p in &parts {
        got = &got[*p];
    }
    if got.is_null() && !value.is_null() {
        return Err(format!("unknown setting {key} (see `mulchie config`)"));
    }
    parsed
        .validate()
        .map_err(|e| format!("{key} = {raw} is not allowed: {}", e.join("; ")))?;
    Ok(parsed)
}

pub fn config_set(paths: &Paths, key: &str, raw: &str, json_out: bool) -> i32 {
    let (saved, how) = match conn::connect(paths) {
        Conn::Engine(c) => {
            let cur = match conn::call(&c, "GET", "/v1/config", None) {
                Ok(v) => v,
                Err(e) => return fail(conn::describe(&e)),
            };
            if let Some(e) = cur["config_error"].as_str() {
                return fail(format!(
                    "the config file was not used ({e}); fix or remove {} first",
                    s(&cur, "path")
                ));
            }
            let cfg = match set_key(&cur["config"], key, raw) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            match conn::call(&c, "PUT", "/v1/config", Some(&json!({ "config": cfg }))) {
                Ok(v) => (v["config"].clone(), "through the engine; in effect now"),
                Err(e) => return fail(conn::describe(&e)),
            }
        }
        Conn::NotRunning => {
            let loaded = Config::load_from(paths);
            if let Some(e) = loaded.error {
                return fail(format!(
                    "{} was not used ({e}); fix or remove it first",
                    paths.config_file().display()
                ));
            }
            let cur = serde_json::to_value(&loaded.config).unwrap_or(Value::Null);
            let cfg = match set_key(&cur, key, raw) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            if let Err(e) = cfg.save_to(paths, mulchie_core::monitor::now_unix()) {
                return fail(e);
            }
            (
                serde_json::to_value(&cfg).unwrap_or(Value::Null),
                "in the file (no engine is running; it applies when one starts)",
            )
        }
        other => return fail(other.explain_required("`mulchie config set`")),
    };
    let mut got = &saved;
    for p in key.split('.') {
        got = &got[p];
    }
    if json_out {
        let mut m = Map::new();
        m.insert("ok".into(), json!(true));
        m.insert("key".into(), json!(key));
        m.insert("value".into(), got.clone());
        m.insert("saved".into(), json!(how));
        print_json(&Value::Object(m));
    } else {
        println!("{key} = {got}, saved {how}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_key_checks_names_types_and_ranges() {
        let cur = serde_json::to_value(Config::default()).unwrap();
        let c = set_key(&cur, "worktree_idle_days", "21").unwrap();
        assert_eq!(c.worktree_idle_days, 21);
        assert!(set_key(&cur, "worktree_idle_dayz", "21")
            .unwrap_err()
            .contains("unknown setting"));
        assert!(set_key(&cur, "worktree_idle_days", "\"x\"")
            .unwrap_err()
            .contains("does not fit"));
        assert!(set_key(&cur, "worktree_idle_days", "0")
            .unwrap_err()
            .contains("not allowed"));
        assert!(set_key(&cur, "nope.deeper", "1").is_err());
        assert!(set_key(&cur, "", "1").is_err());
        let c = set_key(&cur, "watch.enabled", "false").unwrap();
        assert!(!c.watch.enabled);
    }

    #[test]
    fn findings_render_as_observations() {
        let v = json!({ "findings": [{ "id": "runaway_cpu:42:7", "kind": "runaway_cpu", "severity": "medium",
            "title": "node has used a full core for 40 minutes", "why": "A stuck loop looks like this.",
            "evidence": { "pid": 42, "cmd": ["node", "--token", "[REDACTED]"] },
            "suggested": [{ "label": "Stop it", "request": { "action": "signal", "pid": 42, "signal": "term" } }] }] });
        let t = render_findings(&v);
        assert!(
            t.contains("not verdicts") && !t.to_lowercase().contains("malware detected"),
            "{t}"
        );
        assert!(t.contains("[medium] node has used"), "{t}");
        assert!(
            t.contains("mulchie signal 42 term") && t.contains("mulchie inspect 42"),
            "{t}"
        );
        assert!(
            t.contains("[REDACTED]") && t.contains("id: runaway_cpu:42:7"),
            "{t}"
        );
        assert_eq!(
            render_findings(&json!({ "findings": [] })),
            "Nothing noticed.\n"
        );
    }

    #[test]
    fn groups_lead_with_the_core_sentences() {
        let snap = json!({
            "cpu_summary": { "headline": "CPU is busy.", "detail": "Slack uses 2 cores." },
            "mem_summary": { "headline": "Memory is fine.", "detail": "" },
            "consumers": { "by_cpu": [{ "key": "app:/A.app", "kind": "app", "name": "Slack", "cpu_pct": 200.0, "mem_bytes": 1u64 << 30, "procs": 9, "protected": false }],
                "by_mem": [], "memory_note": "Memory is shared." }
        });
        let t = render_groups(&snap, 5);
        assert!(
            t.starts_with("CPU is busy. Slack uses 2 cores.\nMemory is fine.\n"),
            "{t}"
        );
        assert!(t.contains("app:/A.app") && t.contains("Slack"), "{t}");
        let j = groups_json(&snap, 5);
        assert_eq!(j["by_cpu"][0]["key"], "app:/A.app");
    }

    #[test]
    fn inspect_renders_redacted_command() {
        let v = json!({ "pid": 42, "name": "node", "cmd": ["node", "--password", "[REDACTED]"], "cwd": "/w",
            "protected": true, "findings": [{ "severity": "low", "title": "t", "why": "w" }] });
        let t = render_inspect(&v);
        assert!(
            t.contains("node --password [REDACTED]") && t.contains("protected"),
            "{t}"
        );
        assert!(t.contains("not verdicts"), "{t}");
    }
}
