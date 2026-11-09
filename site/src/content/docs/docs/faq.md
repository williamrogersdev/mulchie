---
title: FAQ
description: "Answers about what Mulchie does, which agents it works with, what it deletes, whether it sends data anywhere, and which platforms it supports."
---

## Which agents does it work with?

Any. Mulchie reads the process table, so it sees Claude Code, Codex, Cursor agents and other tools without setup. Agents that speak MCP can also ask it for build slots and advice.

## Does it delete my code?

Mulchie removes build caches such as `target/` and `.next`, and idle agent worktrees. Before it removes a worktree it saves uncommitted changes as a patch and gives a detached `HEAD` a backup branch. It never deletes branches and never touches a project with a running process. See the [safety model](/docs/safety/).

## Can I try it without deleting anything?

Yes. `mulchie status`, `top`, `worktrees` and `scan` only read. Use `--dry-run` on `sweep` and `clean` to see what would happen. Automatic reclaim and sweep are off by default.

## Does it send data anywhere?

The API listens on `127.0.0.1` only. <!-- VERIFY: confirm there is no telemetry and that the update check (latest.json on GitHub Releases) is the only outbound request. -->

## What is a worktree "idle"?

A worktree is idle when nothing outside `target/`, `node_modules/` and `.git/` has changed and no commit has been made for the configured number of days. Folder timestamps are not used.

## Does it work on Linux?

The engine, CLI, API and MCP server do. The menu bar app is macOS only. See [Running headless](/docs/headless/).

## Does it work on Windows?

No. Windows is out of scope.

## Why does it need a build queue?

When ten agents start `cargo build` at once, each one slows the others and the machine swaps. `mulchie run -- <command>` waits for a free slot and sets a fair job count, so builds finish sooner as a group.

<!-- VERIFY: "finish sooner as a group" is an expectation, not a measurement. Reword or drop if you do not want to claim it. -->

## How is it licensed?

MIT. The source is on [GitHub](https://github.com/williamrogersdev/mulchie).
