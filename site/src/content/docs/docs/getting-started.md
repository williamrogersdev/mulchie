---
title: Getting started
description: "Install Mulchie, open the menu bar app, and run your first status, advice and build-queue commands in a few minutes."
---

## Install

On macOS, the quickest route is Homebrew:

```bash
brew install --cask williamrogersdev/tap/mulchie
```

For the CLI only, or on Linux, see [Install](/install/). The app needs macOS 12 or later. The CLI runs on macOS and Linux.

## First run

1. Open Mulchie. An icon appears in the menu bar and the engine starts.
2. Open the dashboard from the panel to see CPU, memory, disk and a breakdown by repo and worktree.
3. In the dashboard, open **Agent API** and choose **Link mulchie**. This puts the CLI at `~/.local/bin/mulchie`. Check that `~/.local/bin` is on your `PATH`.

The app bundles the CLI. If you installed only the CLI, run `mulchie serve` to start the engine (see [Running headless](/docs/headless/)).

## Look at the machine

```bash
mulchie status
mulchie top --sort cpu -n 10
mulchie worktrees
```

`status` shows disk, memory, CPU and per-workspace usage. `top` lists processes by CPU, memory or I/O. `worktrees` lists every git worktree with its size, idle days, uncommitted files and a verdict.

Commands use the running app or engine when there is one and fall back to a local scan when there is not.

## Ask before heavy work

```bash
mulchie advice
```

This says whether now is a good time to start heavy work, with the reasons.

## Queue a build

```bash
mulchie run -- cargo build
```

`run` waits for a free build slot, runs your command with a fair `CARGO_BUILD_JOBS`, and releases the slot when it exits. See the [build queue](/docs/cli/#build-queue).

## Clean up

Preview first, then run:

```bash
mulchie scan
mulchie clean --stale --dry-run
mulchie sweep --dry-run
```

Nothing is deleted in a project where a process is running, and idle worktrees are backed up before removal. Read the [safety model](/docs/safety/) before you turn on automatic reclaim.

## Connect your agents

Add Mulchie as an MCP server in your agent's config:

```json
{ "mcpServers": { "mulchie": { "command": "mulchie", "args": ["mcp"] } } }
```

See [MCP](/docs/mcp/) for the tools. The dashboard under **Agent API** has a snippet for `AGENTS.md`.

<!-- VERIFY: `mulchie doctor` (install check) is in the architecture doc but not the README; add a "Check your install" step once it ships. -->
