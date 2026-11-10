---
title: Getting started
description: "Install Mulchie, check the setup, and run your first status, diagnose, advice and build-queue commands, then connect an agent over MCP."
---

This page takes you from install to a first look at your machine, a first cleanup preview and a connected agent.

## Install

On macOS, the quickest route is Homebrew:

```sh
brew install --cask williamrogersdev/tap/mulchie
```

For the CLI only, or on Linux, see [Install](/install/). The app needs macOS 12 or later. The CLI runs on macOS and Linux.

## First run

1. Open Mulchie. An icon appears in the menu bar and the engine starts.
2. Open the dashboard from the panel to see CPU, memory, disk and a breakdown by repo and worktree.
3. In the dashboard, open **Agent API** and choose **Link mulchie into ~/.local/bin** to put the CLI on that path. Check that `~/.local/bin` is on your `PATH`.

The app bundles the CLI. If you installed only the CLI, run `mulchie serve` to start the engine (see [Running headless](/docs/headless/)).

## Check your install

```sh
mulchie doctor
```

`doctor` prints pass, warn or fail for each check: the config file, the permissions of the config directory and token, the running engine, git, whether `mulchie` is on your `PATH`, the background service and how to add the MCP server. It changes nothing, and exits with status 1 if any check fails.

## Look at the machine

```sh
mulchie status
mulchie diagnose
mulchie top
mulchie worktrees
```

- `status` shows disk, memory, CPU and what each agent workspace is using.
- `diagnose` gives one prioritised picture: health and why, who uses CPU and memory, the build queue, reclaimable space, findings, and recommendations. Each recommendation names the next command to run. It changes nothing.
- `top` groups processes by app, workspace and program. Add `--processes` to see single processes.
- `worktrees` lists every git worktree with its size, idle days, uncommitted files and a verdict.

Commands use the running engine when there is one and look at the machine locally when there is not, and they say which on stderr.

## Ask before heavy work

```sh
mulchie advice
```

This says whether now is a good time to start heavy work, with the reasons.

## Queue a build

```sh
mulchie run -- cargo build
```

`run` waits for a free build slot, runs your command with a fair `CARGO_BUILD_JOBS`, and releases the slot when it exits. See [run](/docs/cli/#run).

## Clean up

Each of these shows a plan first. Add `--dry-run` to stop after the plan.

```sh
mulchie scan
mulchie clean --stale --dry-run
mulchie sweep --dry-run
```

After you confirm, Mulchie deletes exactly what the plan showed, checking each item again just before it acts. Nothing is deleted in a project where a process is running, and a worktree is backed up before it is removed. Read the [safety model](/docs/safety/) before you turn on automatic reclaim.

## See what Mulchie noticed

```sh
mulchie findings
```

Findings are observations about processes, such as one left running after its parent exited or one listening on all network interfaces. They are not malware detection. See [Findings](/docs/watch/).

## Connect your agents

```sh
mulchie mcp install --client claude-code
```

Other clients are `claude-desktop`, `cursor` and `codex`. Without `--client`, the command lists the agent apps it found and changes nothing. Add `--read-only` for an agent that should look and plan but not delete, signal or prune. Restart the agent app afterwards.

See [MCP](/docs/mcp/) for the tools and how plans work. The dashboard under **Agent API** has the same commands and text to paste into `AGENTS.md` or `CLAUDE.md`.
