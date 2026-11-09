---
title: Mulchie documentation
description: "Guides and reference for Mulchie, the monitor, build queue and safe cleaner for machines that run many AI coding agents."
---

Mulchie keeps one machine healthy while many AI coding agents run on it. It shows what uses CPU, memory, disk I/O and disk space by repo and worktree, queues heavy builds, and reclaims space from build caches and idle worktrees without losing work.

It runs as a macOS menu bar app with a dashboard, a CLI, a local HTTP API and an MCP server. The engine, CLI and API also run headless on Linux.

## Start here

- [Getting started](/docs/getting-started/): install, first run, your first commands.
- [Install options](/install/): every way to install, verify, update and uninstall.

## Reference

- [CLI reference](/docs/cli/): every `mulchie` command.
- [MCP](/docs/mcp/): the tools agents can call.
- [HTTP API](/docs/http-api/): endpoints for scripts and `curl`.
- [Configuration](/docs/configuration/): the config file and its settings.

## Guides

- [Safety model](/docs/safety/): what Mulchie checks before it deletes or signals anything.
- [Running headless](/docs/headless/): systemd on Linux, launchd on macOS.
- [Troubleshooting](/docs/troubleshooting/)
- [FAQ](/docs/faq/)

<!-- VERIFY: all pages were written from README.md and docs/ARCHITECTURE.md; a later pass checks them against `mulchie --help`. -->
