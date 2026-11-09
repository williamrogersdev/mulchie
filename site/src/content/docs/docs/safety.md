---
title: Safety model
description: "How Mulchie decides what it may delete or signal: facts read at run time, backups before removal, in-use protection and a localhost-only API."
---

Mulchie deletes directories and signals processes, so one rule shapes the design: **a destructive action is decided from facts read at the moment it runs, never from a cached scan.**

<!-- VERIFY: this page follows the target architecture in docs/ARCHITECTURE.md (safety.rs gate). Confirm each rule against the shipped code before release. -->

## The safety gate

Scanners only produce candidates. Every delete, worktree removal and signal goes through one gate that re-reads the facts for each candidate and returns an outcome per item: `done`, or `skipped` with a reason.

Every executed or skipped action is appended to `~/.config/mulchie/actions.jsonl`.

## Deleting a build cache

A cache is deleted only if all of these hold now:

- The path is canonical, is not a symlink, and is under a configured scan root. Xcode DerivedData is the one fixed exception, and only when `include_xcode` is on.
- It is a recognised cache. A `target/` directory must contain `CACHEDIR.TAG`. `node_modules` counts only when `include_node_modules` is on.
- Nothing is using it: no process you own has a working directory at or under the project.
- When you asked for stale caches, staleness is checked again at that moment.

## Removing a worktree

- The request names the worktree path and the `HEAD` commit you saw. If `HEAD` moved, the worktree is skipped.
- Git status, detached state, idle time and busy state are read at that moment.
- A backup comes first, and removal goes ahead only if every backup step worked:
  - uncommitted changes are saved as a patch in the backups directory;
  - gitignored files that are not known caches, such as `.env`, local databases and notes, go into a `tar.gz` beside the patch. If they exceed `backup_ignored_max_mb` the worktree is skipped instead;
  - a detached `HEAD` gets a new branch `backup/<name>-<shortsha>`. An existing branch is never moved.
- Branches are never deleted.
- A worktree that is missing from disk is pruned only when git reports it prunable and it is an agent worktree.

## Sending a signal

- The process must be owned by you. Pid 1 and Mulchie's own processes (engine, `mcp`, `run`) are refused.
- The pid's start time is compared with the snapshot just before the signal, so a reused pid is not hit.

## Plan, then execute

Sweep and clean show a plan first. The `GET` endpoint or `--dry-run` returns what would happen, and the follow-up request takes the items you approved. Automatic sweeps and reclaims build and run a plan in one step through the same gate.

## Git calls

Mulchie runs git with fixed arguments (`-c core.fsmonitor=false -c core.hooksPath=/dev/null`), a 30 second timeout and at most 8 repos at a time. Scanning a repo never runs code from that repo.

## The local API

- Listens on `127.0.0.1` only.
- Every route except `GET /v1/health` needs the bearer token.
- Requests with an `Origin` header are refused, so a web page cannot call it.
- The token file is created with mode `0600` in a `0700` directory. A token file with looser permissions or another owner is refused.

See the [HTTP API](/docs/http-api/) for limits.

## What Mulchie will not do

- Delete anything outside a configured scan root.
- Touch a project while a process runs in it.
- Delete a branch.
- Signal another user's process.
