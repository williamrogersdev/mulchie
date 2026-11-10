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

/// Said when findings come from one local look instead of the engine.
pub const LOCAL_NOTE: &str = "This was one look, taken now: patterns that need time (runaway CPU, growing memory, stale or stopped processes) are seen only by a running engine.";

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
            let enabled = engine.config().watch.enabled;
            Ok(json!({
                "findings": list,
                "enabled": enabled,
                "local": true,
                "local_note": LOCAL_NOTE,
            }))
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
    if let Some(n) = v["local_note"].as_str() {
        out.push_str(&format!("{n}\n"));
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

/// Refuse an id that is not among `known` findings (a `/v1/findings` reply),
/// naming the closest ones.
pub fn check_finding_id(id: &str, known: &Value) -> Result<(), String> {
    let ids: Vec<&str> = known["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["id"].as_str())
        .collect();
    if ids.contains(&id) {
        return Ok(());
    }
    let common = |a: &str| {
        a.chars()
            .zip(id.chars())
            .take_while(|(x, y)| x == y)
            .count()
    };
    let mut close: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|k| common(k) >= 3 || k.contains(id) || id.contains(*k))
        .collect();
    close.sort_by_key(|k| std::cmp::Reverse(common(k)));
    close.truncate(5);
    Err(if close.is_empty() {
        format!(
            "{id:?} is not a current or dismissed finding ({} known); check the id, or force it",
            ids.len()
        )
    } else {
        format!(
            "{id:?} is not a current or dismissed finding; did you mean {}? Or force it",
            close
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

pub fn findings_action(paths: &Paths, action: &FindingsCommand) -> i32 {
    let (id, json_out) = match action {
        FindingsCommand::Dismiss { id, out, .. } | FindingsCommand::Undismiss { id, out } => {
            (id.as_str(), out.json)
        }
    };
    let force = matches!(action, FindingsCommand::Dismiss { force: true, .. });
    let check = matches!(action, FindingsCommand::Dismiss { .. }) && !force;
    // Twice at most: an engine that starts while we look is used instead.
    let mut r = None;
    for _ in 0..2 {
        match conn::connect(paths) {
            Conn::Engine(c) => {
                if check {
                    let known = match conn::call(&c, "GET", "/v1/findings?dismissed=1", None) {
                        Ok(v) => v,
                        Err(e) => return fail(conn::describe(&e)),
                    };
                    if let Err(e) = check_finding_id(id, &known) {
                        return fail(e.replace("force it", "pass --force"));
                    }
                }
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
                    Ok(v) => r = Some(v),
                    Err(e) => return fail(conn::describe(&e)),
                }
                break;
            }
            Conn::NotRunning => {
                let engine = Engine::new(paths.clone(), Config::load_from(paths));
                // Hold the engine lock while rewriting the config, so an
                // engine cannot start and write it at the same time.
                let Ok(_lock) = engine.acquire_lock() else {
                    continue;
                };
                Conn::NotRunning.note_local("saving it in the config file");
                if check {
                    engine.set_snapshot(crate::commands::sample());
                    engine.run_watch();
                    let known = json!({ "findings": engine.findings(None, None, true) });
                    if let Err(e) = check_finding_id(id, &known) {
                        return fail(e.replace("force it", "pass --force"));
                    }
                }
                let res = match action {
                    FindingsCommand::Dismiss { days, .. } => engine
                        .dismiss_finding(id, days.map(u64::from))
                        .map(|until| json!({ "ok": true, "id": id, "until_unix": until })),
                    FindingsCommand::Undismiss { .. } => engine
                        .undismiss_finding(id)
                        .map(|was| json!({ "ok": true, "id": id, "was_dismissed": was })),
                };
                match res {
                    Ok(v) => r = Some(v),
                    Err((code, m)) => return fail(format!("{m} [{code}]")),
                }
                break;
            }
            other => return fail(other.explain_required("`mulchie findings dismiss`")),
        }
    }
    let Some(r) = r else {
        return fail("an engine is starting; try again in a moment");
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
/// Returns the edited JSON itself, so keys this build does not know (from a
/// newer engine) are kept; this build's typed parse is only a check.
pub fn set_key(cfg: &Value, key: &str, raw: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let parts: Vec<&str> = key.split('.').collect();
    if key.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("{key:?} is not a setting name"));
    }
    let unknown = || format!("unknown setting {key} (see `mulchie config`)");
    let known_here = cfg_has_path(cfg, &parts);
    let mut new = cfg.clone();
    let mut at = &mut new;
    for p in &parts[..parts.len() - 1] {
        at = match at.get_mut(*p) {
            Some(v @ Value::Object(_)) => v,
            _ => return Err(unknown()),
        };
    }
    let Some(obj) = at.as_object_mut() else {
        return Err(unknown());
    };
    obj.insert(parts[parts.len() - 1].to_string(), value.clone());
    let parsed: Config = serde_json::from_value(new.clone())
        .map_err(|e| format!("{key} = {raw} does not fit: {e}"))?;
    // A key neither the engine's config nor this build knows is a typo.
    let back = serde_json::to_value(&parsed).unwrap_or(Value::Null);
    if !known_here && !cfg_has_path(&back, &parts) {
        return Err(unknown());
    }
    parsed
        .validate()
        .map_err(|e| format!("{key} = {raw} is not allowed: {}", e.join("; ")))?;
    Ok(new)
}

/// The value of `key` in the config the engine saved. An older engine drops
/// keys it does not know, so a missing key means the setting did not take.
pub fn saved_value<'a>(saved: &'a Value, key: &str) -> Result<&'a Value, String> {
    let parts: Vec<&str> = key.split('.').collect();
    if !cfg_has_path(saved, &parts) {
        return Err(format!(
            "the engine does not know `{key}`, so it was not saved; update the engine (and restart it) to use this setting"
        ));
    }
    let mut got = saved;
    for p in parts {
        got = &got[p];
    }
    Ok(got)
}

/// How often, and how far apart, `config set` tries for the engine lock.
const LOCK_TRIES: usize = 15;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// The RFC 7396 merge patch that sets `key` (dotted) to its value in
/// `edited`: `{"a": {"b": value}}`.
pub fn merge_patch(key: &str, edited: &Value) -> Value {
    let parts: Vec<&str> = key.split('.').collect();
    let mut at = edited;
    for p in &parts {
        at = &at[*p];
    }
    let mut patch = at.clone();
    for p in parts.iter().rev() {
        let mut m = Map::new();
        m.insert((*p).to_string(), patch);
        patch = Value::Object(m);
    }
    patch
}

/// Whether every part of `parts` is a key, object by object, in `v`.
fn cfg_has_path(v: &Value, parts: &[&str]) -> bool {
    let mut at = v;
    for p in parts {
        match at.as_object().and_then(|o| o.get(*p)) {
            Some(x) => at = x,
            None => return false,
        }
    }
    true
}

pub fn config_set(paths: &Paths, key: &str, raw: &str, json_out: bool) -> i32 {
    config_set_with(paths, key, raw, json_out, &conn::connect)
}

/// [`config_set`] with the connection made by `connect`, so a test can
/// stage an engine starting between the look and the write.
fn config_set_with(
    paths: &Paths,
    key: &str,
    raw: &str,
    json_out: bool,
    connect: &dyn Fn(&Paths) -> Conn,
) -> i32 {
    let mut done = None;
    let mut held_by = None;
    // An engine that starts while we look is used instead. Another process
    // holding the lock without a listener (an engine starting, or another
    // `mulchie config set`) is waited for, briefly.
    for attempt in 0..LOCK_TRIES {
        if attempt > 0 {
            std::thread::sleep(LOCK_WAIT);
        }
        match connect(paths) {
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
                // The edit is checked against the engine's config here; what
                // is sent is only the change, as a JSON merge patch.
                let edited = match set_key(&cur["config"], key, raw) {
                    Ok(c) => c,
                    Err(e) => return fail(e),
                };
                let patch = merge_patch(key, &edited);
                let r = match conn::call(&c, "PATCH", "/v1/config", Some(&patch)) {
                    // An engine from before PATCH: send the whole edited config.
                    Err(e) if e.code == "method_not_allowed" || e.code == "not_found" => {
                        conn::call(&c, "PUT", "/v1/config", Some(&json!({ "config": edited })))
                    }
                    r => r,
                };
                match r {
                    Ok(v) => {
                        done = Some((v["config"].clone(), "through the engine; in effect now"))
                    }
                    Err(e) => return fail(conn::describe(&e)),
                }
                break;
            }
            Conn::NotRunning => {
                // Take the engine lock first, then read the file: an edit
                // made by another process before we held the lock is kept.
                let engine = Engine::new(paths.clone(), Config::load_from(paths));
                let Ok(_lock) = engine.acquire_lock() else {
                    held_by = Some("another process".to_string());
                    continue;
                };
                let loaded = Config::load_from(paths);
                if let Some(e) = loaded.error {
                    return fail(format!(
                        "{} was not used ({e}); fix or remove it first",
                        paths.config_file().display()
                    ));
                }
                let cur = serde_json::to_value(&loaded.config).unwrap_or(Value::Null);
                let edited = match set_key(&cur, key, raw) {
                    Ok(c) => c,
                    Err(e) => return fail(e),
                };
                // The core's checked write: it re-reads the file under its
                // own lock and applies only this change.
                match engine.patch_config(&merge_patch(key, &edited)) {
                    Ok(cfg) => {
                        done = Some((
                            serde_json::to_value(&cfg).unwrap_or(Value::Null),
                            "in the file (no engine is running; it applies when one starts)",
                        ))
                    }
                    Err(e) => return fail(e),
                }
                break;
            }
            Conn::Unreachable { pid, reason } if reason.contains("no API listener") => {
                held_by = Some(format!("pid {pid}"));
                continue;
            }
            other => return fail(other.explain_required("`mulchie config set`")),
        }
    }
    let Some((saved, how)) = done else {
        return fail(format!(
            "{} holds the engine lock (an engine starting, or another `mulchie config set`), so nothing was changed; try again in a moment",
            held_by.unwrap_or_else(|| "another process".into())
        ));
    };
    let got = match saved_value(&saved, key) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
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
        assert_eq!(c["worktree_idle_days"], 21);
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
        assert_eq!(c["watch"]["enabled"], false);
    }

    /// The race: no engine when we look, one holding the lock by the time
    /// we would write. The edit must go through that engine, not the file.
    #[test]
    fn a_config_edit_goes_through_an_engine_that_started_after_the_look() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        let engine = Engine::new(paths.clone(), crate::commands::safe_test_config(&paths));
        let mut lk = engine.acquire_lock().unwrap();
        let server = mulchie_core::api::serve(engine.clone(), 0).unwrap();
        lk.set_port(Some(server.port())).unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        let looks = std::cell::Cell::new(0);
        // The first look misses the engine, as if it started just after.
        let connect = |p: &Paths| {
            looks.set(looks.get() + 1);
            if looks.get() == 1 {
                Conn::NotRunning
            } else {
                conn::connect(p)
            }
        };
        assert_eq!(
            config_set_with(&paths, "worktree_idle_days", "13", false, &connect),
            0
        );
        assert_eq!(
            looks.get(),
            2,
            "it looked again after failing to take the lock"
        );
        assert_eq!(
            engine.config().worktree_idle_days,
            13,
            "the running engine has the edit"
        );
        // The engine saved it; a direct write would not have reached it.
        assert_ne!(std::fs::read(paths.config_file()).unwrap(), before);
        server.stop();
    }

    #[test]
    fn a_key_the_engine_dropped_is_not_reported_as_saved() {
        let saved = json!({ "worktree_idle_days": 9, "guard": { "rules": {} } });
        assert_eq!(
            saved_value(&saved, "worktree_idle_days").unwrap(),
            &json!(9)
        );
        let e = saved_value(&saved, "guard.rules.sudo").unwrap_err();
        assert!(e.contains("does not know `guard.rules.sudo`"), "{e}");
        // A key set to null is still a key the engine kept.
        let saved = json!({ "limits": { "default": null } });
        assert!(saved_value(&saved, "limits.default").unwrap().is_null());
    }

    #[test]
    fn config_set_sends_only_the_change() {
        let cur = serde_json::to_value(Config::default()).unwrap();
        let edited = set_key(&cur, "guard.rules.sudo", "block").unwrap();
        assert_eq!(
            merge_patch("guard.rules.sudo", &edited),
            json!({ "guard": { "rules": { "sudo": "block" } } })
        );
        let edited = set_key(&cur, "scan_roots", r#"["/a"]"#).unwrap();
        assert_eq!(
            merge_patch("scan_roots", &edited),
            json!({ "scan_roots": ["/a"] })
        );
    }

    #[test]
    fn set_key_keeps_keys_from_a_newer_engine() {
        let mut cur = serde_json::to_value(Config::default()).unwrap();
        cur["from_a_newer_engine"] = json!({ "knob": 3 });
        // A key this build does not know, but the engine's config has.
        let c = set_key(&cur, "from_a_newer_engine.knob", "4").unwrap();
        assert_eq!(c["from_a_newer_engine"]["knob"], 4);
        // Setting a known key keeps the unknown one in what is sent.
        let c = set_key(&cur, "worktree_idle_days", "9").unwrap();
        assert_eq!(c["from_a_newer_engine"]["knob"], 3);
        assert_eq!(c["worktree_idle_days"], 9);
    }

    #[test]
    fn every_key_in_the_config_help_is_accepted() {
        use clap::CommandFactory;
        let mut cmd = crate::cli::Cli::command();
        let config = cmd.find_subcommand_mut("config").expect("config command");
        let mut help = String::new();
        for sub in config.get_subcommands_mut() {
            help.push_str(&sub.render_long_help().to_string());
        }
        help.push_str(&config.render_long_help().to_string());
        let cur = serde_json::to_value(Config::default()).unwrap();
        let mut seen = 0;
        for line in help.lines() {
            let Some(at) = line.find("mulchie config set ") else {
                continue;
            };
            let words = crate::hooks_install::sh_words(&line[at..]);
            if words.len() != 5 {
                continue;
            }
            set_key(&cur, &words[3], &words[4])
                .unwrap_or_else(|e| panic!("help example {:?} fails: {e}", &line[at..]));
            seen += 1;
        }
        assert!(
            seen >= 3,
            "expected at least three examples in the help, found {seen}:\n{help}"
        );
    }

    #[test]
    fn local_findings_say_when_the_watch_is_off_and_what_one_look_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        let mut cfg = Config::default();
        cfg.watch.enabled = false;
        cfg.save_to(&paths, 1).unwrap();
        let v = findings_value(&paths, None, None, false).unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["local"], true);
        let t = render_findings(&v);
        assert!(t.contains("The watch is turned off"), "{t}");
        assert!(t.contains("only by a running engine"), "{t}");
    }

    #[test]
    fn unknown_finding_ids_are_refused_with_close_ones() {
        let known = json!({ "findings": [
            { "id": "runaway_cpu:42:7" }, { "id": "runaway_cpu:43:1" }, { "id": "stale_worktree:/a" } ] });
        assert!(check_finding_id("runaway_cpu:42:7", &known).is_ok());
        let e = check_finding_id("runaway_cpu:42", &known).unwrap_err();
        assert!(e.contains("runaway_cpu:42:7"), "{e}");
        assert!(check_finding_id("zzz", &known).is_err());
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
