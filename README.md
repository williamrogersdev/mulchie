# Mulchie

Mulchie keeps a machine healthy when many AI coding agents run on it at once. It shows what is using CPU, memory, disk I/O and disk space, broken down by repo and worktree. It lets you or the agents themselves act on it: pause, renice or stop processes, queue heavy builds, and reclaim space from build caches and idle worktrees without losing work.

It runs as a menu bar app with a technical dashboard, a CLI, a local HTTP API and an MCP server.

## What it does

**Monitor**
- Samples every 3 seconds: CPU, load average, memory, swap, disk free and per-process disk I/O. Keeps 30 minutes of history.
- Attributes every process to its git repo or worktree from its working directory, falling back to its parent chain. Build tools that run inside `~/.cargo/registry` still count against the worktree that started them.
- Classifies processes as agents, builds or runtimes, and flags orphaned workspaces: a deleted worktree, or heavy CPU with no agent attached.
- Forecasts when the disk will fill from the last 30 minutes of free-space samples.

**Optimize**
- Lowers the priority of build processes when CPU or memory is under pressure.
- Runs a machine-wide build queue. `mulchie run -- cargo build` waits for a free slot and runs with a fair `CARGO_BUILD_JOBS`. Agents can use the same queue over MCP.
- Answers "is now a good time for heavy work?" with reasons.

**Reclaim**
- Build caches: Rust `target/`, `.next`, optional `node_modules` and Xcode DerivedData. A cache is stale when it has not been used within the stale window and no process runs in its project. Worktree caches have their own window.
- Git worktrees: size, idle days, uncommitted files, merged state and a verdict for each. "Idle" is measured from the last file change outside `target/`, `node_modules/` and `.git/`, and from the last commit, not from folder timestamps.
- Sweep: removes agent worktrees idle past the configured window. Branches are kept. A detached HEAD gets a `backup/<name>` branch first. Uncommitted changes are saved as a patch in `~/.config/mulchie/backups/` before removal.
- Package and toolchain stores: npm, bun, pnpm, yarn, uv, pip, Homebrew, Go, iOS simulators, Docker, model stores. Prunable stores run the tool's own prune command.
- Sends a notification when free space drops below a threshold.

**Safety**
- Only recognised build caches under the configured scan roots can be deleted, and only by canonical, non-symlink paths.
- Nothing in a project with a running process is cleaned or removed.
- Signals go only to processes owned by the current user, never to pid 1 or Mulchie itself.
- The API listens on `127.0.0.1` only, needs a bearer token stored with mode `0600`, and refuses any request with a browser `Origin` header.

## Install

Build from source on macOS (Rust and Bun required):

```bash
./scripts/bundle.sh
open target/release/bundle/macos/Mulchie.app
```

The app bundles the `mulchie` CLI. Use **Agent API → Link mulchie** in the dashboard to put it on your `PATH` at `~/.local/bin/mulchie`, or install it on its own:

```bash
cargo install --path crates/mulchie-cli
```

On a machine without a menu bar, run the engine and API headless:

```bash
mulchie serve
```

## CLI

```
mulchie status [--json]          disk, memory, CPU and per-workspace usage
mulchie top [--sort cpu|mem|io] [-n N] [--json]
mulchie worktrees [--json]       size, idle days, dirty files and verdict for every worktree
mulchie sweep [--dry-run]        remove idle agent worktrees (patch backup, branches kept)
mulchie scan [--json]            build caches and their staleness
mulchie clean --stale [--dry-run]
mulchie prune <cache>            npm, bun, pnpm, yarn, uv, pip, homebrew, go, simulators
mulchie signal <pid> <term|kill|stop|cont>
mulchie advice [--json]          is it OK to start heavy work now?
mulchie run [--label L] -- <cmd> wait for a build slot, run, release
mulchie mcp                      MCP server on stdio
mulchie serve                    headless engine and API
mulchie config                   config path and current settings
```

Commands use the running app when it is up and fall back to a local scan otherwise.

## MCP

```json
{ "mcpServers": { "mulchie": { "command": "mulchie", "args": ["mcp"] } } }
```

Tools: `get_status`, `can_i_build`, `top_processes`, `list_workspaces`, `get_history`, `disk_breakdown`, `reclaim_candidates`, `clean_caches`, `prune_cache`, `remove_worktrees`, `sweep_worktrees`, `workspace_action`, `process_action`, `acquire_build_slot`, `release_build_slot`.

A snippet for `AGENTS.md` is in the dashboard under **Agent API**.

## HTTP API

Base URL `http://127.0.0.1:47474`. Send `Authorization: Bearer $(cat ~/.config/mulchie/token)`.

| Method | Path | Body / returns |
| --- | --- | --- |
| GET | `/v1/health` | open; health, disk, load, free slots |
| GET | `/v1/state` | full snapshot |
| GET | `/v1/history` | 30 minutes of 3 s samples |
| GET | `/v1/advice` | `ok_to_start_heavy_work`, `suggested_jobs`, `reasons` |
| GET | `/v1/sweep` | worktrees a sweep would remove |
| POST | `/v1/scan` | rescan caches and the disk map |
| POST | `/v1/clean` | `{paths}` or `{stale: true}` |
| POST | `/v1/process` | `{pid, action: term\|kill\|stop\|cont\|renice, nice?}` |
| POST | `/v1/workspace` | `{path, action: pause\|resume\|deprioritize\|stop_builds\|kill_all}` |
| POST | `/v1/worktree/remove` | `{paths, backup}` |
| POST | `/v1/sweep` | remove idle agent worktrees |
| POST | `/v1/prune` | `{key}` |
| POST | `/v1/lease` | `{label, pid, ticket?}` → `granted, jobs, position` |
| POST | `/v1/release` | `{ticket}` |

## Configuration

`~/.config/mulchie/config.json`, also editable in the dashboard under **Settings**. `MULCHIE_PORT` overrides the API port.

## Layout

- `crates/mulchie-core`: monitor, engine, disk map, reclaim, API
- `crates/mulchie-cli`: the `mulchie` binary and MCP server
- `crates/mulchie-app`: menu bar app and dashboard

## License

MIT
