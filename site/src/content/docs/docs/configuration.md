---
title: Configuration
description: "Where Mulchie keeps its config file, how to change a setting with mulchie config set, what each setting does and its default, and the files Mulchie writes."
---

Use this page to change how Mulchie samples, throttles, reclaims and watches.

Settings live in `config.json` in Mulchie's config directory, `~/.config/mulchie/` (or `$XDG_CONFIG_HOME/mulchie/` when that is set). The app also edits them under **Settings**.

## Change a setting

```sh
mulchie config
mulchie config set stale_hours 12
mulchie config set scan_roots '["/Users/me/code"]'
mulchie config set watch.runaway_minutes 45
```

`mulchie config` prints the path of the file, any config error, and the current settings. `mulchie config set <key> <value>` changes one setting. The key is dotted, and the value is TOML: `3`, `true`, `'"text"'` or `'["a", "b"]'`.

When an engine is running, `config set` goes through it, so the change applies without a restart. Otherwise Mulchie edits the file directly and says so.

## How the file is checked

The file has a `version` field. When Mulchie loads it, it checks that every value is in range. Values are refused, not silently changed. For example `stale_hours` must be at least 1, `scan_interval_mins` must be between 1 and 1440, and `auto_reclaim_target_gb` must be greater than `low_disk_gb`.

If the file cannot be parsed, or a value is out of range, Mulchie reports it (`mulchie config`, `mulchie doctor`, a `config_not_loaded` [finding](/docs/watch/)) and runs on the defaults with `auto_reclaim` and `auto_sweep_worktrees` off. It does not overwrite your file in place. If a later save finds an invalid file, it first renames it to `config.json.invalid-<unix time>-<random>`. If the file changed on disk after the engine loaded it, a save is refused and the engine uses the file's content.

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
