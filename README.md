# Mulchie

Mulchie keeps a machine healthy while several AI coding agents work on it. With it you can see what uses CPU, memory, disk I/O and disk space by repo, worktree and agent session, queue heavy builds, find stale caches and idle worktrees, and reclaim space without losing work. You can set budgets for agents, connect agent hooks that warn about risky commands, and see when updates are out. Agents can look at the machine and act on it through an MCP server.

Mulchie is a supervisor. It is not a sandbox and not an antivirus: an agent that wants to get around it can.

It ships three ways: the `mulchie` command, a macOS menu bar app with a dashboard (build it from source for now; see Install), and an MCP server (`mulchie mcp`). The engine also serves a local HTTP API for scripts.

Full documentation is at [mulchie.com/docs](https://mulchie.com/docs/).

## What it does

**Monitor**
- Samples CPU, load, memory, swap, disk free and per-process disk I/O every 3 seconds, and keeps 30 minutes of history.
- Attributes each process to its git repo or worktree from its working directory, falling back to its parent chain.
- Groups processes the way you think of them: an app with all its helper processes, an agent workspace, or a single program. `mulchie top` shows the groups.
- Classifies processes as agents, builds or runtimes.
- Forecasts when the disk will run low from the recent free-space samples.

**Watch**
- Reports observations about processes: left over after their parent exited, runaway CPU, steadily growing memory, paused for hours, listening beyond this machine, running from a temporary folder, and similar. These are findings. They come from simple heuristics. They are not malware detection, and Mulchie is not an antivirus. See [findings](https://mulchie.com/docs/watch/).

**Supervise agents**
- A session is an agent and everything it started, found in the process table by name (Claude Code, Codex, Gemini CLI, Copilot CLI, Cursor's `cursor-agent`, aider, opencode and others; add more with `agents.extra`). `mulchie sessions` shows what each uses. See [sessions and limits](https://mulchie.com/docs/sessions/).
- Limits are budgets per session for CPU, memory, processes, writes and runtime. The action is `notify` unless you choose `deprioritize`, `pause` or `stop`. `pause` and `stop` spare the agent itself unless the budget sets `include_agent`. A session that only a hook registered is reported, never enforced, and only the engine that holds the lock acts. A pause ends after `resume_after_secs`.
- The memory guard watches the whole machine. When memory stays low it reports the biggest consumer it may close, and with `mulchie config set memory_guard.action stop` it closes it (SIGTERM, then SIGKILL), one at a time, until memory is back. By default that is what agent sessions started, never the agent itself; `memory_guard.scope all` extends it to your other apps and programs. Closing a process loses what it had not saved.
- Each session has an activity record in `<config dir>/sessions/`. Command lines in it are redacted by pattern, as well as Mulchie can. It is not tamper-proof.
- Hooks (`mulchie hooks install`) let Claude Code, Codex, Cursor, Gemini CLI and Copilot CLI tell Mulchie about session starts, prompts and shell commands. A hook fails open: with no engine, a slow one (300 ms) or any error, the action is allowed. Prompt text is never sent to the engine. See [hooks and guard rules](https://mulchie.com/docs/hooks/).
- Guard rules check the text of a shell command for risky shapes (deleting outside the workspace, `curl | sh`, `sudo`, force-pushing a protected branch, disabling Mulchie, reading credentials, writing startup files). Every rule defaults to `warn`. They are a tripwire, not a sandbox, and miss obfuscated commands.
- Update checks report newer versions of Mulchie and of installed agent tools, and outdated Homebrew packages. They never install anything. Vulnerability lookups against OSV.dev are off unless you turn them on. See [update checks](https://mulchie.com/docs/updates/).

**Optimize**
- Lowers the priority of build processes when CPU or memory is under pressure.
- Runs a machine-wide build queue. `mulchie run -- cargo build` waits for a free slot and runs with a fair job count. Agents use the same queue over MCP.
- Answers "is now a good time for heavy work?" with reasons (`mulchie advice`).

**Reclaim**
- Build caches: Rust `target/`, `.next`, optionally `node_modules`, and Xcode DerivedData on macOS. A cache is stale when it has not been used within the stale window and no process runs in its project.
- Git worktrees: size, idle days, uncommitted files and a verdict for each. Idle time comes from the last file change and the last commit, not from folder timestamps. A sweep removes idle agent worktrees.
- Package and toolchain stores: `mulchie prune` runs the tool's own prune command for npm, bun, pnpm, yarn, uv, pip, Homebrew, Go and iOS simulators. The disk breakdown also lists Docker and model stores, which Mulchie does not prune.
- Every deletion is a plan you see first. Mulchie then deletes exactly that plan, re-checking each item at the moment it acts.

**Act on processes**
- `mulchie signal` signals one process. `mulchie group` quits, pauses, resumes or lowers the priority of an app or workspace. Both show a plan and ask first. Protected processes are refused.

## Install

The first release is the CLI for macOS and Linux. With Homebrew:

```sh
brew install williamrogersdev/tap/mulchie-cli
```

`brew upgrade mulchie-cli` updates it. Without Homebrew, the install script:

```sh
curl -fsSL https://mulchie.com/install.sh | sh
```

or, from GitHub:

```sh
curl -fsSL https://raw.githubusercontent.com/williamrogersdev/mulchie/main/scripts/install.sh | sh
```

The install script puts the binary in `~/.local/bin` after checking its SHA-256 against the release's `.sha256` file. Every release asset has a SHA-256 checksum, and the release lists them in `SHA256SUMS`. From source: `cargo install --git https://github.com/williamrogersdev/mulchie mulchie`, or `cargo install --path crates/mulchie-cli` from a checkout. Mulchie is not on crates.io yet, and the menu bar app is not a download or a Homebrew cask yet: build it with `./scripts/bundle.sh`.

The macOS CLI binaries are ad-hoc signed, not Developer ID signed or notarized. Homebrew and the install script both work. A tarball downloaded with a browser is quarantined by Gatekeeper; after extracting it, run `xattr -d com.apple.quarantine ./mulchie`.

The menu bar app is not available as a download yet. Build it from source on a Mac (Rust and Bun required) with `./scripts/bundle.sh`; a signed download is planned.

Then check the setup:

```sh
mulchie doctor
```

If you built the app, it starts the engine. Without the app, run it yourself with `mulchie serve`, or at login with `mulchie service install`.

## CLI

Run `mulchie --help` or `mulchie <command> --help` for every flag.

| Command | What it does |
| --- | --- |
| `status` | Disk, memory, CPU and what each agent workspace uses. Same as running `mulchie` alone. |
| `diagnose` | One prioritised picture of the machine, with recommendations. Changes nothing. |
| `top` | What uses CPU and memory, by app and workspace. `--processes` lists single processes. |
| `inspect <pid>` | Everything known about one process. |
| `findings` | What the watch noticed. `findings dismiss <id>` hides one; `undismiss` shows it again. |
| `sessions` | Agent sessions and what each uses. `sessions <id> --events` shows its activity record; `sessions stop <id>` plans stopping it. Needs the engine. |
| `limits` | Session budgets: who is over, what Mulchie paused, and whether limits act. Needs the engine. |
| `updates` | The last update check. `updates --check` asks for one now. Needs the engine. |
| `guard check -- <command>` | Ask the guard rules about a command without running it. Needs the engine. |
| `hooks install\|uninstall\|status` | Connect Mulchie to an agent's own hooks. |
| `hook <client> <event>` | Run by an agent's hook system, not by hand. Fails open. |
| `slots` | List build slots. `slots release <ticket>` ends one whose holder is gone. Needs the engine. |
| `worktrees` | Every git worktree: size, idle days, dirty files, verdict. |
| `scan` | Build caches and which are stale. |
| `clean` | Delete build caches: plan, confirm, delete exactly that plan. |
| `sweep` | Remove idle agent worktrees: plan, confirm, remove exactly that plan. |
| `prune <cache>` | Run a package manager's own prune command. |
| `signal <pid> <signal>` | Signal one of your processes: plan, confirm, signal. Needs the engine. |
| `group <key> <action>` | Act on an app, workspace, program or agent session as a whole. Needs the engine. |
| `advice` | Is it OK to start heavy work right now? |
| `run -- <command>` | Wait for a build slot, run the command, release the slot. |
| `mcp` | MCP server on stdio. `mcp install` connects it to an agent app. |
| `token` | Where the API tokens are. |
| `serve` | Run the engine without the app. |
| `config` | Show settings. `config set <key> <value>` changes one. |
| `doctor` | Check the install end to end. |
| `service` | Install, remove or inspect the background service. |
| `completions <shell>` | Print a shell completion script. |

Most commands use the running engine. When none is running, read commands, `clean` and `sweep` work locally and say so on stderr. The commands marked as needing the engine say how to start one.

## MCP

Connect Mulchie to an agent app:

```sh
mulchie mcp install --client claude-code
```

Other clients are `claude-desktop`, `cursor` and `codex`. Add `--read-only` to leave out every tool that deletes, signals, prunes or dismisses, and `check_updates`. Without `--client`, the command lists the apps it found and changes nothing.

Agents start with the `diagnose` tool. Changes are always two calls: a plan tool, then `execute_plan` with items from that plan. Other tools show sessions (`list_sessions`, `get_session`), limits (`limits_status`), updates (`list_updates`, `check_updates`) and ask the guard rules about a command (`check_command`). See [MCP](https://mulchie.com/docs/mcp/) for the tool list.

## Agent hooks

```sh
mulchie hooks install --client claude-code --dry-run
mulchie hooks install --client claude-code
```

Supported clients are `claude-code`, `codex`, `cursor`, `gemini` and `copilot`. The install edits one file (`~/.claude/settings.json`, `~/.codex/hooks.json`, `~/.cursor/hooks.json`, `~/.gemini/settings.json` or `~/.copilot/hooks/mulchie.json`), keeps your other hooks, and first copies the old file to `<file>.mulchie-backup-<unix time>`. Codex runs new hooks only after you trust them with its `/hooks` command. Windsurf, Antigravity, opencode and Aider have no hook support.

## HTTP API

The engine listens on `127.0.0.1:47474` by default. Every route except `GET /v1/health` needs `Authorization: Bearer <token>`. The token path is printed by `mulchie token`. A second, read-only token (`mulchie token --read-only`) works for `GET` routes and plan routes only. The read-only token also works for `POST /v1/hook` and `POST /v1/guard/check`. The routes are listed in [docs/openapi.yaml](docs/openapi.yaml).

## Configuration

`~/.config/mulchie/config.json` (or under `$XDG_CONFIG_HOME/mulchie`). Print it with `mulchie config` and change one setting with `mulchie config set <key> <value>`. The app has the same settings under **Settings**. `MULCHIE_PORT` overrides the API port for one process. Sections: `watch`, `agents`, `sessions`, `limits`, `memory_guard`, `guard` and `updates`; see [configuration](https://mulchie.com/docs/configuration/).

The engine contacts the network only for update checks: the GitHub releases API for Mulchie, and the npm registry for installed agent tools. `mulchie config set updates.check off` stops it. A build without the default `network` cargo feature (`--no-default-features`) cannot make requests at all.

## Layout

- `crates/mulchie-core`: engine, monitor, scanners, safety gate, sessions, limits, guard rules, update checks, HTTP API
- `crates/mulchie-cli`: the `mulchie` binary, MCP server and agent hooks
- `crates/mulchie-app`: menu bar app and dashboard
- `docs/ARCHITECTURE.md`: how the pieces fit and the rules the code keeps

See [CONTRIBUTING.md](CONTRIBUTING.md) to build and test.

## License

MIT
