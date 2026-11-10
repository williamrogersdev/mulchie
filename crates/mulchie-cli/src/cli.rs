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

    /// What is using CPU and memory, by app and workspace (--processes for single processes)
    #[command(
        long_about = "What is using CPU and memory, grouped the way you think of it: apps \
(with all their helper processes), agent workspaces, and other programs. Each group shows its \
key, which `mulchie group` takes. With --processes, single processes by CPU, memory or disk \
I/O, with workspace and command (secrets in command lines are redacted).",
        after_help = "Examples:\n  mulchie top\n  mulchie top --processes --sort mem -n 10\n  mulchie top --json"
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

    /// Signal one of your processes: show the plan, confirm, then signal (needs the engine)
    #[command(
        long_about = "Send a signal to one of your processes, and with --tree to its \
descendants too (deepest first). The engine plans it first: which processes would be \
signalled, and why any is refused (protected processes, other users' processes, Mulchie \
itself, and this terminal and its parents are never signalled). After you confirm, each \
process is re-checked by its start time, so a pid reused since the plan is skipped.",
        after_help = "Exit status: 0 done (or --dry-run, or declined); 1 an item was skipped or a call failed; \
2 confirmation was needed but stdin is not a terminal; 3 the plan is refused or nothing in it is ready.\n\n\
Examples:\n  mulchie signal 4242 term --dry-run\n  mulchie signal 4242 term\n  mulchie signal 4242 stop --yes\n  mulchie signal 4242 term --tree"
    )]
    Signal(SignalArgs),

    /// Act on an app, workspace or program as a whole: plan, confirm, act (needs the engine)
    #[command(
        long_about = "Act on a consumer group from `mulchie top` or `mulchie diagnose`: an app \
with its helpers (app:…), an agent workspace (workspace:…), a program (process:…), or an agent \
session (session:<id> from `mulchie sessions`: its processes, and the agent itself only with \
--include-agent). Actions: \
quit, force-quit, pause, resume, lower-priority, stop-builds, and restart where the plan \
offers it. The engine plans it first and says what happens to each process; protected \
processes and the group holding this terminal are refused. After you confirm, each process \
is re-checked by its start time.",
        after_help = "Exit status: 0 done (or --dry-run, or declined); 1 an item was skipped or a call failed; \
2 confirmation was needed but stdin is not a terminal; 3 the plan is refused or nothing in it is ready.\n\n\
Examples:\n  mulchie group 'app:/Applications/Slack.app' quit --dry-run\n  mulchie group session:s-1700000000-4242 pause\n  mulchie group 'workspace:/Users/me/dev/app' pause\n  mulchie group 'workspace:/Users/me/dev/app' lower-priority --yes"
    )]
    Group(GroupArgs),

    /// What Mulchie's watch noticed about processes and its own setup
    #[command(
        long_about = "Findings from Mulchie's watch: leftover, runaway, stopped, exposed or \
oddly placed processes, and problems with Mulchie's own setup. They are observations from \
simple heuristics, not verdicts: none of them means malware was found. Each says why it was \
raised and what to look at next. Dismiss one you know is expected.",
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  mulchie findings\n  mulchie findings --severity medium\n  mulchie findings --kind runaway_cpu --json\n  mulchie findings dismiss 'runaway_cpu:4242:1700000000' --days 7\n  mulchie findings undismiss 'runaway_cpu:4242:1700000000'"
    )]
    Findings(FindingsArgs),

    /// Agent sessions: what each agent and everything it started is using (needs the engine)
    #[command(
        long_about = "A session is an agent process and everything it started, found from the \
process table or reported by `mulchie hook`. With no id: every session, running first, with \
CPU, memory, processes and runtime. With an id: its processes, budget breaches and what \
Mulchie paused; with --events, its activity record (command lines redacted). \
`mulchie sessions stop <id>` plans stopping its processes, shows the plan and asks.",
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  mulchie sessions\n  mulchie sessions s-1700000000-4242\n  mulchie sessions s-1700000000-4242 --events --limit 50\n  mulchie sessions stop s-1700000000-4242 --dry-run"
    )]
    Sessions(SessionsArgs),

    /// Session budgets: who is over, what Mulchie paused, and whether it acts (needs the engine)
    #[command(
        long_about = "Whether session budgets only report or act, the budgets themselves \
(limits in `mulchie config`), sessions over a cap and what was done about it, sessions \
hogging the machine, and processes Mulchie paused.",
        after_help = "Examples:\n  mulchie limits\n  mulchie limits --json"
    )]
    Limits(JsonArg),

    /// Updates for Mulchie, agent tools and Homebrew, and advisories (needs the engine)
    #[command(
        long_about = "The engine's last update check and what it found. --check asks the \
engine to check now (it runs in the background; run `mulchie updates` again for the \
result). Checks are off when updates.check is off in `mulchie config`.",
        after_help = "Examples:\n  mulchie updates\n  mulchie updates --check\n  mulchie updates --json"
    )]
    Updates(UpdatesArgs),

    /// Ask the guard rules about a command, without running it (needs the engine)
    #[command(subcommand)]
    Guard(GuardCommand),

    /// List the build slots, or release one whose holder is gone (needs the engine)
    #[command(
        long_about = "List the build slots the engine has handed out: ticket, label, holder \
pid, age and expiry. `mulchie slots release <ticket>` ends one, for a holder that crashed or \
an agent session that ended without releasing it. It uses this user's full API token, so no \
release secret is needed; it asks first unless --yes is given.",
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  mulchie slots\n  mulchie slots --json\n  mulchie slots release 17\n  mulchie slots release 17 --yes"
    )]
    Slots(SlotsArgs),

    /// Everything known about one process (secrets in its command line redacted)
    #[command(after_help = "Examples:\n  mulchie inspect 4242\n  mulchie inspect 4242 --json")]
    Inspect(InspectArgs),

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

    /// One prioritised picture of the machine, with recommendations
    #[command(
        long_about = "Health and why, what is using CPU, memory and disk by workspace, the build \
queue, reclaimable space (stale build caches, idle agent worktrees, package caches) with sizes, \
engine problems, and recommendations with their reason, expected gain, risk and the next \
command. Changes nothing. Uses the running engine, or a local look when none runs (said on \
stderr). The same picture as the MCP diagnose tool.",
        after_help = "Examples:\n  mulchie diagnose\n  mulchie diagnose --json"
    )]
    Diagnose(JsonArg),

    /// MCP server on stdio for agents, and connecting it to agent apps
    #[command(
        long_about = "With no subcommand, run an MCP server on stdin/stdout (JSON-RPC, one \
message per line). Agents start with its diagnose tool; deleting is always two calls, a plan \
and then execute_plan with items from that plan, re-checked by the same safety gate as the \
CLI. --read-only (or MULCHIE_MCP_READ_ONLY=1) leaves out every tool that deletes, signals or \
prunes.\n\nUse `mulchie mcp install` to connect it to Claude Code, Claude Desktop, Cursor \
or Codex in one step.",
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  mulchie mcp install                      # list agent apps found\n  mulchie mcp install --client claude-code\n  mulchie mcp install --client cursor --read-only --dry-run\n  mulchie mcp config                       # JSON to paste by hand\n  mulchie mcp --read-only                  # serve on stdio"
    )]
    Mcp(McpArgs),

    /// Where the API tokens are; prints a token only with --show
    #[command(
        long_about = "Print the path of the local API token. With --read-only, the read-only \
token, which the API accepts only for GET routes and plan routes. The token itself is printed \
only with --show. Nothing is created: the engine writes both tokens when it starts.",
        after_help = "Examples:\n  mulchie token\n  mulchie token --read-only\n  mulchie token --read-only --show"
    )]
    Token(TokenArgs),

    /// Run the engine (monitor, optimizer, build queue, local API) without the app
    #[command(
        long_about = "Run the engine in the foreground: the monitor, the optimizer, the build \
queue and the local HTTP API on 127.0.0.1. Only one engine runs per user; if the app or \
another `mulchie serve` already runs, this exits with status 1. Stops cleanly on SIGINT \
or SIGTERM. The port is `api_port` from the config, or MULCHIE_PORT for this process.",
        after_help = "Examples:\n  mulchie serve\n  MULCHIE_PORT=47575 mulchie serve\n  mulchie service install   # run it at login instead"
    )]
    Serve,

    /// Show or change the settings (through the engine when it runs, so changes apply at once)
    #[command(
        long_about = "Print the config path, any config error, and the current settings. \
`mulchie config set <key> <value>` changes one setting (a dotted key, e.g. \
`worktree_idle_days` or `guard.rules.sudo`). The value is JSON: a number (21), true or \
false, a string ('\"block\"', or just block: a value that is not JSON is taken as a \
string), or a list ('[\"/Users/me/dev\"]'). When \
an engine runs, both go through it, so a change takes effect without a restart; otherwise \
the file is read or edited directly, and Mulchie says so.",
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  mulchie config\n  mulchie config --json\n  mulchie config set worktree_idle_days 21\n  mulchie config set guard.rules.sudo '\"block\"'\n  mulchie config set scan_roots '[\"/Users/me/dev\", \"/Users/me/src\"]'\n  mulchie config set watch.enabled false"
    )]
    Config(ConfigArgs),

    /// Check the install end to end; exits 1 if any check fails
    #[command(
        long_about = "Check the install end to end and print pass, warn or fail per check: the \
config file, config directory and token permissions, the running engine (lock holder, \
port, health, sample age), git, which `mulchie` is on PATH, the background service, and \
how to add the MCP server. Nothing is changed. Exits 1 if any check fails.",
        after_help = "Examples:\n  mulchie doctor\n  mulchie doctor --json"
    )]
    Doctor(JsonArg),

    /// Connect Mulchie to an agent's own hooks (session start, prompts, shell commands)
    #[command(
        subcommand,
        long_about = "Add `mulchie hook` to an agent app's user-level hook configuration, so the \
agent tells Mulchie when a session starts, before and after each shell command, and when it \
stops. Mulchie answers with the machine's state, advice, limit breaches and guard rules \
(risky commands: warn or block). Other hooks in the file are kept; the old file is backed \
up; uninstall removes exactly the entries install added. Supported: claude-code, codex, \
cursor, gemini, copilot."
    )]
    Hooks(HooksCommand),

    /// Run by an agent's hook system: read its event on stdin and answer (fails open)
    #[command(
        long_about = "The command an agent's hook system runs; `mulchie hooks install` sets it \
up. Reads the client's JSON event on stdin, tells the engine (with the read-only token), \
and prints the client's own answer format. It fails open: with no engine, a slow engine \
(300 ms budget), bad input or any error it allows the action, exits 0, and notes the \
problem in <config dir>/hook.log at most once a minute. EVENT is session_start, prompt, \
pre_tool, post_tool, stop or session_end (the client's own event names work too).",
        after_help = "Examples:\n  echo '{\"session_id\":\"s\",\"cwd\":\"/tmp\"}' | mulchie hook claude-code session_start"
    )]
    Hook(HookArgs),

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

#[derive(Args, Debug, Clone, Default)]
pub struct McpArgs {
    /// Offer only tools that change nothing (plus build slots)
    #[arg(long)]
    pub read_only: bool,
    #[command(subcommand)]
    pub action: Option<McpCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum McpClient {
    /// Claude Code, through `claude mcp add --scope user`
    ClaudeCode,
    /// Claude Desktop's claude_desktop_config.json
    ClaudeDesktop,
    /// Cursor's ~/.cursor/mcp.json
    Cursor,
    /// Codex, through `codex mcp add` or ~/.codex/config.toml
    Codex,
}

#[derive(Args, Debug, Clone, Default)]
pub struct McpInstallArgs {
    /// The agent app; without it, list the apps found and what would be done
    #[arg(long, value_enum)]
    pub client: Option<McpClient>,
    /// Register the read-only server (no deleting, signalling or pruning)
    #[arg(long)]
    pub read_only: bool,
    /// Print the change, change nothing
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Replace an existing, different mulchie entry
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct McpUninstallArgs {
    /// The agent app
    #[arg(long, value_enum)]
    pub client: Option<McpClient>,
    /// Print the change, change nothing
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum McpCommand {
    /// Add Mulchie's MCP server to an agent app
    #[command(
        long_about = "Add Mulchie's MCP server to an agent app's user-level config. Claude Code \
and Codex are set up through their own CLI (`claude mcp add`, `codex mcp add`); Claude \
Desktop, Cursor and (without its CLI) Codex by editing their config file: other entries are \
kept, the old file is backed up next to it, the new one is written atomically, and a file \
that cannot be parsed is left alone. Without --client, lists the apps found and what would \
be done. Restart the app afterwards.",
        after_help = "Examples:\n  mulchie mcp install\n  mulchie mcp install --client claude-code\n  mulchie mcp install --client claude-desktop --read-only --dry-run"
    )]
    Install(McpInstallArgs),
    /// Remove Mulchie's MCP server from an agent app
    #[command(
        after_help = "Examples:\n  mulchie mcp uninstall --client cursor --dry-run\n  mulchie mcp uninstall --client claude-code"
    )]
    Uninstall(McpUninstallArgs),
    /// Print the JSON config snippet to paste into any MCP client
    #[command(after_help = "Examples:\n  mulchie mcp config\n  mulchie mcp config --read-only")]
    Config {
        /// The read-only server
        #[arg(long)]
        read_only: bool,
    },
}

#[derive(Args, Debug, Clone, Copy, Default)]
pub struct TokenArgs {
    /// The read-only token (GET and plan routes only)
    #[arg(long)]
    pub read_only: bool,
    /// Print the token itself, not its path
    #[arg(long)]
    pub show: bool,
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

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookClient {
    /// Claude Code: ~/.claude/settings.json
    ClaudeCode,
    /// Codex: ~/.codex/hooks.json
    Codex,
    /// Cursor: ~/.cursor/hooks.json
    Cursor,
    /// Gemini CLI: ~/.gemini/settings.json
    Gemini,
    /// GitHub Copilot CLI: ~/.copilot/hooks/mulchie.json
    Copilot,
}

impl HookClient {
    pub fn name(self) -> &'static str {
        match self {
            HookClient::ClaudeCode => "claude-code",
            HookClient::Codex => "codex",
            HookClient::Cursor => "cursor",
            HookClient::Gemini => "gemini",
            HookClient::Copilot => "copilot",
        }
    }
}

/// How far guard rules may go in one client.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// Never ask or block; guard warnings become advice
    Off,
    /// At most ask the user (blocks become asks where the client can ask)
    Warn,
    /// Follow the engine, including blocks
    Block,
}

impl GuardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardMode::Off => "off",
            GuardMode::Warn => "warn",
            GuardMode::Block => "block",
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum HooksCommand {
    /// Add Mulchie's hooks to an agent app
    #[command(
        after_help = "Examples:\n  mulchie hooks install --client claude-code --dry-run\n  mulchie hooks install --client codex\n  mulchie hooks install --client cursor --guard block"
    )]
    Install {
        /// The agent app
        #[arg(long, value_enum)]
        client: HookClient,
        /// Print the exact change, change nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// How far guard rules may go in this app (default: as the engine's config says)
        #[arg(long, value_enum)]
        guard: Option<GuardMode>,
    },
    /// Remove exactly the hooks Mulchie added
    #[command(after_help = "Examples:\n  mulchie hooks uninstall --client cursor --dry-run")]
    Uninstall {
        /// The agent app
        #[arg(long, value_enum)]
        client: HookClient,
        /// Print the exact change, change nothing
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Which apps have Mulchie's hooks, and for which events
    #[command(after_help = "Examples:\n  mulchie hooks status\n  mulchie hooks status --json")]
    Status {
        #[command(flatten)]
        out: JsonArg,
    },
}

#[derive(Args, Debug)]
pub struct HookArgs {
    /// The agent app that runs this hook
    #[arg(value_enum)]
    pub client: HookClient,
    /// The event
    pub event: String,
    /// How far guard rules may go in this app
    #[arg(long, value_enum)]
    pub guard: Option<GuardMode>,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(flatten)]
    pub out: JsonArg,
    #[command(subcommand)]
    pub action: Option<ConfigCommand>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Change one setting
    Set {
        /// A dotted key, e.g. worktree_idle_days or guard.rules.sudo
        key: String,
        /// The value, as JSON: 21, true, '"block"' (or a bare word), '["/a", "/b"]'
        value: String,
        #[command(flatten)]
        out: JsonArg,
    },
}

#[derive(Args, Debug)]
pub struct FindingsArgs {
    /// Only findings at least this severe
    #[arg(long, value_enum)]
    pub severity: Option<Severity>,
    /// Only this kind, e.g. runaway_cpu
    #[arg(long)]
    pub kind: Option<String>,
    /// Include dismissed findings
    #[arg(long)]
    pub all: bool,
    #[command(flatten)]
    pub out: JsonArg,
    #[command(subcommand)]
    pub action: Option<FindingsCommand>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum FindingsCommand {
    /// Hide a finding you know is expected
    Dismiss {
        /// The finding id
        id: String,
        /// For this many days (default 30)
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3650))]
        days: Option<u32>,
        /// Dismiss it even if no current or dismissed finding has this id
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        out: JsonArg,
    },
    /// Show a dismissed finding again
    Undismiss {
        /// The finding id
        id: String,
        #[command(flatten)]
        out: JsonArg,
    },
}

#[derive(Args, Debug)]
pub struct SessionsArgs {
    /// A session id from the list, e.g. s-1700000000-4242
    pub id: Option<String>,
    /// The session's activity record instead of its detail
    #[arg(long, requires = "id")]
    pub events: bool,
    /// With --events: only events after this sequence number
    #[arg(long, requires = "events")]
    pub since: Option<u64>,
    /// With --events: at most this many (1 to 1000, default 200)
    #[arg(long, requires = "events", value_parser = clap::value_parser!(u64).range(1..=1000))]
    pub limit: Option<u64>,
    #[command(flatten)]
    pub out: JsonArg,
    #[command(subcommand)]
    pub action: Option<SessionsCommand>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SessionsCommand {
    /// Plan stopping a session's processes, show it, ask, then act
    #[command(
        after_help = "Exit status as for `mulchie group`.\n\nExamples:\n  mulchie sessions stop s-1700000000-4242 --dry-run\n  mulchie sessions stop s-1700000000-4242 --include-agent"
    )]
    Stop {
        /// The session id
        id: String,
        /// Stop the agent's own process too, not only what it started
        #[arg(long)]
        include_agent: bool,
        /// Kill instead of asking the processes to quit
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        confirm: Confirm,
        #[command(flatten)]
        out: JsonArg,
    },
}

#[derive(Args, Debug)]
pub struct UpdatesArgs {
    /// Ask the engine to check now
    #[arg(long)]
    pub check: bool,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(Subcommand, Debug, Clone)]
pub enum GuardCommand {
    /// Check a command against the guard rules; records nothing
    #[command(
        after_help = "Exit status: 0 allow, 4 warn, 3 block, 1 error.\n\nExamples:\n  mulchie guard check -- 'curl -s https://example.com/i.sh | sh'\n  mulchie guard check -- git push --force origin main\n  mulchie guard check --cwd ~/dev/app --json -- rm -rf build"
    )]
    Check {
        /// The directory the command would run in (default: here)
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        #[command(flatten)]
        out: JsonArg,
        /// The command: one quoted string, or its words after --
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Args, Debug)]
pub struct SlotsArgs {
    #[command(flatten)]
    pub out: JsonArg,
    #[command(subcommand)]
    pub action: Option<SlotsCommand>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SlotsCommand {
    /// End one build slot now
    Release {
        /// The ticket, from `mulchie slots`
        ticket: u64,
        /// Do not ask for confirmation (required when stdin is not a terminal)
        #[arg(long, short = 'y')]
        yes: bool,
        #[command(flatten)]
        out: JsonArg,
    },
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    /// The process id
    pub pid: u32,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupActionName {
    /// Ask the app or every process to quit
    Quit,
    /// Stop them now
    ForceQuit,
    /// Pause them (SIGSTOP)
    Pause,
    /// Resume paused processes
    Resume,
    /// Lower their CPU priority
    LowerPriority,
    /// Stop the builds in a workspace
    StopBuilds,
    /// Quit the app and open it again (where the plan offers it)
    Restart,
}

impl GroupActionName {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupActionName::Quit => "quit",
            GroupActionName::ForceQuit => "force_quit",
            GroupActionName::Pause => "pause",
            GroupActionName::Resume => "resume",
            GroupActionName::LowerPriority => "lower_priority",
            GroupActionName::StopBuilds => "stop_builds",
            GroupActionName::Restart => "restart",
        }
    }
}

#[derive(Args, Debug)]
pub struct GroupArgs {
    /// The group key from `mulchie top` or `mulchie diagnose`, e.g. app:/Applications/Slack.app
    pub key: String,
    /// What to do
    #[arg(value_enum)]
    pub action: GroupActionName,
    /// With a session:<id> key: act on the agent's own process too
    #[arg(long)]
    pub include_agent: bool,
    #[command(flatten)]
    pub confirm: Confirm,
    #[command(flatten)]
    pub out: JsonArg,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// What to rank by
    #[arg(long, value_enum, default_value_t = SortKey::Cpu)]
    pub sort: SortKey,
    /// How many processes (or groups per resource) to show
    #[arg(short = 'n', long = "limit", default_value_t = 25)]
    pub limit: usize,
    /// Single processes instead of apps and workspaces
    #[arg(long, short = 'p')]
    pub processes: bool,
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
    /// Its descendants too, deepest first
    #[arg(long)]
    pub tree: bool,
    #[command(flatten)]
    pub confirm: Confirm,
    #[command(flatten)]
    pub out: JsonArg,
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
        let Some(Command::Signal(s)) =
            parse(&["signal", "42", "term", "--tree", "--dry-run", "--json"])
                .unwrap()
                .command
        else {
            panic!()
        };
        assert!(s.tree && s.confirm.dry_run && s.out.json);
        assert!(parse(&["signal", "42", "term", "--yes", "--dry-run"]).is_err());
    }

    #[test]
    fn hooks_args() {
        let Some(Command::Hooks(HooksCommand::Install {
            client,
            dry_run,
            guard,
        })) = parse(&[
            "hooks",
            "install",
            "--client",
            "claude-code",
            "-n",
            "--guard",
            "block",
        ])
        .unwrap()
        .command
        else {
            panic!()
        };
        assert_eq!(
            (client, dry_run, guard),
            (HookClient::ClaudeCode, true, Some(GuardMode::Block))
        );
        assert!(parse(&["hooks", "install"]).is_err());
        assert!(parse(&["hooks", "install", "--client", "aider"]).is_err());
        assert!(parse(&["hooks", "status", "--json"]).is_ok());
        let Some(Command::Hook(h)) = parse(&["hook", "copilot", "pre_tool", "--guard", "warn"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(
            (h.client, h.event.as_str(), h.guard),
            (HookClient::Copilot, "pre_tool", Some(GuardMode::Warn))
        );
    }

    #[test]
    fn group_findings_inspect_config() {
        let Some(Command::Group(g)) = parse(&["group", "app:/A.app", "lower-priority", "-y"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(
            (g.key.as_str(), g.action.as_str()),
            ("app:/A.app", "lower_priority")
        );
        assert!(g.confirm.yes);
        assert!(parse(&["group", "app:/A.app", "explode"]).is_err());
        assert!(parse(&["group", "app:/A.app"]).is_err());

        let Some(Command::Findings(f)) = parse(&[
            "findings",
            "--severity",
            "medium",
            "--kind",
            "runaway_cpu",
            "--all",
            "--json",
        ])
        .unwrap()
        .command
        else {
            panic!()
        };
        assert_eq!(f.severity, Some(Severity::Medium));
        assert!(f.all && f.out.json && f.action.is_none());
        let Some(Command::Findings(f)) = parse(&["findings", "dismiss", "x:1", "--days", "7"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(matches!(
            f.action,
            Some(FindingsCommand::Dismiss { days: Some(7), .. })
        ));
        assert!(parse(&["findings", "dismiss", "x", "--days", "0"]).is_err());
        assert!(parse(&["findings", "undismiss", "x"]).is_ok());
        assert!(parse(&["findings", "--severity", "dire"]).is_err());

        let Some(Command::Inspect(i)) = parse(&["inspect", "42", "--json"]).unwrap().command else {
            panic!()
        };
        assert_eq!(i.pid, 42);
        assert!(parse(&["inspect"]).is_err());

        let Some(Command::Config(c)) = parse(&["config", "set", "worktree_idle_days", "21"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(
            matches!(c.action, Some(ConfigCommand::Set { ref key, ref value, .. }) if key == "worktree_idle_days" && value == "21")
        );
        assert!(parse(&["config", "set", "x"]).is_err());

        let Some(Command::Top(t)) = parse(&["top", "--processes"]).unwrap().command else {
            panic!()
        };
        assert!(t.processes);
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
            Some(Command::Mcp(McpArgs {
                read_only: false,
                action: None
            }))
        ));
        assert!(matches!(
            parse(&["mcp", "--read-only"]).unwrap().command,
            Some(Command::Mcp(McpArgs {
                read_only: true,
                action: None
            }))
        ));
        match parse(&[
            "mcp",
            "install",
            "--client",
            "claude-desktop",
            "--read-only",
            "-n",
        ])
        .unwrap()
        .command
        {
            Some(Command::Mcp(McpArgs {
                action: Some(McpCommand::Install(a)),
                ..
            })) => {
                assert_eq!(a.client, Some(McpClient::ClaudeDesktop));
                assert!(a.read_only && a.dry_run && !a.force);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["mcp", "install", "--client", "vim"]).is_err());
        assert!(parse(&["mcp", "--read-only", "install"]).is_err());
        assert!(matches!(
            parse(&["mcp", "uninstall", "--client", "codex"])
                .unwrap()
                .command,
            Some(Command::Mcp(McpArgs {
                action: Some(McpCommand::Uninstall(_)),
                ..
            }))
        ));
        assert!(matches!(
            parse(&["mcp", "config", "--read-only"]).unwrap().command,
            Some(Command::Mcp(McpArgs {
                action: Some(McpCommand::Config { read_only: true }),
                ..
            }))
        ));
        assert!(matches!(
            parse(&["diagnose", "--json"]).unwrap().command,
            Some(Command::Diagnose(JsonArg { json: true }))
        ));
        assert!(matches!(
            parse(&["token", "--read-only", "--show"]).unwrap().command,
            Some(Command::Token(TokenArgs {
                read_only: true,
                show: true
            }))
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
