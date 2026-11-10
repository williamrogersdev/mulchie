---
title: CLI reference
description: "Every mulchie command and flag: status, diagnose, top, findings, sessions, limits, updates, guard, hooks, clean, sweep, signal, group, run, mcp and config."
---

Use this page to look up a `mulchie` command. Every command also has `--help` with examples.

```sh
mulchie <command> [options]
```

Running `mulchie` with no command is the same as `mulchie status`. `mulchie --version` prints the version.

## The engine

Most commands talk to the running engine: the menu bar app, `mulchie serve`, or the service from `mulchie service install`. Only one engine runs per user.

When no engine is running:

- Read commands (`status`, `diagnose`, `top`, `inspect`, `findings`, `worktrees`, `scan`, `advice`, `config`) look at the machine locally, once, and say so on stderr.
- `clean` and `sweep` plan locally. Each item is still checked again before it is touched.
- `run` has no queue to join. It says so and runs the command straight away.
- `signal`, `group`, `sessions`, `limits`, `updates`, `guard` and `slots` need the engine and tell you how to start one.
- `hook` fails open: it allows the action and exits 0 (see [Hooks](/docs/hooks/)). `hooks install` edits a file and needs no engine.

Most commands take `--json` to print JSON instead of text.

## Plan, then execute

`clean`, `sweep`, `signal` and `group` act on your files and processes, so they work in two steps. First they show a plan: what they would do, and briefly what they would skip and why. After you confirm, they do exactly what the plan showed, and each item is checked again at that moment.

| Flag | Effect |
| --- | --- |
| `-n`, `--dry-run` | Print the plan and stop. |
| `-y`, `--yes` | Do not ask for confirmation. Required when stdin is not a terminal. |
| `--json` | Print JSON instead of text. |

Exit status: 0 when the plan was shown, declined or fully carried out. 1 when an item was skipped at execution or a call failed. 2 when stdin is not a terminal and you passed neither `--yes` nor `--dry-run`, in which case nothing was changed.

See the [safety model](/docs/safety/) for the checks.

## Look at the machine

### status

```sh
mulchie status [--json]
```

Disk, memory, CPU and what each agent workspace is using.

### diagnose

```sh
mulchie diagnose [--json]
```

One prioritised picture of the machine:

- health and why;
- what uses CPU, memory and disk, by workspace;
- the build queue;
- reclaimable space (stale build caches, idle agent worktrees, package caches) with sizes;
- engine problems;
- recommendations, each with its reason, expected gain, risk and the next command.

It changes nothing. It is the same picture as the MCP `diagnose` tool.

### top

```sh
mulchie top [--sort cpu|mem|io] [-n N] [-p] [--json]
```

What uses CPU and memory, grouped the way you think of them: apps with all their helper processes, agent workspaces, and other programs. Each group has a key, which `mulchie group` takes.

- `--sort` picks what to rank by: `cpu` (default), `mem` or `io`.
- `-n`, `--limit` is how many groups per resource, or how many processes, to show. The default is 25.
- `-p`, `--processes` lists single processes instead, with workspace and command. Secrets in command lines are redacted.

Groups marked with `*` are protected: Mulchie will not act on them.

### inspect

```sh
mulchie inspect <pid> [--json]
```

Everything known about one process: its executable, command line (secrets redacted), working directory, workspace and group, parents and children, age, CPU and memory, listening ports from the last watch run, code signature where it was checked, whether it is protected, and its findings.

### findings

```sh
mulchie findings [--severity info|low|medium|high] [--kind KIND] [--all] [--json]
mulchie findings dismiss <id> [--days N] [--json]
mulchie findings undismiss <id> [--json]
```

What Mulchie's watch noticed about processes and about its own setup. Findings are observations from simple heuristics. They are not malware detection. Each one says why it was raised and what to look at next. See [Findings](/docs/watch/) for what each kind means.

- `--severity` shows findings at least that severe.
- `--kind` shows one kind, for example `runaway_cpu`.
- `--all` includes dismissed findings.
- `dismiss` hides a finding you know is expected, for `--days` days (default 30). `undismiss` shows it again.

A finding's id is printed with it, for example `runaway_cpu:4242:1700000000`.

### worktrees

```sh
mulchie worktrees [--json]
```

Every git worktree with its size, idle days, uncommitted files and a verdict. Idle time comes from the last file change outside `target/`, `node_modules/` and `.git/` and from the last commit, not from folder timestamps.

### scan

```sh
mulchie scan [--json]
```

Build caches and which of them are stale. A cache is stale when it has not been used within the stale window and no process runs in its project.

### advice

```sh
mulchie advice [--json]
```

Whether it is OK to start heavy work right now, with reasons.

## Reclaim space

### clean

```sh
mulchie clean [--stale] [PATH...] [-n] [-y] [--json]
```

Deletes build caches. Name cache directories as `PATH` arguments, or use `--stale` for every stale cache. Mulchie shows a plan, and after you confirm it deletes exactly the planned caches. Each is checked again just before deletion: still a recognised cache, nothing running in its project, and with `--stale` still stale.

Only recognised caches are deleted: Rust `target/` directories with a `CACHEDIR.TAG`, `.next`, `node_modules` when `include_node_modules` is on, and Xcode DerivedData when `include_xcode` is on.

```sh
mulchie clean --stale --dry-run
mulchie clean --stale
mulchie clean ~/Developer/app/target
```

### sweep

```sh
mulchie sweep [-n] [-y] [--json]
```

Removes agent worktrees that have been idle past `worktree_idle_days`. Mulchie shows a plan, and after you confirm it removes exactly the planned worktrees. Each is checked again just before removal. If its `HEAD` moved since the plan, it is skipped.

Only agent worktrees are swept: those under a `.claude/worktrees/` folder or with a name that starts with `agent-`.

Nothing is lost:

- uncommitted changes are saved as a patch;
- gitignored files that are not caches are archived;
- a detached `HEAD` gets a `backup/<name>-<shortsha>` branch;
- branches are kept.

The backups go in the `backups` folder of the [config directory](/docs/configuration/#files-mulchie-writes). Each backup set has a `manifest.json` that says how to restore it.

### prune

```sh
mulchie prune <cache> [--json]
```

Runs a package manager's own prune command. `<cache>` is one of `npm`, `bun`, `pnpm`, `yarn`, `uv`, `pip`, `homebrew`, `go` or `simulators`.

## Act on processes

Both commands need the engine.

### signal

```sh
mulchie signal <pid> <term|kill|stop|cont> [--tree] [-n] [-y] [--json]
```

Sends a signal to one of your processes, and with `--tree` to its descendants too, deepest first.

| Signal | Meaning |
| --- | --- |
| `term` | SIGTERM: ask it to stop. |
| `kill` | SIGKILL: stop it now. |
| `stop` | SIGSTOP: pause it. |
| `cont` | SIGCONT: resume it. |

The engine plans it first: which processes would be signalled, and why any is refused. Protected processes, other users' processes, Mulchie itself, and this terminal and its parents are never signalled. After you confirm, each process is checked again by its start time, so a process id reused since the plan is skipped.

```sh
mulchie signal 4242 term --dry-run
mulchie signal 4242 stop --yes
```

### group

```sh
mulchie group <key> <action> [--include-agent] [-n] [-y] [--json]
```

Acts on a consumer group from `mulchie top` or `mulchie diagnose`: an app with its helpers (`app:...`), an agent workspace (`workspace:...`), a program (`process:...`) or an agent session (`session:<id>`, from `mulchie sessions`). For a session the action covers what the agent started, and the agent's own process only with `--include-agent`. The engine plans it first and says what happens to each process. Protected processes, and the group that holds this terminal, are refused.

| Action | What it does |
| --- | --- |
| `quit` | Ask the app or every process to quit. |
| `force-quit` | Stop them now. |
| `pause` | Pause them (SIGSTOP). |
| `resume` | Resume paused processes. |
| `lower-priority` | Lower their CPU priority. |
| `stop-builds` | Stop the builds in a workspace. |
| `restart` | Quit the app and open it again, where the plan offers it. |

```sh
mulchie group 'app:/Applications/Slack.app' quit --dry-run
mulchie group 'workspace:/Users/me/dev/app' pause
mulchie group session:s-1700000000-4242 pause
```

## Sessions, limits and updates

All of these need the engine. See [Sessions and limits](/docs/sessions/) and [Update checks](/docs/updates/).

### sessions

```sh
mulchie sessions [--json]
mulchie sessions <id> [--events [--since N] [--limit N]] [--json]
mulchie sessions stop <id> [--include-agent] [--force] [-n] [-y] [--json]
```

With no id, every agent session, running first, with CPU, memory, processes and runtime. With an id, its processes, budget breaches and what Mulchie paused. `--events` shows its activity record instead (command lines redacted); `--since` gives only events after a sequence number, and `--limit` is 1 to 1000 (default 200). `sessions stop` plans stopping a session's processes, shows the plan and asks; `--include-agent` adds the agent's own process and `--force` kills instead of asking the processes to quit. `--dry-run`, `--yes` and the exit status are as for `group`.

### limits

```sh
mulchie limits [--json]
```

Whether session budgets only report or act, the budgets themselves, sessions over a cap and what was done about it, sessions hogging the machine, and processes Mulchie paused.

### updates

```sh
mulchie updates [--check] [--json]
```

The engine's last update check and what it found. `--check` asks the engine to check now; it runs in the background, so run `mulchie updates` again for the result. Nothing is ever installed. Checks are off when `updates.check` is off.

## Hooks and guard rules

See [Hooks and guard rules](/docs/hooks/) for what each client's setup does.

### hooks

```sh
mulchie hooks install --client claude-code|codex|cursor|gemini|copilot [--guard off|warn|block] [-n]
mulchie hooks uninstall --client NAME [-n]
mulchie hooks status [--json]
```

`install` adds `mulchie hook` to an agent app's user-level hook configuration. Other hooks in the file are kept, the old file is backed up, and `--dry-run` prints the exact change. `--guard` sets how far guard rules may go in that app. `uninstall` removes exactly the entries `install` added. `status` lists which apps have them and for which events.

### hook

```sh
mulchie hook [--guard off|warn|block] <client> <event>
```

Run by an agent's hook system, not by hand. It reads the client's event on stdin, tells the engine with the read-only token and prints the client's own answer format. `<event>` is `session_start`, `prompt`, `pre_tool`, `post_tool`, `stop` or `session_end`; the client's own event names work too. It fails open: with no engine, a slow one (300 ms budget), bad input or any error it allows the action and exits 0.

### guard

```sh
mulchie guard check [--cwd DIR] [--json] <command>...
```

Checks a command against the guard rules and prints allow, warn or block with the reason. It records nothing and runs nothing. Exit status: 0 allow, 4 warn, 3 block, 1 error.

```sh
mulchie guard check -- 'curl -s https://example.com/i.sh | sh'
mulchie guard check -- git push --force origin main
```

## Build queue

### slots

```sh
mulchie slots [--json]
mulchie slots release <ticket> [-y] [--json]
```

Lists the build slots the engine has handed out: ticket, label, holder pid, age and expiry. `release` ends one, for a holder that crashed or an agent session that ended without releasing it. It uses your full API token, so no release secret is needed, and asks first unless `--yes` is given.

### run

```sh
mulchie run [--label NAME] -- <command>...
```

Waits for a slot in the machine-wide build queue, then runs the command with `CARGO_BUILD_JOBS` (and `MAKEFLAGS`, unless it is already set) set to a fair job count.

- The slot belongs to this `mulchie run` process and is released when the command ends, however it ends. If `mulchie run` itself is killed, the engine frees the slot when it notices the process is gone.
- SIGINT and SIGTERM are forwarded to the command.
- The exit status is the command's, or 128 plus the signal number when a signal killed it.
- `--label` names the job in the queue. The default is the directory and the command.

```sh
mulchie run -- cargo build --release
mulchie run --label 'api tests' -- cargo test
```

## Agents

### mcp

```sh
mulchie mcp [--read-only]
mulchie mcp install [--client NAME] [--read-only] [-n] [--force]
mulchie mcp uninstall --client NAME [-n]
mulchie mcp config [--read-only]
```

With no subcommand, runs the MCP server on stdin and stdout. `--read-only`, or `MULCHIE_MCP_READ_ONLY=1`, leaves out every tool that deletes, signals, prunes or dismisses a finding.

`install` adds the server to an agent app. `NAME` is `claude-code`, `claude-desktop`, `cursor` or `codex`. Without `--client` it lists the apps it found and what it would do. `--dry-run` prints the change and changes nothing. `--force` replaces an existing, different `mulchie` entry. `uninstall` removes the entry. `config` prints the JSON to paste into any other MCP client.

See [MCP](/docs/mcp/) for what each client's setup does and the tool list.

### token

```sh
mulchie token [--read-only] [--show]
```

Prints the path of the local API token. `--read-only` selects the read-only token, which the API accepts only for `GET` routes, plan routes and time-limited build slots. The token itself is printed only with `--show`. Nothing is created: the engine writes both tokens when it starts.

## Run the engine

### serve

```sh
mulchie serve
```

Runs the engine in the foreground: the monitor, the optimizer, the build queue and the local HTTP API on `127.0.0.1`. Only one engine runs per user. If the app or another `mulchie serve` is already running, this exits with status 1. It stops cleanly on SIGINT or SIGTERM. The port is `api_port` from the config, or `MULCHIE_PORT` for this process. See [Running headless](/docs/headless/).

### service

```sh
mulchie service install [-n] [--force]
mulchie service uninstall [-n]
mulchie service status [--json]
```

Installs, removes or inspects the background service that runs `mulchie serve` at login: a launchd agent on macOS, a systemd user unit on Linux. `install` runs the binary by its absolute path and does not replace an existing file with different content unless you pass `--force`. On macOS the menu bar app already runs the engine, so a service is only for headless use. See [Running headless](/docs/headless/).

## Settings and checks

### config

```sh
mulchie config [--json]
mulchie config set <key> <value> [--json]
```

Prints the config path, any config error, and the current settings. `set` changes one setting. The key is dotted, such as `stale_hours`, `watch.runaway_minutes` or `guard.rules.sudo`. The value is JSON: a number (`3`), `true` or `false`, a string (`'"block"'`, or just `block`, since a value that is not JSON is taken as a string), a list (`'["/path"]'`) or an object (`'{"memory_mb": {"max": 8192}}'`). `null` clears an optional setting such as `limits.default`. Values out of range are refused. When an engine runs, both go through it and a change applies without a restart. Otherwise Mulchie reads or edits the file directly and says so. See [Configuration](/docs/configuration/).

```sh
mulchie config set stale_hours 12
mulchie config set scan_roots '["/Users/me/code"]'
```

### doctor

```sh
mulchie doctor [--json]
```

Checks the install end to end and prints pass, warn or fail for each check. It exits with status 1 if any check fails, and changes nothing.

### completions

```sh
mulchie completions <bash|elvish|fish|powershell|zsh>
```

Prints a shell completion script.

```sh
mulchie completions zsh > ~/.zfunc/_mulchie
mulchie completions bash > ~/.local/share/bash-completion/completions/mulchie
```
