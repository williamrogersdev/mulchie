---
title: Configuration
description: "Where Mulchie keeps its config file, how to change a setting with mulchie config set, every setting and its default, and the files Mulchie writes."
---

Use this page to change how Mulchie samples, throttles, reclaims, watches, limits sessions, guards commands and checks for updates.

Settings live in `config.json` in Mulchie's config directory, `~/.config/mulchie/` (or `$XDG_CONFIG_HOME/mulchie/` when that is set). The app also edits them under **Settings**.

## Change a setting

```sh
mulchie config
mulchie config set stale_hours 12
mulchie config set scan_roots '["/Users/me/code"]'
mulchie config set watch.runaway_minutes 45
```

`mulchie config` prints the path of the file, any config error, and the current settings. `mulchie config set <key> <value>` changes one setting. The key is dotted, and the value is JSON: `3`, `true`, `'"text"'` (or just `text`, since a value that is not JSON is taken as a string), `'["a", "b"]'` or an object. `null` clears an optional setting such as `limits.default`.

When an engine is running, `config set` goes through it, so the change applies without a restart. Otherwise Mulchie edits the file directly and says so.

## How the file is checked

The file has a `version` field. When Mulchie loads it, it checks that every value is in range. Values are refused, not silently changed. For example `stale_hours` must be at least 1, `scan_interval_mins` must be between 1 and 1440, and `auto_reclaim_target_gb` must be greater than `low_disk_gb`.

If the file cannot be parsed, or a value is out of range, Mulchie reports it (`mulchie config`, `mulchie doctor`, a `config_not_loaded` [finding](/docs/watch/)) and runs on the defaults with `auto_reclaim` and `auto_sweep_worktrees` off. It does not overwrite your file in place. If a later save finds an invalid file, it first renames it to `config.json.invalid-<unix time>-<random>`. If the file changed on disk after the engine loaded it (an editor, another engine), a save is refused with `config_changed` and the engine takes the file's content, so you read it again and reapply your change. Over the [HTTP API](/docs/http-api/#config), `GET /v1/config` returns a `revision`, and `PUT` or `PATCH` with that revision write only if nothing changed the file since. Only the engine that holds the engine lock writes the config. A key Mulchie does not have is refused, naming its path.

Saves are atomic: Mulchie writes a temporary file and renames it.

## Environment

| Variable | Effect |
| --- | --- |
| `MULCHIE_PORT` | Overrides the API port for that process only. It is never written to the file. |
| `MULCHIE_MCP_READ_ONLY` | Set to `1` to start `mulchie mcp` in read-only mode. |
| `XDG_CONFIG_HOME` | When set to an absolute path, the config directory is `$XDG_CONFIG_HOME/mulchie`. |

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `scan_roots` | existing folders among `~/Developer`, `~/Projects`, `~/code`, `~/src`, `~/dev`, `~/repos`, `~/work`; otherwise `~` | Where Mulchie looks for repos and caches. Only caches under these roots can be deleted. Paths must be absolute. |
| `stale_hours` | 24 | A build cache is stale after this many hours without use. |
| `worktree_target_stale_hours` | 48 | The same window for caches inside worktrees. |
| `worktree_idle_days` | 10 | A worktree is idle after this many days without file changes or commits. |
| `auto_sweep_worktrees` | `false` | Sweep idle agent worktrees automatically. |
| `auto_reclaim` | `false` | Reclaim stale caches automatically when disk is low. |
| `auto_reclaim_target_gb` | 60 | Free space to reach when reclaiming automatically. Must be greater than `low_disk_gb`. |
| `low_disk_gb` | 25 | Free space below which the disk counts as low. |
| `alert_below_gb` | 50 | Send a notification when free space drops below this. |
| `backup_ignored_max_mb` | 200 | Gitignored files in a worktree that are not caches are archived before removal. Above this size the worktree is skipped instead. |
| `include_node_modules` | `false` | Treat `node_modules` as a reclaimable cache. |
| `include_xcode` | `true` on macOS, `false` elsewhere | Treat Xcode DerivedData as a reclaimable cache. |
| `throttle_builds` | `true` | Lower the priority of builds under pressure. |
| `throttle_cpu_pct` | 92 | CPU use above which builds are throttled. |
| `throttle_mem_free_pct` | 8 | Free memory below which builds are throttled. |
| `build_nice` | 10 | Nice value applied to throttled builds, 0 to 20. |
| `max_heavy_jobs` | 0 | Build slots in the queue. 0 picks one slot per four CPU cores, at least 2. |
| `scan_interval_mins` | 30 | How often caches and the disk map are rescanned. |
| `api_port` | 47474 | Port for the local API. |

Automatic sweeps and reclaims build a plan and run it in one step, through the same [safety checks](/docs/safety/) as manual ones.

## Watch

The `watch` settings control [findings](/docs/watch/). Detectors only report. Nothing here makes Mulchie act.

| Setting | Default | Meaning |
| --- | --- | --- |
| `watch.enabled` | `true` | Run the watch. |
| `watch.interval_secs` | 60 | Seconds between runs, 10 to 3600. |
| `watch.stale_process_hours` | 72 | An agent or build process older than this and idle is reported as stale. |
| `watch.stale_idle_cpu_pct` | 1 | Idle means average CPU below this percent of one core. |
| `watch.stopped_hours` | 6 | A paused process is reported after this long. |
| `watch.runaway_minutes` | 30 | The window for runaway CPU and growing memory, 5 to 120. |
| `watch.runaway_cpu_pct` | 90 | Average CPU, as a percent of one core, that counts as runaway. |
| `watch.memory_growth_mb` | 1024 | Memory growth over the window that is reported. |
| `watch.check_listeners` | `true` | Look for listening network sockets. |
| `watch.check_signatures` | `true` | Check macOS code signatures and quarantine flags of running programs. |
| `watch.allow_exes` | empty | Absolute paths of programs that are never reported. |
| `watch.allow_ids` | empty | Finding ids that are never reported. |
| `watch.dismissed` | empty | Findings hidden for a while. Set by `mulchie findings dismiss`. |

## Sessions

The `sessions` and `agents` settings control [sessions and the activity record](/docs/sessions/).

| Setting | Default | Meaning |
| --- | --- | --- |
| `sessions.enabled` | `true` | Track sessions. Off means no sessions, no records, no limits and no hook mapping. |
| `sessions.retain_days` | 14 | Records of ended sessions are deleted after this many days, 1 to 365. |
| `sessions.max_record_mb` | 4 | One session's record stops growing at this size, 1 to 100. |
| `sessions.max_events` | 20000 | ... or at this many events, 100 to 1000000. |
| `sessions.max_total_mb` | 200 | All records together. The oldest records of ended sessions go first. 10 to 10000. |
| `sessions.process_events_per_minute` | 60 | Process-start events written per session per minute. The rest are summed in one event. 1 to 10000. |
| `agents.extra` | empty | File names (no `/`) of agent programs to treat as session roots, on top of the built-in list. At most 64. |

## Limits

`limits` sets budgets for sessions. See [Sessions and limits](/docs/sessions/#limits) for what each action does and where limits do not apply. With no budget Mulchie only reports.

| Setting | Default | Meaning |
| --- | --- | --- |
| `limits.default` | none | The budget for every session. |
| `limits.workspaces` | empty | A list of `{"path": "/abs/path", "budget": {...}}`. The entry with the longest matching path replaces the default whole. At most 256. |
| `limits.hog_share_pct` | 50 | A session using at least this share of the machine's CPU or memory while health is strained or critical becomes a `session_hog` finding. 10 to 100. |
| `limits.hog_sustain_secs` | 120 | ... for this long. 10 to 3600. |

A budget has any of the caps `cpu_cores`, `memory_mb`, `processes`, `write_mb_per_hour` and `runtime_hours`, each `{"max": N, "sustain_secs": 60, "action": "notify"}`. `max` must be positive. `sustain_secs` is 0 to 86400 and defaults to 60. `action` is `notify` (the default), `deprioritize`, `pause` or `stop`. The budget also has:

| Budget setting | Default | Meaning |
| --- | --- | --- |
| `resume_after_secs` | 300 | A `pause` ends after this long, 10 to 86400. |
| `include_agent` | `false` | Let `deprioritize`, `pause` and `stop` reach the agent's own process too. |
| `stop_grace_secs` | 10 | Between SIGTERM and SIGKILL for `stop`, 1 to 300. |

## Memory guard

`memory_guard` decides what happens when the whole machine runs low on memory. See [the memory guard](/docs/sessions/#the-memory-guard).

| Setting | Default | Meaning |
| --- | --- | --- |
| `memory_guard.action` | `notify` | `off`, `notify` (a `memory_low` finding and a notification) or `stop` (close the biggest closable unit). |
| `memory_guard.scope` | `sessions` | `sessions`: what agent sessions started. `all`: also any other unprotected app, workspace or program of yours. |
| `memory_guard.free_pct` | 5 | Memory is low when less than this percent is available. 1 to 50. |
| `memory_guard.on_pressure` | `true` | Critical memory pressure reported by the kernel also counts as low. |
| `memory_guard.sustain_secs` | 15 | How long memory must stay low, or a unit over `unit_max_mb`. 0 to 3600. |
| `memory_guard.min_mb` | 300 | Nothing using less is closed for low memory. 16 to 1048576. |
| `memory_guard.unit_max_mb` | 0 | Close a unit that stays over this much memory even when the machine is not low. 0 is off; otherwise 64 to 1048576. |
| `memory_guard.cooldown_secs` | 30 | At least this long between two closings. 10 to 3600. |
| `memory_guard.stop_grace_secs` | 5 | Between SIGTERM and SIGKILL. 1 to 300. |
| `memory_guard.include_agent` | `false` | Close a session's agent process together with what it started. |
| `memory_guard.never` | empty | Process names never closed. At most 256. |

## Guard rules

The `guard` settings control the [guard rules](/docs/hooks/#guard-rules) that agent hooks and `mulchie guard check` apply to shell commands.

| Setting | Default | Meaning |
| --- | --- | --- |
| `guard.enabled` | `true` | Off: no rule is evaluated and every command is allowed. |
| `guard.rules.recursive_delete` | `warn` | Each rule is `off`, `warn` or `block`. |
| `guard.rules.pipe_to_shell` | `warn` | |
| `guard.rules.sudo` | `warn` | |
| `guard.rules.force_push` | `warn` | |
| `guard.rules.disable_mulchie` | `warn` | |
| `guard.rules.credential_read` | `warn` | |
| `guard.rules.startup_write` | `warn` | |
| `guard.protected_branches` | `["main", "master"]` | Branches the `force_push` rule protects. At most 64. |
| `guard.build_advice` | `true` | While the machine is busy, advise running a recognised heavy build as `mulchie run`. |

## Updates

The `updates` settings control [update checks](/docs/updates/). Nothing here installs anything.

| Setting | Default | Meaning |
| --- | --- | --- |
| `updates.check` | `on` | `off` makes no network request and runs no version command or `brew`. |
| `updates.mulchie` | `true` | Check Mulchie's latest GitHub release. |
| `updates.agents` | `true` | Check installed agent tools against the npm registry. |
| `updates.homebrew` | `true` | Run `brew outdated` on Homebrew's local data, when the local core tap exists. |
| `updates.advisories` | `false` | Send package names and versions from workspace lockfiles to OSV.dev. Off because it tells a third party what your projects depend on. |
| `updates.interval_hours` | 24 | How long a result is reused, 6 to 720. |

## Files Mulchie writes

All of these are in the config directory.

| Path | Contents |
| --- | --- |
| `config.json` | Settings. |
| `token` | API bearer token, mode `0600`. |
| `token.read` | Read-only API bearer token, mode `0600`. |
| `backups/` | One folder per removed worktree, with a patch, an archive of ignored files and a `manifest.json` that says how to restore. |
| `actions.jsonl` | Journal of every executed or skipped action. Rotated at 5 MB to `actions.jsonl.1`. |
| `engine.lock` | Makes sure one engine runs per user. |
| `sessions/` | One `<id>.jsonl` [activity record](/docs/sessions/#the-activity-record) per session (mode `0600`, in a `0700` folder), and `paused.json`, the processes Mulchie paused for a limit and has not resumed. |
| `updates.json` | The last [update check](/docs/updates/), so a restart shows it. |
| `hook.log` | Why a [hook](/docs/hooks/) allowed an action without the engine, at most one line a minute. |
