---
title: Mulchie documentation
description: "Guides and reference for Mulchie, which keeps a machine healthy while several AI coding agents share it: a CLI, a menu bar app and an MCP server."
---

Use these pages to install Mulchie, see what is using your machine, queue heavy builds, reclaim space safely and connect your agents.

Mulchie keeps one machine healthy while several AI coding agents work on it. It shows what uses CPU, memory, disk I/O and disk space by repo and worktree, and what each agent session and everything it started is using. It queues heavy builds, notices processes that look left over or out of place, and reclaims space from build caches and idle worktrees without losing work. It can hold agents to budgets you set, tell them when a command looks risky, and report newer versions. It is a supervisor, not a sandbox and not an antivirus.

It ships as the `mulchie` command, a macOS menu bar app with a dashboard, and an MCP server (`mulchie mcp`). The first release ships only the command; the app is built from source for now (see [Install](/install/#app)). The engine also serves a local HTTP API. The engine, CLI and MCP server run on macOS and Linux. The app is macOS only.

## Start here

- [Getting started](/docs/getting-started/): install, first run, your first commands.
- [Install options](/install/): every way to install, verify, update and uninstall.

## Reference

- [CLI reference](/docs/cli/): every `mulchie` command.
- [MCP](/docs/mcp/): connect an agent, and the tools it can call.
- [Findings](/docs/watch/): what Mulchie's watch reports and what each kind means.
- [Sessions and limits](/docs/sessions/): what each agent is using, budgets, and the activity record.
- [Hooks and guard rules](/docs/hooks/): connect Claude Code, Codex, Cursor, Gemini CLI or Copilot CLI, and what the guard does.
- [Update checks](/docs/updates/): what Mulchie contacts to look for updates, and how to turn it off.
- [HTTP API](/docs/http-api/): endpoints for scripts and `curl`.
- [Configuration](/docs/configuration/): the config file and its settings.

## Guides

- [Safety model](/docs/safety/): what Mulchie checks before it deletes or signals anything.
- [Running headless](/docs/headless/): the engine as a background service on Linux or a Mac without the app.
- [Troubleshooting](/docs/troubleshooting/)
- [FAQ](/docs/faq/)
