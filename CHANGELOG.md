# Changelog

All notable changes to Mulchie are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.3.0] - Unreleased

### Added

- `mulchie diagnose`: one prioritised picture of the machine. It covers health and why, what uses CPU and memory by app and workspace, the build queue, reclaimable space, findings, engine problems and recommendations. Each recommendation gives its reason, expected gain, risk and the next command. It changes nothing.
- Findings: Mulchie's watch reports observations about processes and about its own setup, on the scan loop. The kinds are `orphaned_process`, `missing_working_directory`, `executable_changed`, `stale_process`, `stopped_process`, `runaway_cpu`, `growing_memory`, `exposed_listener`, `temp_executable`, `hidden_dir_executable`, `system_name_mismatch`, `unsigned_executable`, `quarantined_executable`, `insecure_permissions`, `api_exposed`, `engine_lock_not_held` and `config_not_loaded`. They come from heuristics, are not malware detection, and each says why it was raised. `mulchie findings` lists them, filtered with `--severity`, `--kind` and `--all`. `mulchie findings dismiss <id>` hides one for a number of days, and `undismiss` shows it again. The settings are under `watch` in the config.
- `mulchie top` groups processes into apps (with their helper processes), agent workspaces and other programs, each with a key. `--processes` lists single processes, as before.
- `mulchie inspect <pid>`: everything known about one process, with secrets in its command line redacted.
- `mulchie signal <pid> <signal>`: signals one of your processes, and with `--tree` its descendants. It shows a plan, asks, and re-checks each process by its start time.
- `mulchie group <key> <action>`: quits, force-quits, pauses, resumes, lowers the priority of or stops the builds of an app, workspace or program as a whole, and restarts an app where the plan offers it. It shows a plan and asks first.
- `mulchie config set <key> <value>`: changes one setting. With an engine running the change applies at once. Out-of-range values are refused.
- MCP server: `mulchie mcp install --client claude-code|claude-desktop|cursor|codex` connects it to an agent app, `mulchie mcp uninstall` removes it, and `mulchie mcp config` prints the JSON to paste by hand. Existing config files are backed up and other entries are kept; `--dry-run` shows the change.
- MCP `--read-only` (or `MULCHIE_MCP_READ_ONLY=1`): leaves out every tool that deletes, signals, prunes or dismisses. Build slots still work.
- MCP tools: `diagnose`, `get_status`, `can_i_build`, `top_processes`, `list_workspaces`, `get_history`, `disk_breakdown`, `reclaim_candidates`, `list_findings`, `inspect_process`, `plan_cache_cleanup`, `plan_worktree_removal`, `plan_worktree_sweep`, `plan_process_action`, `execute_plan`, `prune_cache`, `dismiss_finding`, `acquire_build_slot` and `release_build_slot`. Each tool is marked read-only or destructive.
- Plan, then execute, everywhere. `clean`, `sweep`, `signal` and `group` show a plan and delete or signal exactly that plan after you confirm. `--dry-run` stops after the plan, `--yes` skips the question, and `--json` prints JSON. Over MCP, `execute_plan` accepts only ready items from a plan the server showed, once, within 30 minutes.
- A read-only API token (`token.read` in the config directory, printed by `mulchie token --read-only`). It is accepted for `GET` routes, the plan routes and time-limited build slots. Every other route answers 403.
- `mulchie run -- <command>`: waits for a build slot, runs the command with a fair `CARGO_BUILD_JOBS` (and `MAKEFLAGS` unless already set), and releases the slot however the command ends. It forwards SIGINT and SIGTERM and exits with the command's status. `--label` names the job in the queue. Without an engine it says so and runs the command unqueued.
- Build slots over MCP and the API are leases with a time limit (`ttl_secs`, default 900 over MCP). A lease must be renewed with its ticket or it expires, so a crashed agent cannot block the queue. Releasing or renewing a ticket needs the secret returned when it was created.
- `mulchie doctor`: checks the config, token and directory permissions, the engine, git, `PATH`, the background service and how to add the MCP server. It exits 1 if any check fails.
- `mulchie service install|uninstall|status`: runs the engine at login as a launchd agent on macOS or a systemd user unit on Linux.
- `mulchie completions <shell>` for bash, zsh, fish, elvish and powershell, and `mulchie --version`.
- `mulchie token`: prints the path of the API token. The token itself is printed only with `--show`.
- Install script (`install.sh`) that verifies the tarball's SHA-256 before installing to `~/.local/bin`, and Homebrew formula and cask templates.
- Menu bar app: grouped CPU and memory usage, a "Needs attention" list of findings, process details, and an Agent API page with the connect commands.
- Linux support for the core, CLI, engine and MCP server.
- Public site with documentation, and an OpenAPI description of the local API in `docs/openapi.yaml`.

### Changed

- Command-line parsing uses clap: every command has `--help` with examples, flags are strict, and unknown flags are errors.
- `clean` and `sweep` no longer delete in one step. They show what they would do and, briefly, what they would skip and why.
- Worktree removal backs up first and removes only if every backup step worked. Uncommitted changes become a patch that is checked to apply cleanly, gitignored files that are not caches go into a `tar.gz`, and a detached HEAD gets a `backup/<name>-<shortsha>` branch. Each backup set is a directory under the config directory's `backups/` with a `manifest.json` that says how to restore it.
- A worktree is skipped, not removed, when it has a checked-out submodule, holds another git repository, has local edits that git hides (skip-worktree or assume-unchanged), is locked, or its HEAD moved since the plan.
- The API refuses requests with a browser `Origin` header or a `Host` other than `127.0.0.1` or `localhost`, and refuses a token file that other users can read. Each request is limited in size and time.
- Exactly one engine runs per user. The app and `mulchie serve` share a lock; the one that loses becomes a client of the other.
- The config file is checked on load and on every change. An invalid file is reported, and Mulchie runs on defaults with automatic reclaim and sweep off until it is fixed. A later save moves the invalid file aside instead of overwriting it.
- Failures to write the action journal are reported instead of dropped.

### Removed

- The MCP tools `clean_caches`, `remove_worktrees`, `sweep_worktrees`, `workspace_action` and `process_action`. Use `plan_cache_cleanup`, `plan_worktree_removal`, `plan_worktree_sweep` and `plan_process_action`, then `execute_plan`.
