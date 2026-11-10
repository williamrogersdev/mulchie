---
title: Findings
description: "What Mulchie's watch reports about processes and its own setup, what each kind of finding means in plain words, and how to dismiss one you know is expected."
---

Use findings to spot processes that look left over, runaway or out of place, and problems with Mulchie's own setup. You can list them, read why each was raised, and hide the ones you know are expected.

## What findings are, and are not

Findings are observations made by simple heuristics: how long a process has run, how much CPU it uses, where its program file lives, which network addresses it listens on. Each one says what was seen and why it may matter.

A finding is not a verdict. **None of them means malware was found.** Mulchie is not an antivirus. The watch has no signatures, no file hashes and no lookups against any list, and it sends nothing anywhere. A finding that a process runs from a temporary folder is a reason to check that you know what the process is, nothing more. Most turn out to be a dev server or an updated app.

The watch only looks at processes owned by you. It never acts on its own. Acting on a finding, for example quitting a process, goes through a plan that you confirm. See the [safety model](/docs/safety/).

## List findings

```sh
mulchie findings
mulchie findings --severity medium
mulchie findings --kind runaway_cpu --json
```

Severity is `info`, `low`, `medium` or `high`, most severe first. Each finding shows a one-sentence title, why it was raised, the evidence (process id, program path, working directory and the like), and next commands to try, such as `mulchie inspect <pid>` or a plan to quit the process. Command lines in the evidence have secrets redacted.

Agents see the same findings through the MCP tools `list_findings` and `diagnose`. The dashboard lists them under **Needs attention**.

With an engine, the watch runs on the scan loop every `watch.interval_secs` seconds (default 60). Without one, `mulchie findings` looks at the processes once, and findings that need a time window (stale, runaway, growing memory, stopped) cannot appear because there is no history.

## Dismiss a finding

When you know a finding is expected, hide it:

```sh
mulchie findings dismiss 'runaway_cpu:4242:1700000000' --days 7
mulchie findings undismiss 'runaway_cpu:4242:1700000000'
mulchie findings --all
```

The id is printed with each finding. For a process it has the form `<kind>:<pid>:<start time>`, so it stops applying when the process ends. A dismissal lasts `--days` days (default 30) and is saved in the config under `watch.dismissed`. `--all` includes dismissed findings.

To never report a program, add its absolute path to `watch.allow_exes`. To never report one finding id, add it to `watch.allow_ids`. See [Configuration](/docs/configuration/#watch).

## Kinds about processes

### Left over

| Kind | What was seen | Severity |
| --- | --- | --- |
| `orphaned_process` | An agent, build or runtime process whose parent exited, still running from a workspace. Nothing is watching it, so it may be a dev server or build left behind that uses CPU, memory or a port. | low; medium once older than `watch.stale_process_hours` |
| `missing_working_directory` | A process whose working directory no longer exists, often a removed worktree. | medium |
| `stale_process` | An agent or build process older than `watch.stale_process_hours` (default 72) that has been idle: under `watch.stale_idle_cpu_pct` average CPU (default 1) over at least 30 minutes. | low |
| `stopped_process` | A process paused with SIGSTOP for longer than `watch.stopped_hours` (default 6). It keeps its memory, locks and ports. | low |
| `executable_changed` | A process whose program file was deleted or modified after the process started, usually by an update or rebuild. It may be running an older version. Restart it. | low; medium if the file is in a temporary folder |

### Heavy use

| Kind | What was seen | Severity |
| --- | --- | --- |
| `runaway_cpu` | A process that averaged at least `watch.runaway_cpu_pct` of one core (default 90) over `watch.runaway_minutes` (default 30). It is either a long job or stuck in a loop. | medium |
| `growing_memory` | A process whose memory grew by at least `watch.memory_growth_mb` (default 1024) over the same window and kept rising. This is the shape of a leak. | medium |

### Exposed or oddly placed

| Kind | What was seen | Severity |
| --- | --- | --- |
| `exposed_listener` | A process listening on all network interfaces or another non-loopback address, so it can be reached from the same network. A development server usually needs only `127.0.0.1`. Needs `watch.check_listeners`. | medium for a process in a workspace; info for an app; otherwise low |
| `temp_executable` | A process whose program file is in a temporary or downloads folder: something started from a download or by another program. | low if started by a package runner such as `npx`; medium; high if it also listens beyond this machine, is quarantined, or is unsigned or ad-hoc signed |
| `hidden_dir_executable` | A process started from a hidden folder inside your code, other than ordinary ones such as `.venv`, `.cache` or `.cargo`. | medium |
| `system_name_mismatch` | A process that uses the name of an operating-system program but runs from somewhere other than a system folder. | high |
| `unsigned_executable` | A macOS app that is not code-signed, or only ad-hoc signed, and was not built in your workspaces. macOS cannot say who built it. Needs `watch.check_signatures`. | low; medium if downloaded |
| `quarantined_executable` | A loose command-line program that still carries the macOS download quarantine flag and is running. Needs `watch.check_signatures`. | low; medium if unsigned |

The last two kinds are macOS only. This group is where people most often see a false alarm: an unsigned app you built yourself, or a tool you downloaded on purpose, will show up here.

## Kinds about Mulchie's own setup

| Kind | What was seen | Severity |
| --- | --- | --- |
| `insecure_permissions` | Mulchie's config directory or a token file can be read by other users. Run `chmod 700` on the directory or `chmod 600` on the token file. | high |
| `api_exposed` | Mulchie's API port is listening beyond this machine. It should listen on `127.0.0.1` only, so another program may have taken the port. | high |
| `engine_lock_not_held` | Another Mulchie engine is in charge. Only that one reclaims, sweeps and throttles. Expected when the app and `mulchie serve` both run. | medium |
| `config_not_loaded` | The config file was not used because it could not be parsed or a value is out of range. Mulchie runs on defaults with automatic reclaim and sweep off until you fix it. | medium |

## Kinds about sessions and updates

These come from [sessions and limits](/docs/sessions/) and [update checks](/docs/updates/), not from the process watch, and they appear in the same list.

| Kind | What was seen | Severity |
| --- | --- | --- |
| `limit_exceeded` | An agent session stayed over a cap of its budget for the sustain time. The title says which cap, the value and what Mulchie did. | medium; high when the action is `pause` or `stop` |
| `session_hog` | An agent session uses at least `limits.hog_share_pct` of the machine's CPU or memory while health is strained or critical. Only a report; no budget is needed. | medium |
| `paused_list_unreadable` | The file of processes Mulchie paused for a limit could not be read at start, so processes paused by an earlier engine may still be stopped. | high |
| `mulchie_update` | A newer Mulchie release is out. | low |
| `agent_update` | A newer version of an installed agent tool is out. | low |
| `homebrew_outdated` | Homebrew packages are outdated, from Homebrew's local data. | info |
| `advisory` | A package in a workspace lockfile has a known vulnerability in OSV.dev. Only with `updates.advisories` on. | from the advisory |

An update finding never installs anything. It names the command you can run.

## Turning the watch off

```sh
mulchie config set watch.enabled false
```

See [Configuration](/docs/configuration/#watch) for every `watch` setting.
