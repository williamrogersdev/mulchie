mod mcp;

use mulchie_core::api::Client;
use mulchie_core::{human_bytes, Config, Engine, Monitor};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "mulchie — keeps machines running many AI agents healthy

USAGE:
  mulchie status [--json]          disk, memory, CPU and what each agent workspace is using
  mulchie scan [--json]            find reclaimable build caches
  mulchie clean --stale [--dry-run] clear caches unused for the stale window with no agent running
  mulchie clean <path>...          clear specific caches (only recognised build caches)
  mulchie top [--sort cpu|mem|io] [-n N] [--json]
                                   processes by CPU, memory or disk I/O with workspace and command
  mulchie worktrees [--json]       every git worktree: size, idle days, dirty files, verdict
  mulchie sweep [--dry-run]        remove idle agent worktrees (patch backup first, branches kept)
  mulchie prune <cache>            run a tool's own prune command (npm, bun, pnpm, uv, brew, go…)
  mulchie signal <pid> <term|kill|stop|cont>
  mulchie advice [--json]          is it OK to start heavy work right now?
  mulchie run [--label L] -- <cmd> wait for a build slot, run with a fair job count, release
  mulchie mcp                      MCP server on stdio so agents can query and act
  mulchie serve                    run the monitor, optimizer and local API without a menu bar
  mulchie config                   print the config path and current settings
";

fn main() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    let code = match cmd {
        "status" => status(json),
        "scan" => scan(json),
        "clean" => clean(&args[1..]),
        "serve" => serve(),
        "top" => top(&args[1..], json),
        "worktrees" => worktrees(json),
        "sweep" => sweep(&args[1..]),
        "prune" => prune(&args[1..]),
        "signal" => signal(&args[1..]),
        "advice" => advice(json),
        "run" => run(&args[1..]),
        "mcp" => mcp::serve(),
        "config" => {
            println!("{}", Config::path().display());
            println!("{}", serde_json::to_string_pretty(&Config::load()).unwrap_or_default());
            0
        }
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            0
        }
        "-V" | "--version" => {
            println!("mulchie {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("unknown command: {other}\n\n{HELP}");
            2
        }
    };
    std::process::exit(code);
}

fn sample() -> mulchie_core::Snapshot {
    let mut m = Monitor::new();
    m.sample();
    std::thread::sleep(Duration::from_millis(600));
    m.sample()
}

fn status(json: bool) -> i32 {
    let s = sample();
    let cfg = Config::load();
    if json {
        println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
        return 0;
    }
    let health = mulchie_core::engine::health_of(&s, &cfg);
    println!(
        "health {:?}  ·  disk {} free of {}  ·  memory {:.0}% free  ·  cpu {:.0}% ({} cores)  ·  swap {}",
        health,
        human_bytes(s.disk_free),
        human_bytes(s.disk_total),
        s.mem_free_pct(),
        s.cpu_pct,
        s.cpu_count,
        human_bytes(s.swap_used)
    );
    println!("{} agent process(es), {} build process(es) across {} workspace(s)\n", s.agent_count, s.build_count, s.workspaces.len());
    for w in s.workspaces.iter().take(15) {
        println!(
            "  {:<48} {:>6.0}% cpu  {:>9}  {} agent · {} build",
            truncate(&w.label, 48),
            w.cpu_pct,
            human_bytes(w.mem_bytes),
            w.agents,
            w.builds
        );
    }
    0
}

fn scan(json: bool) -> i32 {
    let cfg = Config::load();
    let busy = sample().busy_paths();
    let r = mulchie_core::reclaim::scan(&cfg, &busy);
    if json {
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        return 0;
    }
    println!(
        "{} reclaimable in {} caches, {} of it stale (scan took {:.1}s)\n",
        human_bytes(r.total_bytes),
        r.items.len(),
        human_bytes(r.stale_bytes),
        r.took_ms as f64 / 1000.0
    );
    for i in r.items.iter().take(30) {
        let tag = if i.in_use { "in use" } else if i.stale { "stale" } else { "recent" };
        println!("  {:>9}  {:<7} {:<18} {}", human_bytes(i.bytes), tag, i.kind.label(), i.path.display());
    }
    0
}

fn clean(args: &[String]) -> i32 {
    let dry = args.iter().any(|a| a == "--dry-run");
    let stale = args.iter().any(|a| a == "--stale");
    let engine = Engine::new(Config::load());
    {
        let snap = sample();
        engine.state.lock().unwrap().snapshot = snap;
    }
    let paths: Vec<PathBuf> = if stale {
        let r = engine.run_scan();
        r.items.into_iter().filter(|i| i.stale).map(|i| i.path).collect()
    } else {
        args.iter().filter(|a| !a.starts_with("--")).map(PathBuf::from).collect()
    };
    if paths.is_empty() {
        println!("nothing to clean");
        return 0;
    }
    if dry {
        for p in &paths {
            println!("would clear {}", p.display());
        }
        return 0;
    }
    let results = engine.clean_paths(&paths, "cli");
    let mut freed = 0;
    let mut failed = 0;
    for r in &results {
        match &r.error {
            None => {
                freed += r.freed;
                println!("cleared {:>9}  {}", human_bytes(r.freed), r.path.display());
            }
            Some(e) => {
                failed += 1;
                eprintln!("skipped {}: {e}", r.path.display());
            }
        }
    }
    println!("\nfreed {}", human_bytes(freed));
    if failed > 0 { 1 } else { 0 }
}

fn serve() -> i32 {
    let cfg = Config::load();
    let port = cfg.api_port;
    let engine = Engine::new(cfg);
    engine.start();
    match mulchie_core::api::serve(engine, port) {
        Ok(h) => {
            println!("mulchie serving on http://127.0.0.1:{port}");
            let _ = h.join();
            0
        }
        Err(e) => {
            eprintln!("could not start API on port {port}: {e}");
            1
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn local_engine(scan: bool) -> Engine {
    let engine = Engine::new(Config::load());
    engine.state.lock().unwrap().snapshot = sample();
    if scan {
        engine.run_scan();
    }
    engine
}

fn state(scan: bool) -> Value {
    match Client::connect() {
        Some(c) => c.call("GET", "/v1/state", None).unwrap_or(Value::Null),
        None => serde_json::to_value(local_engine(scan).snapshot_state()).unwrap_or(Value::Null),
    }
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn top(args: &[String], json_out: bool) -> i32 {
    let st = state(false);
    let sort = flag(args, "--sort").unwrap_or("cpu");
    let n: usize = flag(args, "-n").and_then(|x| x.parse().ok()).unwrap_or(25);
    let mut procs: Vec<Value> = st["snapshot"]["top"].as_array().cloned().unwrap_or_default();
    let key = |p: &Value| match sort {
        "mem" => f(p, "mem_bytes"),
        "io" => f(p, "read_bps") + f(p, "write_bps"),
        _ => f(p, "cpu_pct"),
    };
    procs.sort_by(|a, b| key(b).total_cmp(&key(a)));
    procs.truncate(n);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&procs).unwrap_or_default());
        return 0;
    }
    let snap = &st["snapshot"];
    println!(
        "cpu {:.0}%  load {:.1} {:.1} {:.1}  mem {} used of {}  swap {}  disk io r {}/s w {}/s",
        f(snap, "cpu_pct"),
        snap["load"][0].as_f64().unwrap_or(0.0),
        snap["load"][1].as_f64().unwrap_or(0.0),
        snap["load"][2].as_f64().unwrap_or(0.0),
        human_bytes((f(snap, "mem_total") - f(snap, "mem_available")) as u64),
        human_bytes(f(snap, "mem_total") as u64),
        human_bytes(f(snap, "swap_used") as u64),
        human_bytes(f(snap, "read_bps") as u64),
        human_bytes(f(snap, "write_bps") as u64),
    );
    println!("{:>7} {:>6} {:>9} {:>9} {:>4} {:<8} {:<16} {}", "PID", "CPU%", "MEM", "IO/s", "NI", "KIND", "NAME", "GROUP");
    for p in &procs {
        println!(
            "{:>7} {:>6.1} {:>9} {:>9} {:>4} {:<8} {:<16} {}",
            p["pid"].as_i64().unwrap_or(0),
            f(p, "cpu_pct"),
            human_bytes(f(p, "mem_bytes") as u64),
            human_bytes((f(p, "read_bps") + f(p, "write_bps")) as u64),
            p["nice"].as_i64().unwrap_or(0),
            s(p, "kind"),
            truncate(s(p, "name"), 16),
            truncate(s(p, "group"), 50),
        );
    }
    0
}

fn worktrees(json_out: bool) -> i32 {
    let st = state(true);
    let wts = st["diskmap"]["worktrees"].as_array().cloned().unwrap_or_default();
    if json_out {
        println!("{}", serde_json::to_string_pretty(&wts).unwrap_or_default());
        return 0;
    }
    if wts.is_empty() {
        println!("no worktrees found yet (the first scan may still be running)");
    }
    for w in &wts {
        println!(
            "{:>9} {:>4}d {:>3} dirty  {:<44} {}",
            human_bytes(f(w, "bytes") as u64),
            w["idle_days"].as_i64().unwrap_or(0),
            w["dirty_files"].as_i64().unwrap_or(0),
            truncate(&format!("{} › {}", s(w, "repo_name"), s(w, "name")), 44),
            s(w, "verdict")
        );
    }
    0
}

fn sweep(args: &[String]) -> i32 {
    let dry = args.iter().any(|a| a == "--dry-run");
    let client = Client::connect();
    let plan: Vec<Value> = match &client {
        Some(c) => c.call("GET", "/v1/sweep", None).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default(),
        None => local_engine(true).sweep_plan().into_iter().filter_map(|w| serde_json::to_value(w).ok()).collect(),
    };
    if plan.is_empty() {
        println!("no idle agent worktrees to sweep");
        return 0;
    }
    for w in &plan {
        println!(
            "{} {:>9}  {} › {}  ({})",
            if dry { "would remove" } else { "removing" },
            human_bytes(f(w, "bytes") as u64),
            s(w, "repo_name"),
            s(w, "name"),
            s(w, "verdict")
        );
    }
    if dry {
        return 0;
    }
    let results: Vec<Value> = match &client {
        Some(c) => c.call("POST", "/v1/sweep", Some(&json!({}))).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default(),
        None => local_engine(true).sweep_worktrees("cli").into_iter().filter_map(|r| serde_json::to_value(r).ok()).collect(),
    };
    report_removals(&results)
}

fn report_removals(results: &[Value]) -> i32 {
    let mut freed = 0u64;
    let mut failed = 0;
    for r in results {
        if r["ok"].as_bool().unwrap_or(false) {
            freed += f(r, "freed") as u64;
            println!("  ok    {}  {}", s(r, "path"), s(r, "message"));
        } else {
            failed += 1;
            println!("  skip  {}  {}", s(r, "path"), s(r, "message"));
        }
    }
    println!("freed {}", human_bytes(freed));
    i32::from(failed > 0)
}

fn prune(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: mulchie prune <npm|bun|pnpm|yarn|uv|pip|homebrew|go|simulators>");
        return 2;
    };
    let r = match Client::connect() {
        Some(c) => c.call("POST", "/v1/prune", Some(&json!({ "key": key }))).unwrap_or(Value::Null),
        None => serde_json::to_value(mulchie_core::diskmap::prune_cache(key)).unwrap_or(Value::Null),
    };
    println!("{}", s(&r, "output"));
    println!("freed {}", human_bytes(f(&r, "freed") as u64));
    i32::from(!r["ok"].as_bool().unwrap_or(false))
}

fn signal(args: &[String]) -> i32 {
    let (Some(pid), Some(sig)) = (args.first().and_then(|p| p.parse::<u32>().ok()), args.get(1)) else {
        eprintln!("usage: mulchie signal <pid> <term|kill|stop|cont>");
        return 2;
    };
    let Some(c) = Client::connect() else {
        eprintln!("mulchie is not running; start Mulchie.app or `mulchie serve`");
        return 1;
    };
    match c.call("POST", "/v1/process", Some(&json!({ "pid": pid, "action": sig }))) {
        Ok(_) => {
            println!("sent {sig} to {pid}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn advice(json_out: bool) -> i32 {
    let v = match Client::connect() {
        Some(c) => c.call("GET", "/v1/advice", None).unwrap_or(Value::Null),
        None => serde_json::to_value(local_engine(false).advice()).unwrap_or(Value::Null),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else if v["ok_to_start_heavy_work"].as_bool().unwrap_or(false) {
        println!("ok: {} free build slot(s), use -j{}", v["free_slots"], v["suggested_jobs"]);
    } else {
        let reasons: Vec<&str> = v["reasons"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        println!("wait: {}", reasons.join("; "));
    }
    0
}

fn run(args: &[String]) -> i32 {
    let split = args.iter().position(|a| a == "--").map(|i| i + 1).unwrap_or(0);
    let label = flag(&args[..split], "--label").map(String::from);
    let cmd = &args[split..];
    let Some(program) = cmd.first() else {
        eprintln!("usage: mulchie run [--label L] -- <command> [args...]");
        return 2;
    };
    let label = label.unwrap_or_else(|| {
        let cwd = std::env::current_dir().ok().and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
        format!("{cwd}: {}", cmd.join(" "))
    });
    let client = Client::connect();
    let mut ticket: Option<u64> = None;
    let mut jobs: Option<u64> = None;
    if let Some(c) = &client {
        let started = std::time::Instant::now();
        let mut last_reason = String::new();
        loop {
            let body = json!({ "label": label, "pid": std::process::id(), "ticket": ticket });
            match c.call("POST", "/v1/lease", Some(&body)) {
                Ok(r) => {
                    ticket = r["ticket"].as_u64();
                    if r["granted"].as_bool().unwrap_or(false) {
                        jobs = r["jobs"].as_u64();
                        if started.elapsed() > Duration::from_secs(1) {
                            eprintln!("mulchie: slot granted after {:.0}s", started.elapsed().as_secs_f64());
                        }
                        break;
                    }
                    let reason = s(&r, "reason").to_string();
                    if reason != last_reason {
                        eprintln!("mulchie: waiting — {reason}");
                        last_reason = reason;
                    }
                }
                Err(e) => {
                    eprintln!("mulchie: {e}; running without a slot");
                    break;
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    let mut child = std::process::Command::new(program);
    child.args(&cmd[1..]);
    if let Some(j) = jobs {
        child.env("CARGO_BUILD_JOBS", j.to_string());
        if std::env::var_os("MAKEFLAGS").is_none() {
            child.env("MAKEFLAGS", format!("-j{j}"));
        }
    }
    let code = match child.status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(e) => {
            eprintln!("mulchie: could not run {program}: {e}");
            127
        }
    };
    if let (Some(c), Some(t)) = (&client, ticket) {
        let _ = c.call("POST", "/v1/release", Some(&json!({ "ticket": t })));
    }
    code
}
