---
title: FAQ
description: "Answers about what Mulchie does, which agents it works with, what it deletes, whether it is a sandbox, what it sends over the network, and platforms."
---

## Which agents does it work with?

Any. Mulchie reads the process table, so it sees Claude Code, Codex, Cursor agents and other tools without setup, and groups each agent with everything it started as a [session](/docs/sessions/). Agents that speak MCP can also ask it for build slots, a diagnosis and cleanup plans. `mulchie mcp install` connects Claude Code, Claude Desktop, Cursor and Codex in one step, and `mulchie hooks install` connects the hooks of Claude Code, Codex, Cursor, Gemini CLI and Copilot CLI. See [Hooks](/docs/hooks/).

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

## Is Mulchie a sandbox? Can it stop an agent?

No. Mulchie is a supervisor. It can warn an agent, hand it limit breaches and, if you configured it, pause or stop what an agent started when it goes over a budget. Limits only report unless you choose another action. [Guard rules](/docs/hooks/#guard-rules) read the text of a shell command that the agent's own hook reports, so they miss anything obfuscated and anything the agent does not report. An agent that wants to get around Mulchie can. For a hard boundary use a sandbox, a container or a separate user.

## Is Mulchie an antivirus? Do findings mean malware?

No. Mulchie is not an antivirus, and a finding never means it found malware. Findings are observations from simple heuristics, such as a process left running after its parent exited, one using a full core for 30 minutes, or one listening on all network interfaces. Mulchie has no signatures and does no file or network lookups. Use findings as a prompt to check that you know what a process is. If it is expected, dismiss it. See [Findings](/docs/watch/).

## Does it send data anywhere?

The API listens on `127.0.0.1` only, and Mulchie has no telemetry. The engine does make [update checks](/docs/updates/) by default: it asks the GitHub releases API for the latest Mulchie, and the npm registry for the latest version of each agent tool it finds installed. It never installs anything. An opt-in advisory check (off by default) sends package names and versions from your lockfiles to OSV.dev. Turn all of it off with `mulchie config set updates.check off`. The app also fetches `latest.json` from the project's GitHub Releases when you choose **Check for Updates…**, in builds that have an update key. Hooks and the MCP server talk only to the local engine. Your agent apps and package managers make their own requests, which Mulchie does not control.

## What is a worktree "idle"?

A worktree is idle when nothing outside `target/`, `node_modules/` and `.git/` has changed and no commit has been made for the configured number of days (`worktree_idle_days`, default 10). Folder timestamps are not used.

## Does it work on Linux?

The engine, CLI, API and MCP server do. The menu bar app is macOS only, and you build it from source for now (see [Install](/install/#app)). See [Running headless](/docs/headless/).

## Does it work on Windows?

No.

## Why does it need a build queue?

When ten agents start `cargo build` at once, each one slows the others and the machine swaps. `mulchie run -- <command>` waits for a free slot and sets a fair job count, so builds share the machine instead of fighting over it. Agents use the same queue through the `acquire_build_slot` and `release_build_slot` MCP tools.

## How is it licensed?

MIT. The source is on [GitHub](https://github.com/williamrogersdev/mulchie).
