# Mulchie

Mulchie keeps a machine healthy while several AI coding agents work on it. With it you can see what uses CPU, memory, disk I/O and disk space by repo and worktree, queue heavy builds, find stale caches and idle worktrees, and reclaim space without losing work. Agents can do the same through an MCP server.

It ships three ways: the `mulchie` command, a macOS menu bar app with a dashboard, and an MCP server (`mulchie mcp`). The engine also serves a local HTTP API for scripts.

Full documentation is at [mulchie.com/docs](https://mulchie.com/docs/).

## What it does

**Monitor**
- Samples CPU, load, memory, swap, disk free and per-process disk I/O every 3 seconds, and keeps 30 minutes of history.
- Attributes each process to its git repo or worktree from its working directory, falling back to its parent chain.
- Groups processes the way you think of them: an app with all its helper processes, an agent workspace, or a single program. `mulchie top` shows the groups.
- Classifies processes as agents, builds or runtimes.
- Forecasts when the disk will run low from the recent free-space samples.

**Watch**
- Reports observations about processes: left over after their parent exited, runaway CPU, steadily growing memory, paused for hours, listening beyond this machine, running from a temporary folder, and similar. These are findings. They come from simple heuristics. They are not malware detection, and Mulchie is not an antivirus. See [findings](https://mulchie.com/docs/findings/).

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

macOS app (includes the CLI):

```sh
brew install --cask williamrogersdev/tap/mulchie
```

CLI only:

```sh
brew install williamrogersdev/tap/mulchie-cli
curl -fsSL https://mulchie.com/install.sh | sh
cargo install mulchie
```

The install script puts the binary in `~/.local/bin` after checking its SHA-256. From a source checkout, `./scripts/bundle.sh` builds the app (Rust and Bun required) and `cargo install --path crates/mulchie-cli` installs the CLI.

Then check the setup:

```sh
mulchie doctor
```

The app starts the engine. Without the app, run it yourself with `mulchie serve`, or at login with `mulchie service install`.

## CLI

Run `mulchie --help` or `mulchie <command> --help` for every flag.

| Command | What it does |
| --- | --- |
| `status` | Disk, memory, CPU and what each agent workspace uses. Same as running `mulchie` alone. |
| `diagnose` | One prioritised picture of the machine, with recommendations. Changes nothing. |
| `top` | What uses CPU and memory, by app and workspace. `--processes` lists single processes. |
| `inspect <pid>` | Everything known about one process. |
| `findings` | What the watch noticed. `findings dismiss <id>` hides one; `undismiss` shows it again. |
| `worktrees` | Every git worktree: size, idle days, dirty files, verdict. |
| `scan` | Build caches and which are stale. |
| `clean` | Delete build caches: plan, confirm, delete exactly that plan. |
| `sweep` | Remove idle agent worktrees: plan, confirm, remove exactly that plan. |
| `prune <cache>` | Run a package manager's own prune command. |
| `signal <pid> <signal>` | Signal one of your processes: plan, confirm, signal. Needs the engine. |
| `group <key> <action>` | Act on an app, workspace or program as a whole. Needs the engine. |
| `advice` | Is it OK to start heavy work right now? |
| `run -- <command>` | Wait for a build slot, run the command, release the slot. |
| `mcp` | MCP server on stdio. `mcp install` connects it to an agent app. |
| `token` | Where the API tokens are. |
| `serve` | Run the engine without the app. |
| `config` | Show settings. `config set <key> <value>` changes one. |
| `doctor` | Check the install end to end. |
| `service` | Install, remove or inspect the background service. |
| `completions <shell>` | Print a shell completion script. |

Most commands use the running engine. When none is running, read commands, `clean` and `sweep` work locally and say so on stderr.

## MCP

Connect Mulchie to an agent app:

```sh
mulchie mcp install --client claude-code
```

Other clients are `claude-desktop`, `cursor` and `codex`. Add `--read-only` to leave out every tool that deletes, signals or prunes. Without `--client`, the command lists the apps it found and changes nothing.

Agents start with the `diagnose` tool. Changes are always two calls: a plan tool, then `execute_plan` with items from that plan. See [MCP](https://mulchie.com/docs/mcp/) for the tool list.

## HTTP API

The engine listens on `127.0.0.1:47474` by default. Every route except `GET /v1/health` needs `Authorization: Bearer <token>`. The token path is printed by `mulchie token`. A second, read-only token (`mulchie token --read-only`) works for `GET` routes and plan routes only. The routes are listed in [docs/openapi.yaml](docs/openapi.yaml).

## Configuration

`~/.config/mulchie/config.json` (or under `$XDG_CONFIG_HOME/mulchie`). Print it with `mulchie config` and change one setting with `mulchie config set <key> <value>`. The app has the same settings under **Settings**. `MULCHIE_PORT` overrides the API port for one process.

## Layout

- `crates/mulchie-core`: engine, monitor, scanners, safety gate, HTTP API
- `crates/mulchie-cli`: the `mulchie` binary and MCP server
- `crates/mulchie-app`: menu bar app and dashboard
- `docs/ARCHITECTURE.md`: how the pieces fit and the rules the code keeps

See [CONTRIBUTING.md](CONTRIBUTING.md) to build and test.

## License

MIT
