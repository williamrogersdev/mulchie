---
title: MCP server
description: "Connect AI coding agents to Mulchie over MCP with one command. Every tool, which ones change things, plan-then-execute, read-only mode and build slots."
---

Use the MCP server to let your agents look at the machine, ask whether to start a build, join the build queue, and plan cleanups that you approve.

`mulchie mcp` runs an MCP server on stdio. It talks to the same engine as the CLI and the app, so the engine must be running (the app, `mulchie serve` or the service). If it is not, tools return an error that says how to start it.

## Connect an agent

```sh
mulchie mcp install --client claude-code
```

`--client` is `claude-code`, `claude-desktop`, `cursor` or `codex`. Without it, the command lists the agent apps it found and what it would do, and changes nothing. Add `--dry-run` to see the change first. Restart the agent app afterwards.

What each client's setup does:

| Client | How Mulchie is added |
| --- | --- |
| `claude-code` | Runs `claude mcp add --scope user mulchie -- <mulchie> mcp`. The `claude` command must be on your `PATH`. |
| `codex` | Runs `codex mcp add mulchie -- <mulchie> mcp` when `codex` is on your `PATH`. Otherwise edits `~/.codex/config.toml`. |
| `claude-desktop` | Edits `claude_desktop_config.json`: `~/Library/Application Support/Claude/` on macOS, `~/.config/Claude/` on Linux. |
| `cursor` | Edits `~/.cursor/mcp.json`. |

When Mulchie edits a config file, it keeps the other entries, copies the old file to `<file>.mulchie-backup-<unix time>`, writes the new one atomically, and leaves a file it cannot parse alone. An existing, different `mulchie` entry is not replaced unless you pass `--force`. `<mulchie>` is the absolute path of the `mulchie` binary you ran.

To remove it:

```sh
mulchie mcp uninstall --client claude-code
```

For any other MCP client, print the JSON to paste:

```sh
mulchie mcp config
```

```json
{ "mcpServers": { "mulchie": { "command": "/path/to/mulchie", "args": ["mcp"] } } }
```

MCP lets an agent ask Mulchie. To have Mulchie hear from the agent as well (session start, before and after shell commands) and to run guard rules, install [hooks](/docs/hooks/) too. They are separate: hooks use `mulchie hooks install`.

The dashboard's **Agent API** page has the same commands and a block of text to paste into `AGENTS.md` or `CLAUDE.md`.

## Read-only mode

```sh
mulchie mcp install --client claude-code --read-only
```

This registers `mulchie mcp --read-only`. You can also start the server with `MULCHIE_MCP_READ_ONLY=1`. In read-only mode:

- The server offers only tools that change nothing, plus the build-slot tools. `check_updates`, `execute_plan`, `prune_cache` and `dismiss_finding` are left out.
- It connects to the engine with the [read-only token](/docs/http-api/#tokens), so the engine itself refuses anything else.
- Plan tools still work and show what would happen, but they return no `plan_id`. You carry the plan out yourself with `mulchie clean`, `mulchie sweep`, `mulchie signal` or `mulchie group`.

## Plan, then execute

Nothing is deleted or signalled in one call. Changes take two:

1. The agent calls a plan tool: `plan_cache_cleanup`, `plan_worktree_removal`, `plan_worktree_sweep` or `plan_process_action`. These change nothing. The result has a `plan_id`, the items that are ready (with sizes, and what would be backed up) and the items that are not, with reasons.
2. The agent shows the plan to you. If you approve, it calls `execute_plan` with the `plan_id` and the targets of the items you approved.

The server keeps each plan it showed, in memory. `execute_plan` accepts only targets of ready items in a stored plan. A plan can be executed once, expires after 30 minutes, and is lost if the server restarts. The engine's safety gate then checks each item again just before it runs and skips anything that changed. The result lists what was done, what was skipped and why, every backup with its path, and how to restore.

`prune_cache` is the exception. Without `execute=true` it only shows the cache, its size and what pruning does. With `execute=true` it runs the package manager's own prune command.

See the [safety model](/docs/safety/) for what the gate checks.

## Tools

Agents should start with `diagnose`.

| Tool | Changes things? | What it does |
| --- | --- | --- |
| `diagnose` | No | One prioritised picture: health and why, who uses CPU and memory, the build queue, reclaimable space, findings, engine problems and recommendations. Each recommendation names the next tool call. |
| `get_status` | No | CPU and memory in words, the top consumer groups with their keys, and raw figures: disk, load, swap, counts, build slots, disk forecast and recent actions. |
| `can_i_build` | No | Whether now is a good moment for heavy work, free build slots and a suggested job count. |
| `top_processes` | No | Processes ranked by CPU, memory or I/O. Arguments: `sort`, `limit`, `workspace`. |
| `list_workspaces` | No | Every repo or worktree with running processes. Argument: `include_processes`. |
| `get_history` | No | Samples every 3 seconds for up to 30 minutes. Argument: `minutes`. |
| `disk_breakdown` | No | Toolchain and package caches, model stores, and every git worktree with its size, idle days, uncommitted files and verdict. |
| `reclaim_candidates` | No | Build caches with size, last use, and in-use and stale flags, from the engine's last scan. |
| `list_findings` | No | What the watch noticed. Arguments: `severity`, `kind`, `include_dismissed`. See [Findings](/docs/watch/). |
| `list_sessions` | No | Agent sessions, running first, with CPU, memory, processes, build slots, runtime and whether their limits are enforced here. See [Sessions and limits](/docs/sessions/). |
| `get_session` | No | One session: its processes, budget breaches and what Mulchie paused. Argument `id`; with `events=true` also its activity record (`since`, `limit`, or `tail` and `before` to read backwards). |
| `limits_status` | No | Whether budgets only report or act, the budgets, sessions over a cap and what was done, sessions hogging the machine, and processes Mulchie paused. |
| `list_updates` | No | The last update check and the update findings, with the command that would update each. See [Update checks](/docs/updates/). |
| `check_command` | No | Ask the [guard rules](/docs/hooks/#guard-rules) about a shell command before running it: allow, warn or block, with the reason. Records and runs nothing. Arguments: `command`, `cwd`. |
| `inspect_process` | No | Everything known about one process, read now. Argument: `pid`. |
| `plan_cache_cleanup` | No | Plan deleting build caches: `stale=true`, or `paths`. |
| `plan_worktree_removal` | No | Plan removing specific worktrees: `paths`, and `allow_dirty` to save uncommitted changes as a patch and remove anyway. |
| `plan_worktree_sweep` | No | Plan removing every agent worktree idle past the configured window. |
| `plan_process_action` | No | Plan an action on one process (`pid`, `signal`, `tree`), on a group (`group`, `action`), or on a session (`session`, `include_agent`, `action`). |
| `execute_plan` | Yes, destructive | Carry out approved items of a stored plan. |
| `prune_cache` | Yes, destructive | Preview or run a package manager's own prune command. Arguments: `key`, `execute`. |
| `check_updates` | Contacts the network | Ask the engine to check for updates now (GitHub, npm and, if you turned advisories on, OSV.dev). Refused when `updates.check` is off. It installs nothing. |
| `dismiss_finding` | Changes a setting | Hide a finding for `days` (default 30), or show it again with `undo=true`. It is saved in Mulchie's config and changes nothing else on the machine. |
| `acquire_build_slot` | Takes a slot | Join the build queue, poll with the ticket, renew. |
| `release_build_slot` | Gives up a slot | Release a slot or leave the queue. |

Each tool carries MCP annotations: `readOnlyHint` for the tools marked "No", `destructiveHint` for `execute_plan` and `prune_cache`, and `openWorldHint` for `check_updates`. In read-only mode, only the tools marked "No" and the two build-slot tools are offered.

`plan_process_action` acts on processes you own. For a session it covers what the agent started, and the agent's own process only with `include_agent`. Protected processes are never acted on: operating-system programs, Mulchie itself, and the app and terminal the agent runs in.

Mulchie also offers two prompts (`whats-going-on` and `optimize`) and read-only resources: `mulchie://diagnosis`, `mulchie://state`, `mulchie://plan/sweep`, `mulchie://actions` and `mulchie://findings`.

## Build slots

Agents wrap heavy work in a build slot so that several agents do not start large builds at once:

1. Call `acquire_build_slot` with a `label`. Keep calling it with the returned `ticket`, every few seconds, until `granted` is true. Then run with the returned job count.
2. A granted slot lasts `ttl_secs` (default 900, at most 3600). During long work, call it again with the same ticket before that time passes to renew it. If it is not renewed, the slot expires and goes to someone else, so a crashed agent cannot block the queue.
3. Call `release_build_slot` as soon as the work ends.

Build slots work in read-only mode. The server holds the secret that releases or renews a ticket, and never shows it to the agent.

From a shell, `mulchie run -- <command>` does the same thing. See [run](/docs/cli/#run).
