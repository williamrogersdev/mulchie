//! `sweep` and `clean`: fetch a plan, show it, confirm, then execute exactly
//! the requests of its ready items. The same flow runs against the engine and
//! against a local engine.

use crate::cli::Confirm;
use crate::conn::{self, Conn};
use mulchie_core::api::Client;
use mulchie_core::safety::BackupKind;
use mulchie_core::{
    human_bytes, ActionResult, Config, Engine, Outcome, Paths, Plan, PlanItem, Request,
};
use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

/// What to plan.
#[derive(Debug, Clone)]
pub enum Job {
    Sweep,
    CleanStale,
    CleanPaths(Vec<PathBuf>),
}

impl Job {
    fn verb(&self) -> &'static str {
        match self {
            Job::Sweep => "remove",
            _ => "delete",
        }
    }

    fn noun(&self) -> &'static str {
        match self {
            Job::Sweep => "worktree(s)",
            _ => "cache(s)",
        }
    }
}

/// Said when the engine took a plan but gave no usable answer.
pub const MAY_HAVE_ACTED: &str = "The engine may have acted on some of the plan: check `mulchie status` or the action journal before trying again";

/// Whether the engine may have carried out some of a request that failed
/// this way: no answer (or a lost one), or a server error. A refusal (4xx)
/// came before anything ran.
pub fn may_have_acted(e: &mulchie_core::api::ApiError) -> bool {
    match e.status {
        0 => !matches!(
            e.code.as_str(),
            "engine_not_running" | "engine_unreachable" | "unreachable"
        ),
        s => s >= 500,
    }
}

/// An execute request's error in words, with [`MAY_HAVE_ACTED`] only when
/// that can be so.
pub fn execute_error(e: &mulchie_core::api::ApiError) -> String {
    if may_have_acted(e) {
        format!("{}. {MAY_HAVE_ACTED}", conn::describe(e))
    } else {
        conn::describe(e)
    }
}

/// Where plans come from and go to.
pub enum Backend {
    Engine(Client),
    Local(Engine),
}

impl Backend {
    pub fn source(&self) -> &'static str {
        match self {
            Backend::Engine(_) => "engine",
            Backend::Local(_) => "local",
        }
    }

    pub fn plan(&self, job: &Job) -> Result<Plan, String> {
        match self {
            Backend::Engine(c) => {
                let v = match job {
                    Job::Sweep => conn::call(c, "GET", "/v1/sweep", None),
                    Job::CleanStale => conn::call(c, "GET", "/v1/clean/plan", None),
                    Job::CleanPaths(p) => {
                        conn::call(c, "POST", "/v1/clean/plan", Some(&json!({ "paths": p })))
                    }
                }
                .map_err(|e| conn::describe(&e))?;
                serde_json::from_value(v)
                    .map_err(|e| format!("unexpected plan from the engine: {e}"))
            }
            Backend::Local(e) => Ok(match job {
                Job::Sweep => e.sweep_plan(),
                Job::CleanStale => e.reclaim_plan(None),
                Job::CleanPaths(p) => e.clean_plan(p),
            }),
        }
    }

    pub fn execute(&self, job: &Job, items: &[Request]) -> Result<Vec<ActionResult>, String> {
        match self {
            Backend::Engine(c) => {
                let route = match job {
                    Job::Sweep => "/v1/sweep",
                    _ => "/v1/clean",
                };
                let v = conn::call(c, "POST", route, Some(&json!({ "items": items })))
                    .map_err(|e| execute_error(&e))?;
                serde_json::from_value(v).map_err(|e| {
                    format!("unexpected reply from the engine; versions may differ ({e}). {MAY_HAVE_ACTED}")
                })
            }
            Backend::Local(e) => {
                // Hold the engine lock while acting, so an engine (the app
                // or `mulchie serve`) cannot start and act on the same items
                // at the same time. The lock is released when this returns.
                let _lock = e.acquire_lock().map_err(|err| {
                    format!("{err}; nothing was changed. Run the command again so it goes through that engine")
                })?;
                Ok(e.execute(items, "cli"))
            }
        }
    }
}

/// How the run was confirmed.
pub struct Ui<'a> {
    pub confirm: Confirm,
    pub json: bool,
    /// stdin is a terminal, so we may ask.
    pub interactive: bool,
    /// Asks the question; true means go ahead.
    pub ask: &'a mut dyn FnMut(&str) -> bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// The CLI entry point: pick the engine or a local engine, then [`run`].
pub fn command(paths: &Paths, job: Job, confirm: Confirm, json_out: bool) -> i32 {
    let job = match job {
        Job::CleanPaths(p) => Job::CleanPaths(
            p.into_iter()
                .map(|p| std::path::absolute(&p).unwrap_or(p))
                .collect(),
        ),
        j => j,
    };
    let backend = match conn::connect(paths) {
        Conn::Engine(c) => Backend::Engine(c),
        Conn::NotRunning => {
            Conn::NotRunning
                .note_local("planning locally; each item is still re-checked before it is touched");
            Backend::Local(Engine::new(paths.clone(), Config::load_from(paths)))
        }
        other => {
            // An engine exists but cannot be used: acting beside it would
            // bypass its journal and state, so stop.
            eprintln!(
                "mulchie: {}",
                other.explain_required(match job {
                    Job::Sweep => "`mulchie sweep`",
                    _ => "`mulchie clean`",
                })
            );
            return 1;
        }
    };
    let mut ask = |q: &str| -> bool {
        eprint!("{q} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut ui = Ui {
        confirm,
        json: json_out,
        interactive: std::io::stdin().is_terminal(),
        ask: &mut ask,
        out: &mut out,
        err: &mut err,
    };
    run(&backend, &job, &mut ui)
}

/// Plan, show, confirm, execute exactly the ready items, report.
///
/// Exit status: 0 when everything approved was done (or nothing was to do,
/// or a dry run), 1 when an item was skipped at execution or a call failed,
/// 2 when confirmation was needed and impossible.
pub fn run(backend: &Backend, job: &Job, ui: &mut Ui) -> i32 {
    let plan = match backend.plan(job) {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(ui.err, "mulchie: {e}");
            return 1;
        }
    };
    let approved = plan.approved();
    let emit_json = |ui: &mut Ui, results: Option<&[ActionResult]>| {
        let v = json!({
            "source": backend.source(),
            "plan": plan,
            "executed": results.is_some(),
            "results": results,
        });
        let _ = writeln!(
            ui.out,
            "{}",
            serde_json::to_string_pretty(&v).unwrap_or_default()
        );
    };

    // In JSON mode stdout carries only JSON; the readable plan goes to stderr
    // when someone has to read it to answer.
    let show_text = !ui.json
        || (ui.interactive && !ui.confirm.yes && !ui.confirm.dry_run && !approved.is_empty());
    if show_text {
        let text = render_plan(job, &plan);
        let w: &mut dyn Write = if ui.json { &mut *ui.err } else { &mut *ui.out };
        let _ = w.write_all(text.as_bytes());
    }

    if approved.is_empty() || ui.confirm.dry_run {
        if ui.json {
            emit_json(ui, None);
        }
        return 0;
    }
    if !ui.confirm.yes {
        if !ui.interactive {
            let _ = writeln!(
                ui.err,
                "mulchie: stdin is not a terminal, so nothing was changed; re-run with --yes to execute this plan, or --dry-run to only show it"
            );
            if ui.json {
                emit_json(ui, None);
            }
            return 2;
        }
        let q = format!(
            "{} these {} {} ({})?",
            capitalise(job.verb()),
            approved.len(),
            job.noun(),
            human_bytes(plan.ready_bytes)
        );
        if !(ui.ask)(&q) {
            let _ = writeln!(ui.err, "mulchie: nothing changed");
            if ui.json {
                emit_json(ui, None);
            }
            return 0;
        }
    }

    let results = match backend.execute(job, &approved) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(ui.err, "mulchie: {e}");
            return 1;
        }
    };
    if ui.json {
        emit_json(ui, Some(&results));
    } else {
        let _ = ui.out.write_all(render_results(&plan, &results).as_bytes());
    }
    i32::from(results.iter().any(|r| !r.is_done()))
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn label(i: &PlanItem) -> String {
    match &i.repo_name {
        Some(r) => format!("{r} › {}", i.name),
        None => i.target.clone(),
    }
}

const NOT_READY_SHOWN: usize = 10;

/// The plan as text: ready items in full, the rest briefly with reasons.
pub fn render_plan(job: &Job, plan: &Plan) -> String {
    let mut s = String::new();
    let ready: Vec<&PlanItem> = plan.items.iter().filter(|i| i.ready).collect();
    let not_ready: Vec<&PlanItem> = plan.items.iter().filter(|i| !i.ready).collect();
    if plan.items.is_empty() {
        s.push_str(match job {
            Job::Sweep => "No idle agent worktrees to sweep.\n",
            Job::CleanStale => "No stale caches to delete.\n",
            Job::CleanPaths(_) => "Nothing to delete.\n",
        });
        return s;
    }
    if ready.is_empty() {
        s.push_str(&format!("Nothing to {}.\n", job.verb()));
    } else {
        s.push_str(&format!(
            "Plan: {} {} {}, {}\n",
            job.verb(),
            ready.len(),
            job.noun(),
            human_bytes(plan.ready_bytes)
        ));
        for i in &ready {
            let what = match &i.request {
                Request::DeleteCache { .. } => format!("{} {}", i.name, i.target),
                Request::PruneWorktree { .. } => format!("{} (prune entry)", label(i)),
                _ => label(i),
            };
            s.push_str(&format!(
                "  {:<6} {:>9}  {}  ({})\n",
                job.verb(),
                human_bytes(i.bytes),
                what,
                i.verdict
            ));
            for b in &i.would_backup {
                s.push_str(&format!("{:>20}saves {b}\n", ""));
            }
        }
    }
    if !not_ready.is_empty() {
        s.push_str(&format!("Not ready ({}):\n", not_ready.len()));
        for i in not_ready.iter().take(NOT_READY_SHOWN) {
            let code = i.reason.map(|r| r.code()).unwrap_or_default();
            s.push_str(&format!("  skip   {}  {} [{code}]\n", label(i), i.verdict));
        }
        if not_ready.len() > NOT_READY_SHOWN {
            s.push_str(&format!(
                "  … and {} more (--json lists them)\n",
                not_ready.len() - NOT_READY_SHOWN
            ));
        }
    }
    s
}

/// Per-item results with backups and skip reasons.
pub fn render_results(plan: &Plan, results: &[ActionResult]) -> String {
    let mut s = String::new();
    let name_of = |target: &str| {
        plan.items
            .iter()
            .find(|i| i.target == target)
            .map(label)
            .unwrap_or_else(|| target.to_string())
    };
    let mut freed = 0;
    let mut skipped = 0;
    for r in results {
        match &r.outcome {
            Outcome::Done {
                freed_bytes,
                backups,
            } => {
                freed += freed_bytes;
                s.push_str(&format!(
                    "  done     {:>9}  {}\n",
                    human_bytes(*freed_bytes),
                    name_of(&r.target)
                ));
                for b in backups {
                    let what = match b.kind {
                        BackupKind::Patch => "patch saved to",
                        BackupKind::IgnoredArchive => "ignored files saved to",
                        BackupKind::Branch => "backup branch",
                    };
                    s.push_str(&format!("{:>22}{what} {}\n", "", b.location));
                }
            }
            Outcome::Skipped { reason, detail } => {
                skipped += 1;
                s.push_str(&format!(
                    "  skipped             {}: {detail} [{}]\n",
                    name_of(&r.target),
                    reason.code()
                ));
            }
        }
    }
    s.push_str(&format!(
        "Freed {}; {} done, {} skipped.\n",
        human_bytes(freed),
        results.len() - skipped,
        skipped
    ));
    s
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
