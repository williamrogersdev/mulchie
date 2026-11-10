mod cli;
mod commands;
mod conn;
mod diagnose;
mod doctor;
mod hook;
mod hook_wire;
mod hooks_install;
mod json_faithful;
mod mcp;
mod mcp_install;
mod mcp_tools;
mod observe;
mod plan;
mod process;
mod run;
mod service;
mod sessions;
mod slots;

use clap::{CommandFactory, Parser};
use cli::{Cli, Command, ConfigCommand, HooksCommand, McpCommand};
use mulchie_core::Paths;

fn main() {
    // Die quietly on a closed pipe (`mulchie top | head`), like other tools.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cli = if args.get(1).map(String::as_str) == Some("hook") {
        // A hook must never block its client over a usage error.
        match Cli::try_parse_from(&args) {
            Ok(c) => c,
            Err(e)
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                e.exit()
            }
            Err(e) => {
                let out = hook::usage_error(&Paths::from_env(), &args, &e.to_string());
                hook::emit(&out);
                std::process::exit(0);
            }
        }
    } else {
        Cli::parse()
    };
    let paths = Paths::from_env();
    let code = match cli.command {
        None => commands::status(&paths, false),
        Some(cmd) => dispatch(&paths, cmd),
    };
    std::process::exit(code);
}

fn dispatch(paths: &Paths, cmd: Command) -> i32 {
    match cmd {
        Command::Status(j) => commands::status(paths, j.json),
        Command::Scan(j) => commands::scan(paths, j.json),
        Command::Clean(c) => {
            let job = if c.stale {
                plan::Job::CleanStale
            } else {
                plan::Job::CleanPaths(c.paths)
            };
            plan::command(paths, job, c.confirm, c.out.json)
        }
        Command::Top(t) => commands::top(paths, &t),
        Command::Worktrees(j) => commands::worktrees(paths, j.json),
        Command::Sweep(s) => plan::command(paths, plan::Job::Sweep, s.confirm, s.out.json),
        Command::Prune(p) => commands::prune(paths, &p.cache, p.out.json),
        Command::Signal(s) => process::command(
            paths,
            process::Target::Pid {
                pid: s.pid,
                signal: s.signal.as_str().into(),
                tree: s.tree,
            },
            s.confirm,
            s.out.json,
        ),
        Command::Group(g) => {
            let target = match process::Target::from_key(&g.key, g.action.as_str(), g.include_agent)
            {
                Ok(t) => t,
                Err(e) => return commands::fail(e),
            };
            process::command(paths, target, g.confirm, g.out.json)
        }
        Command::Sessions(a) => match a.action {
            Some(cli::SessionsCommand::Stop {
                id,
                include_agent,
                force,
                confirm,
                out,
            }) => {
                if let Err(e) = sessions::check_session_id(&id) {
                    return commands::fail(e);
                }
                let action = if force { "force_quit" } else { "quit" };
                process::command(
                    paths,
                    process::Target::Session {
                        id,
                        action: action.into(),
                        include_agent,
                    },
                    confirm,
                    out.json,
                )
            }
            None => sessions::sessions(
                paths,
                a.id.as_deref(),
                a.events,
                a.since,
                a.limit,
                a.out.json,
            ),
        },
        Command::Limits(j) => sessions::limits(paths, j.json),
        Command::Updates(u) => sessions::updates(paths, u.check, u.out.json),
        Command::Guard(cli::GuardCommand::Check { cwd, out, command }) => {
            sessions::guard_check(paths, &command, cwd.as_deref(), out.json)
        }
        Command::Findings(f) => match &f.action {
            Some(a) => observe::findings_action(paths, a),
            None => observe::findings(paths, f.severity, f.kind.as_deref(), f.all, f.out.json),
        },
        Command::Inspect(i) => observe::inspect(paths, i.pid, i.out.json),
        Command::Slots(a) => match &a.action {
            Some(cli::SlotsCommand::Release { ticket, yes, out }) => {
                slots::release(paths, *ticket, *yes, out.json)
            }
            None => slots::list(paths, a.out.json),
        },
        Command::Advice(j) => commands::advice(paths, j.json),
        Command::Run(r) => run::run(paths, &r),
        Command::Diagnose(j) => commands::diagnose(paths, j.json),
        Command::Mcp(m) => match m.action {
            None => mcp::command(paths, m.read_only),
            Some(McpCommand::Install(a)) => mcp_install::install(&a),
            Some(McpCommand::Uninstall(a)) => mcp_install::uninstall(&a),
            Some(McpCommand::Config { read_only }) => mcp_install::config(read_only),
        },
        Command::Token(t) => commands::token(paths, t.read_only, t.show),
        Command::Serve => commands::serve(paths),
        Command::Config(c) => match c.action {
            None => observe::config_show(paths, c.out.json),
            Some(ConfigCommand::Set { key, value, out }) => {
                observe::config_set(paths, &key, &value, out.json || c.out.json)
            }
        },
        Command::Doctor(j) => doctor::command(paths, j.json),
        Command::Hooks(h) => match h {
            HooksCommand::Install {
                client,
                dry_run,
                guard,
            } => hooks_install::install(client, dry_run, guard),
            HooksCommand::Uninstall { client, dry_run } => {
                hooks_install::uninstall(client, dry_run)
            }
            HooksCommand::Status { out } => hooks_install::status(out.json),
        },
        Command::Hook(h) => hook::command(paths, h.client, &h.event, h.guard),
        Command::Service(s) => service::command(paths, &s),
        Command::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "mulchie",
                &mut std::io::stdout(),
            );
            0
        }
    }
}
