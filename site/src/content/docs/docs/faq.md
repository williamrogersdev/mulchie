---
title: FAQ
description: "Answers about what Mulchie does, which agents it works with, what it deletes, what findings mean, whether it sends data anywhere, and platforms."
---

## Which agents does it work with?

Any. Mulchie reads the process table, so it sees Claude Code, Codex, Cursor agents and other tools without setup. Agents that speak MCP can also ask it for build slots, a diagnosis and cleanup plans. `mulchie mcp install` connects Claude Code, Claude Desktop, Cursor and Codex in one step.

## Does it delete my code?

Mulchie removes build caches such as `target/` and `.next`, and idle agent worktrees. Before it removes a worktree it saves uncommitted changes as a patch, archives gitignored files that are not caches, and gives a detached `HEAD` a backup branch. It never deletes branches and never touches a project with a running process. Every deletion is a plan that you see first. See the [safety model](/docs/safety/).

## Can I try it without deleting anything?

Yes. `mulchie status`, `diagnose`, `top`, `worktrees`, `scan` and `findings` only read. `clean`, `sweep`, `signal` and `group` show a plan and ask before they act, and `--dry-run` stops after the plan. Automatic reclaim and sweep are off by default.

## Can I let an agent use it without letting it delete things?

Yes. Connect it read-only:

```sh
mulchie mcp install --client claude-code --read-only
```

The agent can look, plan and use build slots. It cannot delete, signal, prune or dismiss anything. You carry out a plan yourself with the CLI. See [MCP](/docs/mcp/#read-only-mode).

## Is Mulchie an antivirus? Do findings mean malware?

No. Mulchie is not an antivirus, and a finding never means it found malware. Findings are observations from simple heuristics, such as a process left running after its parent exited, one using a full core for 30 minutes, or one listening on all network interfaces. Mulchie has no signatures and does no file or network lookups. Use findings as a prompt to check that you know what a process is. If it is expected, dismiss it. See [Findings](/docs/watch/).

## Does it send data anywhere?

The API listens on `127.0.0.1` only, and Mulchie has no telemetry. The engine, CLI and MCP server do not send data over the network. The one request the app can make is when you choose **Check for Updates…** from its menu: it fetches `latest.json` from the project's GitHub Releases, and only in builds that have updating set up. Your agent apps and package managers make their own requests, which Mulchie does not control.

## What is a worktree "idle"?

A worktree is idle when nothing outside `target/`, `node_modules/` and `.git/` has changed and no commit has been made for the configured number of days (`worktree_idle_days`, default 10). Folder timestamps are not used.

## Does it work on Linux?

The engine, CLI, API and MCP server do. The menu bar app is macOS only. See [Running headless](/docs/headless/).

## Does it work on Windows?

No.

## Why does it need a build queue?

When ten agents start `cargo build` at once, each one slows the others and the machine swaps. `mulchie run -- <command>` waits for a free slot and sets a fair job count, so builds share the machine instead of fighting over it. Agents use the same queue through the `acquire_build_slot` and `release_build_slot` MCP tools.

## How is it licensed?

MIT. The source is on [GitHub](https://github.com/williamrogersdev/mulchie).
