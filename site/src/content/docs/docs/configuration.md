---
title: Configuration
description: "Where Mulchie keeps its config file, what each setting does, and how the API port, scan roots and reclaim thresholds are set."
---

Settings live in `~/.config/mulchie/config.json`. You can also change them in the dashboard under **Settings**. Run `mulchie config` to print the path and the current values.

The file has a `version` field. On load, Mulchie checks that values are in range. If the file cannot be parsed or a value is invalid, Mulchie reports it in the state and in `mulchie doctor`, and never overwrites the file when you save other settings. Saves are atomic.

<!-- VERIFY: validation and non-overwrite behaviour are described in docs/ARCHITECTURE.md as the target; confirm they ship. -->

## Environment

| Variable | Effect |
| --- | --- |
| `MULCHIE_PORT` | Overrides the API port for that process only. It does not change the file. |

## Settings

<!-- VERIFY: names, defaults and units below come from crates/mulchie-core/src/config.rs at the time of writing and that file is being reworked. Re-check each row. -->

| Setting | Default | Meaning |
| --- | --- | --- |
| `scan_roots` | existing folders among `~/Developer`, `~/Projects`, `~/code`, `~/src`, `~/dev`, `~/repos`, `~/work`; otherwise `~` | Where Mulchie looks for repos and caches. Only caches under these roots can be deleted. |
| `stale_hours` | 24 | A build cache is stale after this long without use. |
| `worktree_target_stale_hours` | 48 | The same window for caches inside worktrees. |
| `worktree_idle_days` | 10 | A worktree is idle after this many days without changes or commits. |
| `auto_sweep_worktrees` | `false` | Sweep idle agent worktrees automatically. |
| `auto_reclaim` | `false` | Reclaim stale caches automatically when disk is low. |
| `auto_reclaim_target_gb` | 60 | Free space to reach when reclaiming automatically. |
| `low_disk_gb` | 25 | Free space below which Mulchie treats the disk as low. |
| `alert_below_gb` | 50 | Send a notification when free space drops below this. |
| `include_node_modules` | `false` | Treat `node_modules` as a reclaimable cache. |
| `include_xcode` | `true` on macOS | Treat Xcode DerivedData as a reclaimable cache. |
| `throttle_builds` | `true` | Lower the priority of builds under pressure. |
| `throttle_cpu_pct` | 92 | CPU use above which builds are throttled. |
| `throttle_mem_free_pct` | 8 | Free memory below which builds are throttled. |
| `build_nice` | 10 | Nice value applied to throttled builds. |
| `max_heavy_jobs` | 0 | Build slots in the queue. 0 lets Mulchie choose. |
| `scan_interval_mins` | 30 | How often caches and the disk map are rescanned. |
| `api_port` | 47474 | Port for the local API. |

Automatic sweeps and reclaims build a plan and run it in one step, through the same [safety checks](/docs/safety/) as manual ones.

## Files Mulchie writes

| Path | Contents |
| --- | --- |
| `~/.config/mulchie/config.json` | Settings. |
| `~/.config/mulchie/token` | API bearer token, mode `0600`. |
| `~/.config/mulchie/backups/` | Patches and archives saved before a worktree is removed. |
| `~/.config/mulchie/actions.jsonl` | Journal of every executed or skipped action, rotated at 5 MB. |
| `~/.config/mulchie/engine.lock` | Makes sure one engine runs per user. |
