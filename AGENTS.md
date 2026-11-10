# AGENTS.md

Notes for coding agents working in this repository. People should start with
[README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## What Mulchie is

Mulchie keeps a machine healthy while several AI coding agents share it. It
samples CPU, memory, disk and I/O per workspace. It runs a machine-wide build
queue, and it finds stale build caches and idle agent worktrees. It reclaims
them only through a safety gate that backs things up first.

It ships three ways:

- the `mulchie` CLI;
- a menu bar app;
- an MCP server (`mulchie mcp`) that lets agents look at the machine and act on it.

## Layout

| Path | What it is |
| --- | --- |
| `crates/mulchie-core` | Engine, monitor, scanners, safety gate (`safety.rs`), local HTTP API (`api.rs`). No UI. |
| `crates/mulchie-cli` | The `mulchie` binary: commands, `serve`, `run`, the MCP server (`mcp.rs`, `mcp_tools.rs`), `mcp install` (`mcp_install.rs`), `diagnose.rs`. |
| `crates/mulchie-app` | Tauri desktop app. macOS only. Not a default workspace member. |
| `docs/ARCHITECTURE.md` | How the pieces fit, and the rules the code keeps. Read it before touching deletion or signals. |
| `docs/openapi.yaml` | The HTTP API. A test keeps it in step with `ROUTES` in `api.rs`. |
| `server.json` | The MCP registry entry. A test keeps its version equal to the workspace version. |
| `packaging/`, `scripts/` | Homebrew, launchd, systemd, installer and release scripts. |

## Build and test

```sh
cargo build                                   # core and CLI
cargo fmt --all --check
cargo clippy -p mulchie-core -p mulchie --all-targets -- -D warnings
cargo test -p mulchie-core -p mulchie
cargo test -p mulchie --test mcp_smoke        # MCP over real stdio, temp HOME
cargo test -p mulchie --test discovery        # openapi.yaml and server.json drift
```

The app needs Bun and, on Linux, WebKitGTK. Leave it out unless you are
changing it.

## Rules

- **Tests never touch the real home.** Unit tests inject `Paths` (`Paths::under(tmp)`). Tests that run the binary:
  - set `HOME` to a fresh temp directory;
  - unset `XDG_CONFIG_HOME`, or point it inside that directory;
  - set `MULCHIE_PORT` to a free port.

  The same applies to manual checks. Do not run `sweep`, `clean`, `service install`, or `mcp install` against your real home; use `--dry-run` with a temp HOME.
- **Only the safety gate deletes or signals.** Every cache deletion, worktree removal and signal to another process goes through `crates/mulchie-core/src/safety.rs`. Nothing else may delete user files or signal other processes. The only other signals in the code are these:
  - `git.rs` kills a git child that timed out;
  - `mulchie run` forwards signals to its own child.

  A change to a deletion or signal rule needs a test for that rule.
- **Plan, then execute.** Destructive API routes take the `request` objects from a plan. The MCP server keeps each plan it shows. `execute_plan` accepts only ready items from that stored plan. Do not add a one-step delete.
- **Additive API changes.** The app, the CLI and the MCP server all use the core API, so add routes and fields rather than change them. When you change routes, update `docs/openapi.yaml`.
- Docs describe only what the code does.

## Using Mulchie as an agent

Connect the MCP server:

```sh
mulchie mcp install --client claude-code
```

Other clients are `claude-desktop`, `cursor` and `codex`. Add `--read-only` to leave out every tool that deletes, signals, prunes or dismisses. A read-only server also connects with the engine's read-only token, so the engine itself refuses changes; build slots still work.

1. Call `diagnose` first. It returns CPU and memory in words with the top apps and workspaces, health, findings, reclaimable space and recommendations. Each recommendation and finding names the exact next tool call.
2. To reclaim space or act on processes, call a plan tool:
   - `plan_cache_cleanup`
   - `plan_worktree_sweep`
   - `plan_worktree_removal`
   - `plan_process_action`: one pid with a signal, or a consumer group (`app:…`, `workspace:…`, `process:…`) with an action. Protected processes and the agent's own app and terminal come back blocked, with the reason.
3. Show the user the plan.
4. Call `execute_plan` with the `plan_id` and only the items the user approved. The result lists every backup and how to restore it. Processes are re-checked by start time.
5. Wrap heavy builds in `acquire_build_slot` and `release_build_slot`. A slot lasts at most an hour per renewal, and the server releases the slots it still holds when it exits. `mulchie slots` lists slots; `mulchie slots release <ticket>` ends one whose holder is gone. The release secret stays inside the MCP server.

Findings (`list_findings`, `mulchie://findings`) are observations from heuristics, not verdicts; never call them malware. `inspect_process` shows one process with secrets in its command line redacted. `dismiss_finding` hides one the user says is expected.

Agent apps can also report to Mulchie through their own hooks: `mulchie hooks install --client <claude-code|codex|cursor|gemini|copilot>` (with `--dry-run` to see the exact diff) adds `mulchie hook <client> <event>` for session start, prompts, before and after shell commands, stop and session end. The hook fails open: with no engine, a slow one (300 ms) or any error it allows the action and exits 0. `mulchie hooks uninstall` removes exactly those entries; `mulchie hooks status` lists them.

The CLI equivalent is `mulchie diagnose`, then `mulchie clean`, `mulchie sweep`, `mulchie signal` or `mulchie group`, which show the plan and ask before acting. `mulchie findings`, `mulchie inspect <pid>` and `mulchie top` look; `mulchie config set` changes a setting through the running engine.
