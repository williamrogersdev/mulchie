---
title: CLI reference
description: "Every mulchie command and flag: status, top, worktrees, sweep, scan, clean, prune, signal, advice, run, mcp, serve and config."
---

<!-- VERIFY: This page follows README.md. The CLI is being reworked (clap help, `doctor`, `service`, `completions`, plan-then-execute `sweep` and `clean`, strict flags). Check every command, flag and output description against `mulchie --help` and `mulchie <command> --help`. -->

```
mulchie <command> [options]
```

Read commands use the running app or engine and fall back to a local scan when none is running. Commands that act on live processes need the engine and tell you how to start it when it is not running.

## Inspect

### status

```
mulchie status [--json]
```

Disk, memory, CPU and per-workspace usage. `--json` prints machine-readable output.

### top

```
mulchie top [--sort cpu|mem|io] [-n N] [--json]
```

Processes ranked by CPU, memory or disk I/O, each attributed to a repo or worktree. `-n` limits the number of rows.

### worktrees

```
mulchie worktrees [--json]
```

Size, idle days, uncommitted files, merged state and a verdict for every git worktree. Idle time comes from the last file change outside `target/`, `node_modules/` and `.git/` and from the last commit, not from folder timestamps.

### scan

```
mulchie scan [--json]
```

Build caches and whether each is stale. A cache is stale when it has not been used within the stale window and no process runs in its project.

### advice

```
mulchie advice [--json]
```

Whether it is a good time to start heavy work. The result has `ok_to_start_heavy_work`, `suggested_jobs` and `reasons`.

## Reclaim

### sweep

```
mulchie sweep [--dry-run]
```

Removes agent worktrees that have been idle past the configured window. Branches are kept. A detached `HEAD` gets a `backup/<name>` branch first, and uncommitted changes are saved as a patch in `~/.config/mulchie/backups/` before removal. Use `--dry-run` to see what would be removed.

<!-- VERIFY: plan-then-execute behaviour of sweep and clean, and ignored-file backups, are in progress in the CLI. -->

### clean

```
mulchie clean --stale [--dry-run]
```

Deletes stale build caches. Only recognised caches under your scan roots are eligible, and nothing in a project with a running process is touched.

### prune

```
mulchie prune <cache>
```

Runs a package or toolchain store's own prune command. `<cache>` is one of `npm`, `bun`, `pnpm`, `yarn`, `uv`, `pip`, `homebrew`, `go`, `simulators`.

## Control processes

### signal

```
mulchie signal <pid> <term|kill|stop|cont>
```

Sends a signal to a process you own. It refuses pid 1 and Mulchie's own processes. This command needs the engine to be running.

## Build queue

### run

```
mulchie run [--label L] -- <cmd>
```

Waits for a free build slot, runs the command, then releases the slot. The command runs with a fair `CARGO_BUILD_JOBS`. `--label` names the job in the queue. Mulchie forwards SIGINT and SIGTERM to the command and exits with the command's status.

## Run the engine and integrations

### mcp

```
mulchie mcp
```

Starts the MCP server on stdio. See [MCP](/docs/mcp/).

### serve

```
mulchie serve
```

Runs the engine and API without the app. See [Running headless](/docs/headless/).

### config

```
mulchie config
```

Prints the config path and current settings. See [Configuration](/docs/configuration/).

## Other commands

<!-- VERIFY: document `doctor`, `service install|uninstall|status`, `completions` and `--version` here once they ship. docs/ARCHITECTURE.md describes them; README.md does not. -->

- `mulchie doctor` checks the install end to end.
- `mulchie service install` sets up the headless engine as a launchd agent or systemd user unit.
- `mulchie completions <shell>` prints shell completions.
