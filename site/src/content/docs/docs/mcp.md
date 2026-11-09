---
title: MCP server
description: "Connect AI coding agents to Mulchie over MCP. Config snippet and the tools for status, build slots, reclaim and process control."
---

`mulchie mcp` runs an MCP server on stdio. It talks to the same engine as the CLI and the app.

## Add it to your agent

```json
{ "mcpServers": { "mulchie": { "command": "mulchie", "args": ["mcp"] } } }
```

Make sure `mulchie` is on the `PATH` of the agent process. In the app, **Agent API → Link mulchie** installs it at `~/.local/bin/mulchie`.

The dashboard under **Agent API** also has a snippet to paste into `AGENTS.md`, so agents know when to ask Mulchie before a heavy build.

## Tools

<!-- VERIFY: tool names are from README.md. Check against the server's tools/list, and add titles, read-only and destructive annotations. -->

| Tool | What it does |
| --- | --- |
| `get_status` | Disk, memory, CPU and per-workspace usage. |
| `can_i_build` | Whether now is a good time for heavy work, with reasons. |
| `top_processes` | Processes ranked by CPU, memory or I/O. |
| `list_workspaces` | Repos and worktrees with their usage. |
| `get_history` | Recent samples (30 minutes at 3 s). |
| `disk_breakdown` | Where disk space goes. |
| `reclaim_candidates` | Stale caches and idle worktrees that could be reclaimed. |
| `clean_caches` | Delete stale build caches. |
| `prune_cache` | Run a package store's prune command. |
| `remove_worktrees` | Remove named worktrees, with backups. |
| `sweep_worktrees` | Remove idle agent worktrees. |
| `workspace_action` | Pause, resume, deprioritize, stop builds or kill all in a workspace. |
| `process_action` | Signal or renice one process. |
| `acquire_build_slot` | Request a build slot from the queue. |
| `release_build_slot` | Release a slot. |

## Build slots

`acquire_build_slot` returns whether a slot was granted, how many jobs to use and the queue position. A slot held through MCP has a time limit and expires if the agent does not renew it, so a crashed agent cannot block the queue.

<!-- VERIFY: lease TTL and renewal behaviour of MCP leases (see docs/ARCHITECTURE.md "Build queue"). -->

## Destructive tools

`clean_caches`, `remove_worktrees`, `sweep_worktrees`, `workspace_action` and `process_action` change things. They go through the same safety checks as the CLI: see the [safety model](/docs/safety/).
