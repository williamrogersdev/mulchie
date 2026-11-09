//! Command-line definition (clap derive). Parsing only; the commands live in
//! their own modules.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

const ABOUT: &str = "Keeps a machine healthy while many AI coding agents work on it";

const LONG_ABOUT: &str = "\
Keeps a machine healthy while many AI coding agents work on it: monitors CPU, \
memory, disk and I/O per repo and worktree, queues heavy builds machine-wide, \
and reclaims stale build caches and idle agent worktrees without losing work.

Most commands talk to the running engine (the Mulchie app, `mulchie serve` or \
the service from `mulchie service install`). When none is running, read \
commands and `clean`/`sweep` work locally and say so on stderr.";

const EXAMPLES: &str = "\
Examples:
  mulchie                        Same as `mulchie status`
  mulchie doctor                 Check the install end to end
  mulchie run -- cargo build     Wait for a build slot, then build
  mulchie sweep --dry-run        Show which idle agent worktrees would go
  mulchie service install        Run the engine in the background at login
  mulchie completions zsh > ~/.zfunc/_mulchie";

#[derive(Parser, Debug)]
#[command(
    name = "mulchie",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = EXAMPLES,
    max_term_width = 100
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Disk, memory, CPU and what each agent workspace is using
    #[command(after_help = "Examples:\n  mulchie status\n  mulchie status --json | jq .health")]
    Status(JsonArg),

    /// Find build caches and show which are stale
    #[command(after_help = "Examples:\n  mulchie scan\n  mulchie scan --json")]
    Scan(JsonArg),

    /// Delete build caches: show the plan, confirm, then delete exactly that plan
    #[command(
        long_about = "Delete build caches. Mulchie shows a plan first: the caches it would \
delete and, briefly, the ones it would not and why. After you confirm, it deletes \
exactly the planned caches, each re-checked just before deletion (still a recognised \
cache, nothing running in its project, and with --stale still stale).\n\n\
Only recognised build caches are deleted: Rust `target/` with CACHEDIR.TAG, `.next`, \
`node_modules` when enabled, and Xcode DerivedData when enabled.",
        after_help = "Examples:\n  mulchie clean --stale --dry-run\n  mulchie clean --stale\n  mulchie clean --stale --yes\n  mulchie clean ~/Developer/app/target"
    )]
    Clean(CleanArgs),

    /// Processes by CPU, memory or disk I/O with workspace and command
    #[command(
        after_help = "Examples:\n  mulchie top\n  mulchie top --sort mem -n 10\n  mulchie top --json"
    )]
    Top(TopArgs),

    /// Every git worktree: size, idle days, dirty files, verdict
    #[command(after_help = "Examples:\n  mulchie worktrees\n  mulchie worktrees --json")]
    Worktrees(JsonArg),

    /// Remove idle agent worktrees: show the plan, confirm, then remove exactly that plan
    #[command(
        long_about = "Remove idle agent worktrees. Mulchie shows a plan first: the worktrees it \
would remove and, briefly, the ones it would not and why. After you confirm, it removes \
exactly the planned worktrees. Each is re-checked just before removal; if its HEAD moved \
since the plan, it is skipped.\n\n\
Nothing is lost: uncommitted changes are saved as a patch, gitignored files that are not \
caches are archived, a detached HEAD gets a backup/<name> branch, and branches are kept.",
        after_help = "Examples:\n  mulchie sweep --dry-run\n  mulchie sweep\n  mulchie sweep --yes --json"
    )]
    Sweep(SweepArgs),

    /// Run a package manager's own prune command (npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators)
    #[command(after_help = "Examples:\n  mulchie prune npm\n  mulchie prune go --json")]
    Prune(PruneArgs),

    /// Send a signal to one of your processes (needs the engine)
    #[command(
        after_help = "Examples:\n  mulchie signal 4242 term\n  mulchie signal 4242 stop\n  mulchie signal 4242 cont"
    )]
    Signal(SignalArgs),

    /// Is it OK to start heavy work right now?
    #[command(after_help = "Examples:\n  mulchie advice\n  mulchie advice --json")]
    Advice(JsonArg),

    /// Wait for a build slot, run a command with a fair job count, release the slot
    #[command(
        long_about = "Wait for a slot in the machine-wide build queue, then run the command \
with CARGO_BUILD_JOBS (and MAKEFLAGS, unless already set) set to a fair job count.\n\n\
The slot is leased to this `mulchie run` process and released when the command ends, \
however it ends; if `mulchie run` itself is killed, the engine frees the slot when it \
notices the process is gone. SIGINT and SIGTERM are forwarded to the command (a Ctrl-C \
in the terminal reaches it directly). The exit status is the command's, or 128 + the \
signal number when it was killed by a signal.\n\n\
When no engine is running there is no queue: Mulchie says so in one line on stderr \
and runs the command straight away, unqueued.",
        after_help = "Examples:\n  mulchie run -- cargo build --release\n  mulchie run --label 'api tests' -- cargo test\n  mulchie run make -j8"
    )]
    Run(RunArgs),

    /// MCP server on stdio, so agents can query and act
    #[command(
        long_about = "Run an MCP server on stdin/stdout (JSON-RPC, one message per line). \
Agents use it to check machine health, join the build queue, and reclaim space through \
the same plan-then-execute safety gate as the CLI. It needs a running engine.",
        after_help = "Examples:\n  claude mcp add mulchie -- mulchie mcp"
    )]
    Mcp,

    /// Run the engine (monitor, optimizer, build queue, local API) without the app
    #[command(
        long_about = "Run the engine in the foreground: the monitor, the optimizer, the build \
queue and the local HTTP API on 127.0.0.1. Only one engine runs per user; if the app or \
another `mulchie serve` already runs, this exits with status 1. Stops cleanly on SIGINT \
or SIGTERM. The port is `api_port` from the config, or MULCHIE_PORT for this process.",
        after_help = "Examples:\n  mulchie serve\n  MULCHIE_PORT=47575 mulchie serve\n  mulchie service install   # run it at login instead"
    )]
    Serve,

    /// Print the config path, any config error, and the current settings
    #[command(after_help = "Examples:\n  mulchie config\n  mulchie config --json")]
    Config(JsonArg),

    /// Check the install end to end; exits 1 if any check fails
    #[command(
        long_about = "Check the install end to end and print pass, warn or fail per check: the \
config file, config directory and token permissions, the running engine (lock holder, \
port, health, sample age), git, which `mulchie` is on PATH, the background service, and \
how to add the MCP server. Nothing is changed. Exits 1 if any check fails.",
        after_help = "Examples:\n  mulchie doctor\n  mulchie doctor --json"
    )]
    Doctor(JsonArg),

    /// Install, remove or inspect the background service (launchd on macOS, systemd on Linux)
    #[command(subcommand)]
    Service(ServiceCommand),

    /// Print a shell completion script
    #[command(
        after_help = "Examples:\n  mulchie completions bash > ~/.local/share/bash-completion/completions/mulchie\n  mulchie completions zsh > ~/.zfunc/_mulchie\n  mulchie completions fish > ~/.config/fish/completions/mulchie.fish"
    )]
    Completions {
        /// The shell to generate completions for
        shell: clap_complete::Shell,
    },
}

#[derive(Args, Debug, Clone, Copy, Default)]
pub struct JsonArg {
    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Copy, Default)]
pub struct Confirm {
    /// Do not ask for confirmation (required when stdin is not a terminal)
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print the plan and stop
    #[arg(long, short = 'n', conflicts_with = "yes")]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Every stale cache: unused for the stale window, with nothing running in its project
    #[arg(long, conflicts_with = "paths", required_unless_present = "paths")]
    pub stale: bool,
    /// Specific cache directories to delete
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    #[command(flatten)]
    pub confirm: Confirm,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(Args, Debug)]
pub struct SweepArgs {
    #[command(flatten)]
    pub confirm: Confirm,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Cpu,
    Mem,
    Io,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// What to rank by
    #[arg(long, value_enum, default_value_t = SortKey::Cpu)]
    pub sort: SortKey,
    /// How many processes to show
    #[arg(short = 'n', long = "limit", default_value_t = 25)]
    pub limit: usize,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// The cache key: npm, bun, pnpm, yarn, uv, pip, homebrew, go or simulators
    pub cache: String,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalName {
    /// SIGTERM: ask it to stop
    Term,
    /// SIGKILL: stop it now
    Kill,
    /// SIGSTOP: pause it
    Stop,
    /// SIGCONT: resume it
    Cont,
}

impl SignalName {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalName::Term => "term",
            SignalName::Kill => "kill",
            SignalName::Stop => "stop",
            SignalName::Cont => "cont",
        }
    }
}

#[derive(Args, Debug)]
pub struct SignalArgs {
    /// The process id
    pub pid: u32,
    /// The signal
    #[arg(value_enum)]
    pub signal: SignalName,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The name shown in the queue (default: the directory and the command)
    #[arg(long, short = 'l')]
    pub label: Option<String>,
    /// The command and its arguments (put them after `--`)
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Write and load the service that runs `mulchie serve` at login
    #[command(
        long_about = "Write the service file for `mulchie serve` and load it: a launchd agent at \
~/Library/LaunchAgents/dev.mulchie.serve.plist on macOS, a systemd user unit at \
~/.config/systemd/user/mulchie.service on Linux. It runs this binary by its absolute path. \
An existing file with different content is not replaced without --force. On macOS the \
menu bar app already runs the engine, so a service is only for headless use.",
        after_help = "Examples:\n  mulchie service install --dry-run\n  mulchie service install\n  mulchie service install --force"
    )]
    Install {
        /// Replace an existing service file with different content
        #[arg(long)]
        force: bool,
        /// Print the file and the commands, change nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Stop and remove the service
    #[command(
        after_help = "Examples:\n  mulchie service uninstall --dry-run\n  mulchie service uninstall"
    )]
    Uninstall {
        /// Print what would be run and removed, change nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Show whether the service is installed, current and loaded
    #[command(after_help = "Examples:\n  mulchie service status\n  mulchie service status --json")]
    Status(JsonArg),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("mulchie").chain(args.iter().copied()))
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_command_means_status() {
        assert!(parse(&[]).unwrap().command.is_none());
    }

    #[test]
    fn version_flag() {
        let e = parse(&["--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(e.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn unknown_flags_are_errors() {
        for args in [
            &["status", "--jsno"][..],
            &["sweep", "--force"],
            &["clean", "--stale", "--bogus"],
            &["--bogus"],
            &["frobnicate"],
        ] {
            let e = parse(args).unwrap_err();
            assert!(
                matches!(
                    e.kind(),
                    clap::error::ErrorKind::UnknownArgument
                        | clap::error::ErrorKind::InvalidSubcommand
                ),
                "{args:?}: {:?}",
                e.kind()
            );
        }
    }

    #[test]
    fn json_flags() {
        for c in ["status", "scan", "worktrees", "advice", "config", "doctor"] {
            assert!(parse(&[c, "--json"]).is_ok(), "{c}");
        }
        assert!(parse(&["top", "--json"]).is_ok());
        assert!(parse(&["service", "status", "--json"]).is_ok());
    }

    #[test]
    fn clean_needs_stale_or_paths() {
        assert!(parse(&["clean"]).is_err());
        assert!(parse(&["clean", "--stale", "/x/target"]).is_err());
        let Some(Command::Clean(c)) = parse(&["clean", "--stale", "--dry-run"]).unwrap().command
        else {
            panic!()
        };
        assert!(c.stale && c.confirm.dry_run && !c.confirm.yes);
        let Some(Command::Clean(c)) = parse(&["clean", "-y", "/a/target", "/b/.next"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(c.paths.len(), 2);
        assert!(c.confirm.yes);
        assert!(parse(&["clean", "--stale", "--yes", "--dry-run"]).is_err());
    }

    #[test]
    fn sweep_flags() {
        let Some(Command::Sweep(s)) = parse(&["sweep", "--yes", "--json"]).unwrap().command else {
            panic!()
        };
        assert!(s.confirm.yes && s.out.json);
        assert!(parse(&["sweep", "extra"]).is_err());
    }

    #[test]
    fn top_flags() {
        let Some(Command::Top(t)) = parse(&["top", "--sort", "mem", "-n", "5"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!(t.sort, SortKey::Mem);
        assert_eq!(t.limit, 5);
        assert!(parse(&["top", "--sort", "disk"]).is_err());
        assert!(parse(&["top", "-n", "x"]).is_err());
    }

    #[test]
    fn signal_args() {
        let Some(Command::Signal(s)) = parse(&["signal", "42", "stop"]).unwrap().command else {
            panic!()
        };
        assert_eq!((s.pid, s.signal), (42, SignalName::Stop));
        assert!(parse(&["signal", "42", "hup"]).is_err());
        assert!(parse(&["signal", "-1", "term"]).is_err());
    }

    #[test]
    fn run_takes_the_rest() {
        let Some(Command::Run(r)) =
            parse(&["run", "--label", "x", "--", "cargo", "build", "--release"])
                .unwrap()
                .command
        else {
            panic!()
        };
        assert_eq!(r.label.as_deref(), Some("x"));
        assert_eq!(r.command, ["cargo", "build", "--release"]);
        // Without `--`, flags after the command belong to the command.
        let Some(Command::Run(r)) = parse(&["run", "make", "-j8", "--label", "y"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(r.command, ["make", "-j8", "--label", "y"]);
        assert!(r.label.is_none());
        assert!(parse(&["run"]).is_err());
        assert!(parse(&["run", "--"]).is_err());
    }

    #[test]
    fn service_subcommands() {
        let Some(Command::Service(ServiceCommand::Install { force, dry_run })) =
            parse(&["service", "install", "--force", "--dry-run"])
                .unwrap()
                .command
        else {
            panic!()
        };
        assert!(force && dry_run);
        assert!(matches!(
            parse(&["service", "uninstall", "-n"]).unwrap().command,
            Some(Command::Service(ServiceCommand::Uninstall {
                dry_run: true
            }))
        ));
        assert!(parse(&["service"]).is_err());
        assert!(parse(&["service", "restart"]).is_err());
    }

    #[test]
    fn other_commands() {
        assert!(matches!(
            parse(&["mcp"]).unwrap().command,
            Some(Command::Mcp)
        ));
        assert!(matches!(
            parse(&["serve"]).unwrap().command,
            Some(Command::Serve)
        ));
        assert!(parse(&["serve", "--port", "1"]).is_err());
        assert!(matches!(
            parse(&["prune", "npm"]).unwrap().command,
            Some(Command::Prune(_))
        ));
        assert!(parse(&["prune"]).is_err());
        assert!(matches!(
            parse(&["completions", "zsh"]).unwrap().command,
            Some(Command::Completions { .. })
        ));
        assert!(parse(&["completions", "tcsh"]).is_err());
    }

    #[test]
    fn every_command_has_help() {
        let mut cmd = Cli::command();
        cmd.build();
        for sub in cmd.get_subcommands() {
            assert!(sub.get_about().is_some(), "{} has no help", sub.get_name());
        }
    }
}
