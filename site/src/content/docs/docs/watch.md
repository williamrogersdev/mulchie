---
title: Findings
description: "What Mulchie's watch reports about processes and its own setup, what each kind of finding means in plain words, and how to dismiss one you know is expected."
---

Use findings to spot processes that look left over, runaway or out of place, and problems with Mulchie's own setup. You can list them, read why each was raised, and hide the ones you know are expected.

## What findings are, and are not

Findings are observations made by simple heuristics: how long a process has run, how much CPU it uses, where its program file lives, which network addresses it listens on. Each one says what was seen and why it may matter.

A finding is not a verdict. **None of them means malware was found.** Mulchie is not an antivirus. The watch has no signatures, no file hashes and no lookups against any list, and it sends nothing anywhere (issue events go to a webhook only if you set one up, as [below](#sending-issues-to-slack-or-a-script)). A finding that a process runs from a temporary folder is a reason to check that you know what the process is, nothing more. Most turn out to be a dev server or an updated app.

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
| `orphan_swarm` | 20 or more copies of one program whose parents exited, wherever they run (for example `yes` processes a test left behind). One finding per program, with the count and their CPU and memory together. When the copies are outside workspaces and apps it suggests a plan to quit the program's process group. | medium; high at 100 copies or 4 cores of CPU together |
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
| `system_name_mismatch` | A process that uses the name of an operating-system program but runs from somewhere other than a system folder. A simulator's own copies under `/Library/Developer/CoreSimulator/` (inside a runtime's `RuntimeRoot`) are not reported. | high |
| `unsigned_executable` | A macOS app that is not code-signed, or only ad-hoc signed, and was not built in your workspaces. macOS cannot say who built it. Needs `watch.check_signatures`. | low; medium if downloaded |
| `quarantined_executable` | A loose command-line program that still carries the macOS download quarantine flag and is running. Needs `watch.check_signatures`. | low; medium if unsigned |

The last two kinds are macOS only. This group is where people most often see a false alarm: an unsigned app you built yourself, or a tool you downloaded on purpose, will show up here.

## Kinds about Mulchie's own setup

| Kind | What was seen | Severity |
| --- | --- | --- |
| `insecure_permissions` | Mulchie's config directory or a token file can be read by other users. Run `chmod 700` on the directory or `chmod 600` on the token file. | high |
| `api_exposed` | Mulchie's API port, or the trace receiver's port while it runs, is listening beyond this machine. Both listen on `127.0.0.1` only, so another program may have taken the port. | high |
| `otel_unavailable` | `otel.enabled` is on but the [trace receiver](/docs/traces/) could not listen, usually because another program (an OpenTelemetry Collector, say) holds the port. Mulchie tries again every minute. Stop the other program or set `otel.port`. | low |
| `engine_lock_not_held` | Another Mulchie engine is in charge. Only that one reclaims, sweeps and throttles. Expected when the app and `mulchie serve` both run. | medium |
| `config_not_loaded` | The config file was not used because it could not be parsed or a value is out of range. Mulchie runs on defaults with automatic reclaim and sweep off until you fix it. | medium |

## Kinds about sessions and updates

These come from [sessions and limits](/docs/sessions/) and [update checks](/docs/updates/), not from the process watch, and they appear in the same list.

| Kind | What was seen | Severity |
| --- | --- | --- |
| `limit_exceeded` | An agent session stayed over a cap of its budget for the sustain time. The title says which cap, the value and what Mulchie did. | medium; high when the action is `pause` or `stop` |
| `session_hog` | An agent session uses at least `limits.hog_share_pct` of the machine's CPU or memory while health is strained or critical. Only a report; no budget is needed. | medium |
| `paused_list_unreadable` | The file of processes Mulchie paused for a limit could not be read at start, so processes paused by an earlier engine may still be stopped. | high |
| `memory_low` | The machine stayed low on memory (or its swap filled up), or a consumer stayed over `memory_guard.unit_max_mb`. The title says what the [memory guard](/docs/sessions/#the-memory-guard) closed or would close. | high |
| `runaway_chain` | A command is running nested inside copies of itself (a build script that calls itself). The title says the command, its directory, how deep it is and how much memory the chain holds; see [runaway chains](/docs/sessions/#runaway-chains). | high |
| `mulchie_update` | A newer Mulchie release is out. | low |
| `agent_update` | A newer version of an installed agent tool is out. | low |
| `homebrew_outdated` | Homebrew packages are outdated, from Homebrew's local data. | info |
| `advisory` | A package in a workspace lockfile has a known vulnerability in OSV.dev. Only with `updates.advisories` on. | from the advisory |

An update finding never installs anything. It names the command you can run.

## Kinds about performance

| Kind | What was seen | Severity |
| --- | --- | --- |
| `build_slower` | A build or test command's latest successful runs each took much longer than its usual time. The title says the command, the workspace and both times; the evidence names the commits in between and the agent sessions that made them. See [Slower builds](#slower-builds). | low; medium at twice the usual time or more |
| `production_slower` | A deploy made a production number from a connected provider worse (Vercel build duration, Sentry transaction p95, PostHog LCP or INP). The evidence names the deploy, the number before and after, and the commits in the deploy with the agent sessions behind them, or `made_elsewhere`. Only when an integration is on. See [Production integrations](/docs/integrations/). | low; medium at twice the baseline or more |
| `endpoint_slower` | An endpoint of a local app that sends traces to Mulchie was much slower at p95 over the last 15 minutes than usual. The title says the operation, the service and both p95s; the evidence names the workspace, the commits since it was last normal and the sessions that made them. See [Traces](/docs/traces/#slower-endpoints). | low; medium at twice the usual p95 or more |
| `endpoint_errors` | An endpoint of a local app failed much more often over the last 15 minutes than usual. Same evidence as `endpoint_slower`. | low; medium at 25% of requests failing or more |
| `dev_server_slow_start` | A dev server's latest runs each took much longer than usual to start listening. The title says the command, the workspace and both times; the evidence names the commits in between and the sessions that made them. Only raised when the usual time is longer than the watch interval can resolve. See [Dev servers](#dev-servers). | low; medium at twice the usual time or more |
| `dev_server_heavier` | A dev server's latest runs each settled at much more memory than usual. Same evidence as `dev_server_slow_start`. See [Dev servers](#dev-servers). | low; medium at twice the usual memory or more |

The dashboard's **Performance** page shows the performance findings with their evidence, including each commit between the usual and the slow figures and the agent session that made it, and then short lists of recent build and test timings, endpoints from traces, dev server runs and production deploys. It reads the same data as `GET /v1/timings`, `/v1/otel`, `/v1/devservers`, `/v1/deploys` and `/v1/integrations`. The thresholds (`perf.*`) and the trace receiver (`otel.enabled`, `otel.port`) are under **Settings › Performance and traces**.

## Slower builds

Every command run with `mulchie run`, and every build slot an agent releases with a duration, is recorded as a timing: how long it ran, its exit status, the command (redacted), the workspace, branch and commit, and the agent session. `mulchie timings` lists them, and agents use `list_timings`.

Runs are grouped by workspace and command, with the command compared the way issues compare it (paths cut to their last part, numbers replaced by `#`). Only runs that exited with 0 count. A group is slower when:

- each of its last `perf.slow_runs` successful runs (default 3) took more than `perf.slower_ratio` times its usual time (default 1.5) and more than `perf.min_slowdown_secs` longer (default 30);
- its usual time is the median of up to `perf.baseline_runs` successful runs (default 10, at least 3) before the slow ones, so slow runs never raise their own baseline and one odd run does not move it.

The `build_slower` finding names the commit `HEAD` pointed to at the last normal run and at the first slow run, and the moves of `HEAD` Mulchie recorded in between, each with the agent session that was working in the workspace then. When `HEAD` did not move, the finding says the cause is outside the code: the build cache, dependencies, the toolchain or a busy machine. The finding goes away when the latest runs are back to the usual time. Like every finding it only reports: nothing is stopped, changed or rolled back.

## Dev servers

A dev server is a process of yours that sits in a workspace and listens on a TCP port: `next dev`, `vite`, `rails s`, a web app started with `cargo run`. Mulchie finds them from what it already reads: the process table, each process's memory and CPU, and the listening sockets the watch looks at (`watch.check_listeners`, on by default). It probes nothing more and acts on nothing. `mulchie devservers` lists the runs, and agents use `list_dev_servers`.

Each run (a process id and its start time) gets one record, written to `devservers.jsonl` when the process ends and every 30 minutes while it lasts. A run seen listening for less than a minute is not recorded. A record has:

- the command (redacted), the ports, the workspace, the branch, `HEAD` and the agent session. `HEAD` and the session are read when Mulchie first sees the run listening.
- **time to listen**: from the process's start to the first watch run that saw it listening.
- **settled memory**: the median resident memory of the process and everything it started, over minutes 2 to 5 of the run. It needs at least three samples in that window, so a run Mulchie met after minute 5 has none.
- **memory slope**: how fast that memory grew from minute 2 on, in MB per hour, as a least-squares line. It needs 10 minutes of watching.
- **peak CPU**, as a percent of one core with the whole tree summed, and peak memory. Both are read from the 3-second samples.
- how it ended: `running` for a record written while it lives, `exited` once the process is gone.

### How exact the time to listen is

The watch looks at listening sockets once every `watch.interval_secs` (60 seconds by default, at least 10). The time to listen is therefore an upper bound: the port may have opened any time since the previous watch run. Each record keeps `listen_uncertainty_ms`, how far below `listen_ms` the true time may lie (about one watch interval), and `watch_interval_secs`. A server that was already listening when Mulchie first looked, for instance after the engine started, has no time to listen, because its start was not watched. A process that started after the previous watch run and already listens counts as watched.

### Slower starts and heavier servers

Runs are grouped by workspace and command, the command compared the way issues compare it. Each run counts once, by its newest record. With the same settings as [slower builds](#slower-builds) (`perf.baseline_runs`, `perf.slow_runs`, `perf.slower_ratio`):

- `dev_server_slow_start`: each of the last runs took more than `perf.slower_ratio` times the median of the runs before to start listening, and more than `perf.min_slowdown_secs` or the largest uncertainty of the runs longer, whichever is more. When the usual time is no longer than that uncertainty, the figure is too coarse to tell and no finding is raised. With the default 60-second interval that means a dev server that usually listens within about a minute is not judged; lower `watch.interval_secs` to judge faster ones.
- `dev_server_heavier`: each of the last runs settled at more than `perf.slower_ratio` times the usual memory and more than 256 MB above it.

Both findings name the commit `HEAD` pointed to at the last normal run and at the first slow one, the recorded moves of `HEAD` in between and the sessions working then, as `build_slower` does, and go away once the latest runs are back to usual. They only report.

`devservers.jsonl` is in the config directory and readable only by you. It is renamed to `.1` at 2 MB, replacing the one before; nothing else is deleted. Without an engine, `mulchie devservers` reads it as the last engine left it.

### Which session moved HEAD

The engine that holds the lock reads `HEAD` of each workspace an agent session works in, or worked in during the last 15 minutes, every `perf.head_check_secs` seconds (default 60), and right after an agent's hook reports a shell command that runs `git commit`, `merge`, `rebase`, `cherry-pick`, `pull` or `am`. Each move is recorded with the session the hook named, or else the session working in that workspace that was active last.

- A record is a move of `HEAD`, not a commit: one `git pull` that brings in twenty commits is one record, and a commit amended before the next check is never seen.
- The session is a heuristic, not proof of who wrote the code. Two agents in one workspace, or a person committing while an agent runs, are told apart only through the hook.
- The first record of a workspace (when Mulchie started watching it) has no previous commit and no session.

`GET /v1/commits` lists the records. They are kept in `commits.jsonl` and timings in `timings.jsonl`, both in the config directory and readable only by you. Each file is renamed to `.1` when it reaches its size (2 MB for commits, 4 MB for timings), replacing the one before; nothing else is deleted.

## Issues

A finding lasts as long as its cause: a finding about a process ends when the process does. An issue groups the findings that are about the same problem, so you can see what keeps going wrong:

- Findings of the `hygiene`, `limit` and `performance` kinds become issues (leftover, stale, stopped and runaway processes, growing memory, runaway chains, low memory, sessions over a budget or hogging the machine, slower builds, slower production deploys, slower or failing endpoints). Security, setup and update findings do not.
- Two findings are the same issue when they have the same kind, the same workspace and the same subject. For a process the subject is its command line with paths cut to their last part and numbers, ports and long ids replaced by `#`: `/usr/bin/node /code/site/node_modules/.bin/next dev --port=3001` becomes `node next dev --port=#`. For a session it is the agent and the cap; for low memory, the consumer; for an endpoint, the service and the operation.
- Each separate finding counts once (`occurrences`): a process that runs away for a day is one occurrence; the same command running away again tomorrow is a second.
- An issue is `open` until you decide. `resolved` means you fixed it: if it appears again it becomes `regressed`, and the journal and, with notifications on, a notification say so. `ignored` keeps counting it but never reports it.
- Issues are kept in `issues.json` in the config directory (readable only by you) for 90 days after they were last seen, at most 500. Only the engine that holds the lock writes it.

`mulchie issues` lists them, `diagnose` names the ones that came back three times or more, and agents use `list_issues`. In the app, the dashboard's **Issues** page lists the unresolved ones (those a finding is about right now are marked), with a filter for open, regressed, resolved and ignored, and each row's title, kind, culprit, workspace, how often it occurred, when it was first and last seen and how many times it came back. **Resolve**, **Ignore** and **Reopen** change the record; if another engine holds the lock, the app says it cannot change issues from here. The count of issues that are current now is shown next to Issues in the sidebar, in the menu bar panel and in the menu bar icon's tooltip. An issue is a record: Mulchie never acts because of one.

## Sending issues to Slack or a script

Mulchie can post issue events to a webhook. **Nothing is sent unless `webhook.url` is set**; it is empty by default.

```sh
mulchie config set webhook.url https://hooks.slack.com/services/T000/B000/XXXX
mulchie config set webhook.format slack
mulchie webhook test
```

The events are `new_issue` (an issue seen for the first time), `regressed` (one you resolved came back) and `resolved_by_user` (you resolved one). By default the first two are sent, for issues of severity `medium` or higher; `webhook.events` and `webhook.min_severity` change that ([Configuration](/docs/configuration/#webhook)).

With `webhook.format json` (the default), each event is one `POST` with a JSON body holding exactly:

- `event`: the event name.
- `issue`: the issue's `id`, `kind`, `category`, `severity`, `title`, `culprit`, `workspace` (a path on this machine), `occurrences`, `status`, `regressions`, `first_seen_unix` and `last_seen_unix`. The title and culprit are passed through Mulchie's secret redaction again before they are sent.
- `commits`, for performance issues only, when the finding names them: an object with the commit range (`from`, `to`), a `commits` list of up to 20 commits, the newest, each with its `sha`, `at_unix`, `branch`, and the `session_id` and `agent` that were working then, and `truncated` when there were more.
- `host`: this machine's host name, only with `webhook.include_host` on.

With `webhook.format slack`, the body is `{"text": "..."}` with the same in one line, for a Slack incoming webhook. `mulchie webhook test` sends `{"event": "test", "message": "..."}` once (or a one-line `text`), with `host` too when `webhook.include_host` is on.

Commands that set or post to a Slack or Discord webhook URL, or that follow `webhook.url`, are kept in session records and findings with the URL cut to its scheme and host.

How it sends:

- Only the engine that holds the engine lock sends, and only in a build with network support. It sends from its own thread, through a queue of 64 events; when the queue is full, an event is dropped and counted.
- Each post has a 10 second timeout and follows no redirect. A `429`, a `5xx` or no answer is tried again, at most twice, with a wait in between; any other answer is final.
- The URL is read when an event is sent, so clearing it stops what is queued.
- `https://` goes to any host; plain `http://` only to a loopback address, for a script on this machine. A URL with a user name or password is refused.

The URL can hold a secret, as Slack's does. Mulchie keeps it in the config file and shows it nowhere else in full: `mulchie webhook`, `mulchie config`, the HTTP API, errors and the journal show `https://hooks.slack.com/[redacted]`. `mulchie webhook` (or `GET /v1/webhook`) shows how many events were sent, failed and dropped, and the last error.

## Turning the watch off

```sh
mulchie config set watch.enabled false
```

See [Configuration](/docs/configuration/#watch) for every `watch` setting.
