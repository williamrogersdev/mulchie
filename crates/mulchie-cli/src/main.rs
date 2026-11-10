mod cli;
mod commands;
mod conn;
mod diagnose;
mod doctor;
mod mcp;
mod mcp_install;
mod mcp_tools;
mod plan;
mod run;
mod service;

use clap::{CommandFactory, Parser};
use cli::{Cli, Command, McpCommand};
use mulchie_core::Paths;

fn main() {
    // Die quietly on a closed pipe (`mulchie top | head`), like other tools.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
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
        Command::Signal(s) => commands::signal(paths, s.pid, s.signal.as_str()),
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
        Command::Config(j) => commands::config(paths, j.json),
        Command::Doctor(j) => doctor::command(paths, j.json),
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
