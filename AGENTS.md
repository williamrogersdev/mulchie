# AGENTS.md

Notes for coding agents working in this repository. People should start with
[README.md](README.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## What Mulchie is

Mulchie keeps a machine healthy while several AI coding agents share it. It
samples CPU, memory, disk and I/O per workspace. It runs a machine-wide build
queue, and it finds stale build caches and idle agent worktrees. It reclaims
them only through a safety gate that backs things up first. It groups each
agent with what it started into a session, holds sessions to limits the user
sets (by default it only reports), keeps a per-session activity record, takes
calls from agent hooks and applies guard rules to shell commands, and reports
newer versions without installing anything. It is a supervisor, not a sandbox
and not an antivirus: say so wherever these features are described.

It ships three ways:

- the `mulchie` CLI;
- a menu bar app;
- an MCP server (`mulchie mcp`) that lets agents look at the machine and act on it.

## Layout

| Path | What it is |
| --- | --- |
| `crates/mulchie-core` | Engine, monitor, scanners, safety gate (`safety.rs`), sessions (`session.rs`), limits (`limits.rs`), memory guard (`memguard.rs`), activity record (`record.rs`), guard rules (`guard.rs`), update checks (`updates.rs`), local HTTP API (`api.rs`). No UI. |
| `crates/mulchie-cli` | The `mulchie` binary: commands, `serve`, `run`, the MCP server (`mcp.rs`, `mcp_tools.rs`), `mcp install` (`mcp_install.rs`), `hook` and `hooks install` (`hook.rs`, `hooks_install.rs`), `sessions.rs`, `diagnose.rs`. |
| `crates/mulchie-app` | Tauri desktop app. macOS only. Not a default workspace member. |
| `docs/ARCHITECTURE.md` | How the pieces fit, and the rules the code keeps. Read it before touching deletion or signals. |
| `docs/openapi.yaml` | The HTTP API. A test keeps it in step with `ROUTES` in `api.rs`. |
| `server.json` | The MCP registry entry. A test keeps its version equal to the workspace version. |
| `packaging/`, `scripts/` | Homebrew, launchd, systemd, installer and release scripts. |
| `.github/workflows` | `ci.yml` (the checks), `main.yml` (CI on `main`, then the release decision), `release.yml`, `publish.yml`, `site.yml`, `dependabot-automerge.yml`. `docs/RELEASING.md` describes them. |

## Build and test

```sh
cargo build                                   # core and CLI
cargo fmt --all --check
cargo clippy -p mulchie-core -p mulchie --all-targets -- -D warnings
cargo clippy -p mulchie-core -p mulchie --all-targets --no-default-features -- -D warnings   # without the `network` feature, as CI's Ubuntu job does
cargo check -p mulchie --target x86_64-unknown-linux-gnu --no-default-features   # Linux cross-check without a C cross-compiler
cargo test -p mulchie-core -p mulchie
cargo test -p mulchie --test mcp_smoke        # MCP over real stdio, temp HOME
cargo test -p mulchie --test discovery        # openapi.yaml and server.json drift
```

The app needs Bun and, on Linux, WebKitGTK. Leave it out unless you are
changing it.

## Shipping a change

Nothing is merged, tagged, released or deployed by hand. Do this and stop:

```sh
git push -u origin HEAD
gh pr create --fill                 # the title becomes the commit subject on main
gh pr merge --auto --squash         # merges by itself when the "CI passed" check is green
```

- Do not push to `main`, push a tag, create or publish a GitHub Release, or run `cargo publish`. The ruleset on `main` refuses direct pushes, and the workflows do the rest.
- `CI passed` is the only required check. It needs every CI job: Rust on macOS and Ubuntu, scripts and workflows, the site build. Run the commands under "Build and test" first; `shellcheck scripts/*.sh` and `actionlint` if you touched scripts or workflows.
- If a check fails, read it (`gh pr checks`, `gh run view <id> --log-failed`), push a fix and leave auto-merge on. If the branch conflicts with `main`, merge `main` into it and push.
- After the merge, `main.yml` runs CI on `main`, and `site.yml` deploys the site if `site/` or `scripts/install.sh` changed.
- To release: `scripts/bump-version.sh X.Y.Z` (or `X.Y.Z-rc.N`) on a branch, then ship that branch as above. The merge that changes the version is the release: the workflows build, create the tag, publish the GitHub Release, and update Homebrew and crates.io. Only do this when asked to release. If a release run fails, `docs/RELEASING.md` ("Each release") says how to run it again.
- A CI job you add must be listed in `needs` of the `ok` job in `.github/workflows/ci.yml`, or it does not gate merges.

## Rules

- **Tests never touch the real home.** Unit tests inject `Paths` (`Paths::under(tmp)`). Tests that run the binary:
  - set `HOME` to a fresh temp directory;
  - unset `XDG_CONFIG_HOME`, or point it inside that directory;
  - set `MULCHIE_PORT` to a free port.

  The same applies to manual checks. Do not run `sweep`, `clean`, `service install`, `mcp install` or `hooks install` against your real home; use `--dry-run` with a temp HOME.
- **A test that starts an engine writes a safe config first**: build throttling off, no limit budgets, the memory guard off (`memory_guard.action` set to `off`, and never `stop` against the real process table) and update checks off (`updates.check` set to `off`). Otherwise the engine, which holds the lock in its temp home and samples the real process table, could renice your real builds, signal real processes or reach the network. Use the existing `safe_config` and `safe_test_config` helpers.
- **Only the safety gate deletes or signals.** Every cache deletion, worktree removal and signal to another process goes through `crates/mulchie-core/src/safety.rs`. Nothing else may delete user files or signal other processes. These are the only other places that delete, signal, renice or probe a process (`docs/ARCHITECTURE.md` has the detail):
  - `git.rs` kills the process group of a child it started that timed out (git, and every helper run through `run_with_timeout` or `run_capped`);
  - `mulchie run` forwards signals to its own child;
  - `record.rs` deletes Mulchie's own record files under `<config dir>/sessions/` (retention and the size cap);
  - `engine.rs` (`restore_priorities`) and `engine_sessions.rs` (`restore_breach`) set a priority back up after lowering it;
  - the build throttle in `engine.rs` calls `lower_priority`, after re-reading the pid's start time;
  - `engine.rs` (`own_pid_alive`) and `safety.rs` (`pid_alive`) probe liveness with `kill(pid, 0)`, which sends nothing;
  - `diskmap.rs` (`prune_cache`) runs a package manager's own prune command, which deletes inside that tool's cache.

  A change to a deletion or signal rule needs a test for that rule. Do not add another exception without adding it here and to `docs/ARCHITECTURE.md`.
- **Session records** (`<config dir>/sessions/*.jsonl`) are opened only through `record::open_plain` (private to `record.rs`): it refuses symlinks, FIFOs and hard links. Read them through `GET /v1/sessions/{id}/events`, never by scanning files.
- **Plan, then execute.** Destructive API routes take the `request` objects from a plan. The MCP server keeps each plan it shows. `execute_plan` accepts only ready items from that stored plan. Do not add a one-step delete.
- **Additive API changes.** The app, the CLI and the MCP server all use the core API, so add routes and fields rather than change them. When you change routes, update `docs/openapi.yaml`.
- Docs describe only what the code does.

## Using Mulchie as an agent

Connect the MCP server:

```sh
mulchie mcp install --client claude-code
```

Other clients are `claude-desktop`, `cursor` and `codex`. Add `--read-only` to leave out every tool that deletes, signals, prunes or dismisses, and `check_updates`. A read-only server also connects with the engine's read-only token, so the engine itself refuses changes; build slots still work.

1. Call `diagnose` first. It returns CPU and memory in words with the top apps and workspaces, health, findings, reclaimable space and recommendations. Each recommendation and finding names the exact next tool call.
2. To reclaim space or act on processes, call a plan tool:
   - `plan_cache_cleanup`
   - `plan_worktree_sweep`
   - `plan_worktree_removal`
   - `plan_process_action`: one pid with a signal, a consumer group (`app:…`, `workspace:…`, `process:…`) with an action, or a session (`session`, from `list_sessions`) with an action. A session plan leaves out the agent's own process unless `include_agent` is true. Protected processes and the agent's own app and terminal come back blocked, with the reason.
3. Show the user the plan.
4. Call `execute_plan` with the `plan_id` and only the items the user approved. The result lists every backup and how to restore it. Processes are re-checked by start time.
5. Wrap heavy builds in `acquire_build_slot` and `release_build_slot`. A slot lasts at most an hour per renewal, and the server releases the slots it still holds when it exits. `mulchie slots` lists slots; `mulchie slots release <ticket>` ends one whose holder is gone. The release secret stays inside the MCP server.

Findings (`list_findings`, `mulchie://findings`) are observations from heuristics, not verdicts; never call them malware. `inspect_process` shows one process with secrets in its command line redacted. `dismiss_finding` hides one the user says is expected.

Sessions, limits, guard rules and updates:

- `list_sessions` shows each agent session (an agent and what it started) with its usage and whether its limits are enforced; `get_session` shows one, and with `events=true` its activity record (command lines redacted; events with `unbound: true` came from a caller other than the session's own client). Page backwards with `tail` and `before`.
- `limits_status` says whether budgets only report or act, which sessions are over a cap, what Mulchie paused and why it may not enforce. Limits notify by default. Its `memory_guard` field is the machine-wide memory guard: whether it closes things when memory runs low (`action: stop`) or only reports, what it would close next and what it closed.
- `check_command` asks the guard rules whether a shell command is `allow`, `warn` or `block` before you run it, with the reason. It records nothing. It is a tripwire on command text, not a sandbox: do not treat `allow` as safe. If it says `block`, do not run the command; if `warn`, tell the user first.
- `list_updates` shows the last update check. `check_updates` asks the engine to check now (it contacts GitHub and npm from the engine, and OSV.dev only if the user turned advisories on) and is refused when `updates.check` is off. Mulchie never installs updates; the commands in the findings are for the user to run.

Agent apps can also report to Mulchie through their own hooks: `mulchie hooks install --client <claude-code|codex|cursor|gemini|copilot>` (with `--dry-run` to see the exact diff) adds `mulchie hook <client> <event>` for session start, prompts, before and after shell commands, stop and session end. The hook fails open: with no engine, a slow one (300 ms) or any error it allows the action and exits 0. `mulchie hooks uninstall` removes exactly those entries; `mulchie hooks status` lists them.

The CLI equivalent is `mulchie diagnose`, then `mulchie clean`, `mulchie sweep`, `mulchie signal` or `mulchie group`, which show the plan and ask before acting. `mulchie findings`, `mulchie inspect <pid>` and `mulchie top` look; `mulchie config set` changes a setting through the running engine.
