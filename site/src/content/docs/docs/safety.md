---
title: Safety model
description: "How Mulchie decides what it may delete or signal: plans you confirm, facts read at run time, backups before removal, protected processes, a local-only API."
---

Read this page to know what Mulchie will and will not do to your files and processes, and what to check if you let agents drive it.

Mulchie deletes directories and signals processes, so one rule shapes the design: **a destructive action is decided from facts read at the moment it runs, never from a cached scan.**

## The safety gate

Scanners only produce candidates. Every cache deletion, worktree removal and signal goes through one module, the safety gate. It re-reads the facts for each candidate and returns an outcome per item: `done`, or `skipped` with a reason. When a check cannot be completed, for example git times out or a backup step fails, the answer is skipped, never proceed.

Two exceptions are not deletions of your files: `mulchie run` forwards signals to its own command, and Mulchie ends a git command of its own that timed out.

Every executed or skipped action is appended to `actions.jsonl` in the [config directory](/docs/configuration/#files-mulchie-writes).

## Plan, then execute

Nothing is deleted or signalled in one step by a person or an agent.

- On the CLI, `clean`, `sweep`, `signal` and `group` show a plan, ask, and then act on exactly that plan. `--dry-run` stops after the plan.
- Over the HTTP API, you get a plan first and post back the items you approve.
- Over MCP, `execute_plan` accepts only ready items from a plan the server showed, once, within 30 minutes. An agent cannot delete anything it was not first shown in a plan.

Automatic sweeps and reclaims, which are off by default (`auto_sweep_worktrees`, `auto_reclaim`), build a plan and run it in one step through the same gate.

## Deleting a build cache

A cache is deleted only if all of these hold at that moment:

- The path is absolute, canonical, is not a symlink, and is under a configured scan root. Xcode DerivedData is the one fixed exception, and only when `include_xcode` is on.
- It is a recognised cache. A Rust `target/` directory must contain `CACHEDIR.TAG`. `node_modules` counts only when `include_node_modules` is on.
- Nothing is using it: no process you own has a working directory at or under the project.
- When you asked for stale caches, staleness is checked again at that moment.

Build caches are not backed up. The next build recreates them.

## Removing a worktree

- The plan names the worktree path and the `HEAD` commit it saw. If `HEAD` moved, the worktree is skipped.
- Git status, detached state, idle time and busy state are read at that moment.
- A sweep removes only agent worktrees: those under a `.claude/worktrees/` folder, or with a name that starts with `agent-`. It skips a worktree that is not idle past `worktree_idle_days`.
- A backup comes first, and removal goes ahead only if every backup step worked:
  - uncommitted changes are saved as a patch, which is checked to apply cleanly to `HEAD`;
  - gitignored files that are not known caches, such as `.env`, local databases and notes, go into a `tar.gz`. If they exceed `backup_ignored_max_mb` the worktree is skipped instead;
  - a detached `HEAD` gets a new branch `backup/<name>-<shortsha>`. An existing branch is never moved.
- Branches are never deleted.
- An idle worktree with uncommitted changes can be swept, because the patch is made first. Removing a worktree by name skips one with uncommitted changes unless the request sets `allow_dirty`.
- These are skipped, not removed, because a patch could not hold what they have: a checked-out submodule, another git repository inside the worktree, local edits that git hides (skip-worktree or assume-unchanged files), and a worktree locked by git.
- If anything in the worktree changes while the backup is taken, the removal is skipped.
- A worktree that is missing from disk is pruned only when git reports it prunable and it is an agent worktree.

Each backup set is a folder in `backups/` named `<repo>-<name>-<unix time>-<random>`. It holds `changes.patch`, `ignored.tar.gz` when there were ignored files, and `manifest.json`, which records the worktree, its `HEAD` and how to restore it. The result of every removal lists its backups.

## Sending a signal

- The process must be owned by you.
- Never signalled: pid 0 and 1, Mulchie's own processes (the engine, `mcp`, `run`) and the processes that started Mulchie, operating-system programs under system folders, parts of the desktop, and on Linux the user services started by `systemd --user`.
- A plan for a process or a group also refuses this terminal and its parents, and the app the agent runs in.
- The process's start time is compared with the plan just before the signal, so a process id that was reused is not hit.

`mulchie top` marks protected groups with `*`.

## What the watch does

The [watch](/docs/watch/) only reads. Its findings are observations, not verdicts, and not malware detection. It never signals or deletes. Acting on a finding is an ordinary plan that you confirm.

## Git calls

Mulchie runs git with fixed arguments (`-c core.fsmonitor=false -c core.hooksPath=/dev/null`), a 30 second timeout and at most 8 repos at a time. Scanning a repo never runs code from that repo.

## The local API

- Listens on `127.0.0.1` only.
- Every route except `GET /v1/health` needs a bearer token.
- Requests with an `Origin` header are refused, so a web page cannot call it.
- The `Host` header must be `127.0.0.1:<port>` or `localhost:<port>`.
- The token files are created with mode `0600` in a `0700` directory. A token file with looser permissions or another owner is refused.
- A second token, `token.read`, can read and plan but not change anything. The MCP server uses it in `--read-only` mode, so the engine refuses changes even if a tool were called.

Command lines, build-queue labels and process details shown by the API, the CLI and MCP have secrets redacted. See the [HTTP API](/docs/http-api/) for limits.

## What Mulchie will not do

- Delete anything outside a configured scan root, apart from Xcode DerivedData when enabled.
- Touch a project while a process runs in it.
- Delete a branch.
- Signal another user's process.
- Delete or signal because of a finding alone.
