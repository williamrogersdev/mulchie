# Mulchie architecture

This describes how the code is built and the rules it keeps. When this page and the code disagree, the code is right and this page is a bug.

## What the product is

Mulchie keeps one machine healthy while many AI coding agents work on it. It does five jobs:

1. **Monitor**: sample CPU, memory, disk and per-process I/O, and attribute them to a git repo or worktree, an app or an agent session.
2. **Optimize**: lower build priority under pressure and queue heavy builds machine-wide.
3. **Reclaim**: delete stale build caches and idle agent worktrees without losing work.
4. **Supervise**: group each agent with what it started into a session, hold sessions to budgets the user set, keep an activity record, take hook calls from the agent apps and apply guard rules to shell commands.
5. **Report**: raise findings about processes and Mulchie's own setup, and report newer versions of Mulchie, agent tools and packages.

Mulchie is a supervisor. It is not a sandbox and not an antivirus. An agent that wants to get around it can, and the user-facing docs say so wherever these features appear.

The third job deletes directories and the second and fourth signal processes. Every design choice below follows from one rule: **a destructive action is decided from facts read at the moment it runs, never from a cached scan.**

## Components

```
                 ┌────────────────────────── mulchie-core ───────────────────────────┐
                 │                                                                    │
 sysinfo, git ──▶│ monitor ──▶ engine (state, optimizer, build queue)                 │
 filesystem   ──▶│ reclaim, diskmap (scanners: read-only)                             │
                 │ watch (findings), consumers, session tracker, limits, memguard,    │
                 │ runaway                                                            │
                 │ guard (pure), updates (HTTPS and `brew outdated`)                  │
                 │                 │                                                  │
                 │                 ▼                                                  │
                 │           safety gate ──▶ delete / remove / signal / renice        │
                 │                 │                                                  │
                 │                 ▼                                                  │
                 │   backups + action journal + session records (record.rs)           │
                 │                                                                    │
                 │ api (HTTP on 127.0.0.1, bearer tokens)                             │
                 └───────▲──────────────▲───────────────────▲─────────────▲──────────┘
                         │ in-process   │ HTTP              │ HTTP        │ HTTP
                 ┌───────┴──────┐  ┌────┴─────────┐  ┌──────┴───────┐  ┌──┴──────────┐
                 │ mulchie-app  │  │ mulchie CLI  │  │ mulchie mcp  │  │ mulchie hook│
                 │ tray + UI    │  │ + `serve`    │  │ (stdio)      │  │ (agent apps)│
                 └──────────────┘  └──────────────┘  └──────────────┘  └─────────────┘
```

| Crate | Package | Role |
| --- | --- | --- |
| `crates/mulchie-core` | `mulchie-core` | Engine, scanners, safety gate, sessions, limits, record, guard, updates, HTTP API and client. No UI. Builds on macOS and Linux. |
| `crates/mulchie-cli` | `mulchie` | The `mulchie` binary: CLI, `serve` (headless engine), `mcp` (stdio server), `hook` and `hooks`, `service`, `doctor`. |
| `crates/mulchie-app` | `mulchie-app` | Tauri 2 menu bar app for macOS. Hosts an engine in-process and bundles the CLI as a sidecar. Not a default workspace member and not published to crates.io. |
| `site/` | n/a | Astro site for mulchie.com: landing page, install page, docs. |

Exactly one engine runs per user. The app and `mulchie serve` both take an exclusive lock on `<config dir>/engine.lock`; the one that loses the lock becomes a client of the other. Only the engine that holds the lock automates: it throttles, reclaims and sweeps (when configured), enforces limits, writes session records, writes the config and runs update checks. An engine that does not hold the lock, or whose config file did not load, reports and never acts (`engine_lock_not_held` and `config_not_loaded` findings say so).

The `network` cargo feature (on by default in `mulchie-core`, `mulchie` and the app) builds the HTTPS client. `--no-default-features` builds without it: every update check then reports that the build has no network support and nothing leaves the machine.

## Core

### Engine

- **Sample loop** (every 3 s, thread `mulchie-sample`) and **scan loop** (every `scan_interval_mins`, and on demand, thread `mulchie-scan`). Both are supervised: a panic is caught, logged, and the loop restarts with backoff.
- Three more workers, each started only by the lock holder: `mulchie-limits` carries out limit actions, `mulchie-updates` runs update checks, and `mulchie-record` writes session records, `mulchie-perf` reads `HEAD` of workspaces and records timings (see Performance), and `mulchie-integrations` polls the providers the user connected (see Production integrations). The API runs on `mulchie-api` with one short-lived thread per connection.
- The sample loop never does slow work. Reclaim, sweep, scans, the watch and update checks are requested through a channel or run on their own thread. Sessions and limits are evaluated in memory on the sample loop (no disk, no subprocess, no network); actions go to the limits worker and records to the record writer through a bounded queue that drops, and counts, events when full.
- **The periodic disk scan is built to go unnoticed.** Measuring a directory is one pass (`measure.rs`) that returns its size and its newest file change together; on macOS it reads each directory with `getattrlistbulk`, one system call for many entries, instead of one `lstat` per file. The scan loop keeps what it measured (`diskmap::ScanMemo`): a worktree that is idle (its `HEAD`, git index and folder unchanged, no process working in it, its last change over a day old) is measured again after `reclaim::REMEASURE_SECS` (6 hours); a busy one (any of those not true) after `diskmap::BUSY_REMEASURE_SECS` (2 hours), since a busy worktree is never swept and its figures are only shown; caches and stores are measured again when their folder's change time moves. The memo is saved after every scan to `<config dir>/scan-memo.json` (mode 0600, written atomically) and read back by the first periodic scan after a start, so a restart does not walk every worktree again; a file that is not a plain file, is over 16 MB, has another format number or cannot be parsed is ignored, and entries dated after the current time are dropped. The periodic scan runs on two worker threads at the operating system's background priority (`measure::Background`, which changes only the calling thread of Mulchie itself). A settings change rescans only when it changes what the scan reads (the scan roots, cache options, the idle and stale windows, extra agents), and then as the periodic scan does; dismissing a finding never rescans. A scan that was asked for (the app's Rescan, `POST /v1/scan`) measures everything again at normal priority, and the safety gate reads every fact again before it removes anything, so a reused measurement is never what a deletion rests on.
- State locks recover from poisoning. `/v1/health` reports `sample_age_secs`; health is `stale` when the last sample is older than 15 s, so a frozen engine cannot keep reporting "Good".
- `serve` handles SIGTERM and SIGINT, resumes any process Mulchie paused for a limit, releases the lock and exits cleanly. When another engine (the app) holds the lock, `serve` waits and tries again every 15 seconds instead of exiting, so a service manager that restarts a failed `serve` does not start a new process every few seconds.

### Consumers and process plans

`consumers.rs` groups every process into exactly one group: a workspace, an application, a program, or the system group. Keys (`workspace:…`, `app:…`, `process:…`) are what `mulchie group` takes. The CLI also takes `session:<id>` and sends it as the `session` field of the process-plan request.

`procplan.rs` turns a request (one pid with a signal, a pid and its descendants, a group with an action, or a session with an action) into a list of gate requests, each carrying the start time the planner saw, plus who is affected and why anything is refused. Nothing there acts; the caller posts approved requests to `POST /v1/process/execute` and the gate re-checks every one. Protected processes (pid 1, operating-system programs, Mulchie, and the caller's own terminal and agent app) come back blocked with a reason. A session plan leaves out the caller and its ancestors even with `include_agent`, and is refused when nothing else is left.

### Safety gate (`safety.rs`)

Every cache deletion, worktree removal and signal to another process goes through this module, as a `Request`: `DeleteCache`, `RemoveWorktree`, `PruneWorktree`, `Signal`, `Renice` or `ReopenApp`. Scanners and planners only produce *candidates*. The gate re-reads the facts for each candidate and returns a per-item outcome: `done`, or `skipped` with a reason.

Checks for a **cache** deletion:

- The path is canonical, not a symlink, and under a configured scan root (Xcode DerivedData is the one fixed exception, and only when `include_xcode` is on).
- It is a recognised cache: `target/` must contain `CACHEDIR.TAG`; `node_modules` only when `include_node_modules` is on.
- Nothing is using it: no process owned by the user has a working directory at or under the project (prefix match on canonical paths, working directories refreshed now). DerivedData is busy while Xcode or `xcodebuild` runs.
- When the request was "stale caches", staleness is recomputed now.

Checks for a **worktree** removal:

- Each request names the path and the `HEAD` commit the caller saw. If `HEAD` moved, the item is skipped.
- `git status`, detached state, idle time and busy state are read now.
- Backup happens first, and removal proceeds only if every backup step succeeded:
  - uncommitted changes → a patch in the backups directory;
  - gitignored files that are not known caches (`.env`, local databases, notes) → a `tar.gz` beside the patch; if they exceed `backup_ignored_max_mb` the worktree is skipped instead;
  - a detached `HEAD` → a new branch `backup/<name>-<shortsha>`; an existing branch is never moved.
- After the backup and before `git worktree remove`, `HEAD`, `git status` and the changed and ignored files are read again, and so is the process table. If anything differs the item is skipped and the backup set written for it is discarded, because the worktree is untouched (a sweep that hits the same skip on every scan does not pile up copies). A backup set that could not be completed is discarded the same way. Once `git worktree remove` has started the backups are always kept, and a failure names the directory they are in.
- A "missing" worktree is pruned only when git itself reports it prunable, only if it is an agent worktree, and with `--expire`.

Checks for a **signal or renice**:

- The target is owned by the current user, is not pid 1, and is not a Mulchie process (engine, `mcp`, `run`).
- The pid's start time is compared with the snapshot immediately before the signal, so a reused pid is not hit.
- A renice only raises the nice value.

Sweep and clean are two-step for interactive callers: `GET` returns a plan, `POST` takes the items the user approved. Automatic sweeps and reclaims build and execute a plan in one step, through the same gate.

Planning only reads, and most of its time is spent waiting for git, so the items of one plan are checked by up to four threads (`PLAN_WORKERS`), sharing one read of the process table; the plan lists them in the order asked. Execution stays one item at a time.

Every executed or skipped action is appended to `<config dir>/actions.jsonl` (rotated at 5 MB). Dismissing a finding, and showing it again, is appended too (kind `finding`).

#### What Mulchie remembers about skips (`failures.rs`)

The engine folds the journal's `reclaim` and `worktree` entries into one record per target: how many times in a row it was skipped, the last reason, and since when. The memory is rebuilt from the last 5000 journal entries at start, so nothing new is stored. A success clears a record, and one with no new skip for 30 days is forgotten. Process actions are left out, because a pid names a different process each time.

- From the second skip in a row the target is in `failures` of `GET /v1/state` (while it still exists on disk), in `diagnose` and in `get_status`, with a sentence on what a person can do about that reason.
- A plan item for such a target carries `past_failures`. It stays executable: a person may have fixed the cause.
- From the third skip in a row the engine's own passes (automatic sweep and reclaim, and `{"auto": true}`) do not run the target for 6 hours, doubling with each further skip up to 7 days. They return it as skipped with `backing_off` and write nothing more to the journal; one `failure` entry is written when the wait begins. The engine only does less here: it never changes the request, widens it or tries another way.

#### What may delete or signal outside the gate

Nothing else may delete user files or signal another program's process. These are the places in the code that delete, signal, renice or probe a process without going through `safety.rs`. Each is narrow, and a change to any of them needs a test:

| Where | What it does |
| --- | --- |
| `git.rs` (`kill_group`) | SIGKILLs the process group of a child Mulchie started when it exceeds its timeout. Every helper command run through `run_with_timeout` or `run_capped` (git, the package managers' prune commands, `lsof`, `codesign`, `brew`) gets this. |
| `mulchie run` (`run.rs`) | Forwards SIGINT and SIGTERM to the command it started. |
| `record.rs` | Deletes Mulchie's own record files under `<config dir>/sessions/`: past `sessions.retain_days`, and the oldest ended sessions' first when the directory passes `sessions.max_total_mb`. Only regular files named after a session id; live sessions' records are kept. |
| `engine.rs` (`restore_priorities`) and `engine_sessions.rs` (`restore_breach`) | Put back, with `set_nice`, a priority Mulchie lowered, after re-reading that the pid is still the same own process by start time. |
| `engine.rs` (`throttle`) | Lowers the priority of a build process with `lower_priority`, after the same start-time recheck. It only raises a nice value. It runs only after 5 samples in a row under pressure, and only in an engine that can put the priority back (one running as root): lowering a nice value needs privilege, so a renice by an ordinary user would last for the life of the process. Priorities go back after 10 calm samples in a row. |
| `engine.rs` (`own_pid_alive`) and `safety.rs` (`pid_alive`) | Liveness probes: `kill(pid, 0)` sends no signal. |
| `safety.rs` (`discard_backup_set`) | Inside the gate, but not a `Request`: removes a backup set the gate itself wrote a moment ago for a removal that did not happen. Only a real directory directly inside Mulchie's backups directory, and only before `git worktree remove` has started. |
| `integrations.rs` (`remove_token`) | Deletes a provider token file, `<config dir>/integrations/<provider>.token`, when the user runs `mulchie integrations remove` (and when `add` saved a new token but the config change was refused). Only a regular file of exactly that name inside that directory; a symbolic link or anything else is refused and left in place. Mulchie's own file, never a user file. |
| `diskmap.rs` (`prune_cache`) | Runs a package manager's own prune command (npm, bun, pnpm, yarn, uv, pip, Homebrew, Go, or `xcrun simctl` for simulators) for a cache key, refused while that tool runs. The tool, not Mulchie, deletes what is in its cache. |

Mulchie also manages its own files, none of them user files: `config.rs` writes the config through a temp file and rename and moves an invalid config file aside; `journal.rs` rotates the action journal; `api.rs` recreates an empty or truncated token file; `hook.rs` restarts `hook.log` past 256 KiB and rewrites `hook-dirs.json`; `serve_log.rs` copies `~/Library/Logs/mulchie-serve.log` to `mulchie-serve.log.1` and empties it past 10 MiB (only a regular file of that name that `serve`'s stdout or stderr appends to); `service.rs` removes the unit file `service install` wrote, and runs `launchctl` or `systemctl` for it, on `service uninstall`; the app replaces its own symbolic link to the CLI; `jsonl.rs` renames `commits.jsonl` and `timings.jsonl` to `.1` at their size cap, replacing the one before; and `mcp_install.rs` and `hooks_install.rs` write agent apps' config files atomically after backing them up, and remove all but the newest five of their own backups of each file (regular files named exactly `<file>.mulchie-backup-<unix>` or `<file>.mulchie-backup-<unix>-<hex>`; a symbolic link is never followed or removed).

### Findings (`watch.rs`)

Read-only detectors turn facts about processes and about Mulchie's own setup into findings. They run on the scan loop every `watch.interval_secs`. Costly probes (listening sockets, code signatures, quarantine flags) are bounded in count and time. A finding carries a kind, a category, a severity, a plain sentence on what was seen and why it may matter, evidence (command lines redacted), and suggested next calls, which are plans, not actions. Findings are observations from heuristics, never a verdict and never malware detection. A dismissal is stored in the config (`watch.dismissed`).

The other producers replace their own category of findings: `limit` (limit breaches and hogs), `update` (`updates.rs`), `performance` and the posture checks. Producers that share a category replace only their own kinds (`replace_findings_of`): `build_slower`, `endpoint_slower` and `endpoint_errors` (`engine_perf.rs`) and `production_slower` (`engine_integrations.rs`) leave each other's findings alone.

### Sessions (`session.rs`, `engine_sessions.rs`)

A session is a root process recognised as an agent plus every descendant. A root is recognised by name, executable, `argv[0]`, or the script a runtime (`node`, `bun`, `deno`, `python`) runs, against a built-in list extended by `agents.extra`; or it is registered by a hook call. A process belongs to the nearest root among its ancestors, so an agent started by another agent is its own session and no process is counted twice. A session's id is `s-<root start time>-<root pid>`, so a reused pid is a new session. It ends when its root exits.

`Tracker::observe` is pure: one process list per sample in, events out. Per session it keeps CPU cores, memory, process count, bytes written and a one-minute write rate (where the system reports per-process writes), runtime, build slots held, listening ports from the last watch run, and peaks. A session's `source` is `detected` or `hook`.

### Limits (`limits.rs`, `engine_sessions.rs`)

`limits.default` and `limits.workspaces` (longest path wins, and replaces the default whole) set budgets. A budget has caps (`cpu_cores`, `memory_mb`, `processes`, `write_mb_per_hour`, `runtime_hours`), each with `sustain_secs` and an action, plus `resume_after_secs`, `include_agent` and `stop_grace_secs`.

| Action | What happens |
| --- | --- |
| `notify` (default) | A finding and, if notifications are on, a notification. Nothing is signalled. |
| `deprioritize` | `Renice` to `max(build_nice, 10)` on the session's processes; priorities are put back when the breach clears, which usually needs privilege and may fail. |
| `pause` | `Signal` SIGSTOP; SIGCONT after `resume_after_secs`, whether or not the session is back under the cap, and on shutdown or at the next start after a crash. |
| `stop` | `Signal` SIGTERM, then SIGKILL after `stop_grace_secs` to what is left. |

- `Limits::evaluate` is pure and decides; the engine queues the action for the limits worker.
- The three actions other than `notify` act on the session's descendants, not on its root, unless `include_agent` is set.
- A breach is enforced only when the engine holds the lock and the config loaded, and only for a session whose source is `detected`. A hook-registered session, or an engine that is not automating, gets the breach recorded as `not_enforced`. `Session.enforced` and `not_enforced_reason` (`hook_registered`, `not_automating`, `no_budget`, `notify_only`, `ended`) say so.
- Every action is a gate request carrying the start time the sample saw, and is journaled and written to the session's record.
- Processes Mulchie paused are written to `<config dir>/sessions/paused.json` before SIGSTOP is sent. A start after a crash resumes them, or, on an engine that cannot act, reports them (`paused_list_unreadable` when the file cannot be read).
- With or without budgets, a session using `limits.hog_share_pct` of CPU or memory while health is strained or critical for `limits.hog_sustain_secs` becomes a `session_hog` finding. That only reports.
- A breach is delivered to the agent once, through its next hook call (only to the client the session is bound to), and shows in `diagnose` and `GET /v1/limits/status`.

### Memory guard (`memguard.rs`, `engine_memguard.rs`)

Limits hold one session to its budget. The memory guard looks at the whole machine. `memory_guard.action` is `off`, `notify` (default) or `stop`.

- Memory is low when less than `free_pct` of it is available, or, with `on_pressure`, the kernel reports critical pressure, or swap in use reaches `swap_pct` percent of physical memory (0 turns that off), for `sustain_secs`. Swap is measured against physical memory because macOS grows its swap file as needed; a machine can fill its swap while a third of memory still counts as available. Each memory finding names what is behind it (`evidence.culprits`): the sessions, apps, workspaces and programs using the most memory, or holding the most swap when swap crossed its threshold. Swap per process is its physical footprint minus what is resident (`proc_pid_rusage`, macOS, your own processes only), so it counts compressed memory too.
- The sample loop builds the closable units from the sample: one per running session Mulchie detected (what the agent started; the agent too with `include_agent`), and with `scope: all` every other consumer group whose members are all the user's own, none protected, none named in `never`, and none an ancestor of an agent session or of the engine. A session's processes are only ever in the session's unit.
- `MemGuard::evaluate` is pure. It picks the biggest unit using at least `min_mb` (or, with `unit_max_mb`, a unit that stayed over that cap), at most one per `cooldown_secs`, and at most `MAX_CLOSINGS_PER_EPISODE` (2) for low memory or high swap until memory is fine again: closing the biggest unit does not always free what is short, and swap is often held outside any session. A unit of which the gate closed nothing is left alone for ten minutes and does not count against the episode.
- With `notify`, or on an engine that does not automate, the result is a `memory_low` finding and one notification per episode. With `stop`, the closing is queued for the limits worker: `Signal` SIGTERM for every process with the start time the sample saw, then SIGKILL after `stop_grace_secs` to what is left (a process a limit had paused is resumed first, so it can act on SIGTERM), all through the gate, journaled, and written to the session's record as `memory_guard` events.
- `state.memory_guard` and `memory_guard` in `GET /v1/limits/status` report the action, whether this engine enforces it, whether memory is low, what would be closed and the last 50 closings.

### Runaway chains (`runaway.rs`, `engine_runaway.rs`)

A build script that calls itself adds a level about every second, and every level keeps its memory while it waits for the next. `runaway.action` is `off`, `notify` (default) or `stop`.

- `runaway::detect` is pure and runs on the sample loop over the user's own processes. A chain is one command line, in one working directory, nested inside at least `runaway.depth` copies of itself (default 6). Login shells, shells running with flags only or with a `-c` string (`zsh -l`, `bash -lc …`), and agent processes are never the repeating command: a shell forks a copy of itself, command line and all, for every subshell, so an agent's shells nest like this all day. A command a script recurses through (`npm run build`, `bash build.sh`) is still matched. A parent that started after its child is a reused pid, not a parent. A chain is reported once, from its outermost copy, with everything under it.
- With `notify`, or on an engine that does not automate, the result is a `runaway_chain` finding per chain and one notification when it is first seen.
- With `stop`, nothing is decided on one sample: a chain is stopped only once it was in at least 3 samples over at least 6 seconds and has grown at least 2 levels deeper since first seen (`runaway::confirmed`). Nesting that comes and goes, or stays as deep as it was, is reported and left alone. The stopping is then queued for the limits worker, which holds the pause lock (so shutdown cannot run in between), refuses when the chain has since ended or shrunk below `runaway.depth`, reads a fresh process list and sends `Signal` SIGSTOP to the outermost copy and everything under it, and repeats until a pass finds nothing new (at most 8 passes): a stopped process starts nothing, and a level it started in the meantime is still under it. Each process is written to the paused list (`paused.json`, `breach_id` `runaway:<id>`) before its SIGSTOP and removed once the stopping is over, so a crash between the freeze and the kill leaves it for the next engine to resume. Then it sends SIGKILL to all of it. Killing without the freeze would race the chain, and a level started between the list and the kill would carry on as an orphan. A frozen process the gate would not kill is resumed. Every signal is a gate request with the start time that list saw, journaled, and written to the session's record as a `runaway` event. A chain still there is tried again after 30 seconds.
- Nothing above the outermost copy is touched: not the shell that ran it, not the agent.
- A chain's self-nesting commands are remembered with their directory for `runaway.remember_secs` (in memory only, so not across an engine restart). The `runaway_repeat` guard rule matches them, and a session whose chain was stopped gets one sentence about it in its next hook reply.
- `state.runaway` and `runaway` in `GET /v1/limits/status` report the action, whether this engine enforces it, the chains running now, the last 50 stoppings and what is remembered.

### Issues (`issues.rs`)

Findings of the `hygiene`, `limit` and `performance` categories (without `paused_list_unreadable`) are grouped into issues by a fingerprint that leaves the pid out: the kind, the workspace and a subject (the command line normalized by `issues::normalize_command`; the agent and cap for session findings; the consumer for low memory; the program for an orphan swarm). The id is 16 hex digits of a 64-bit FNV-1a hash of that, the same in every build.

The engine feeds every stored finding to the book whenever the watch or the limit findings change (`observe_issues`). A finding id not current at the previous call is a new occurrence; a resolved issue with a new occurrence becomes `regressed`, which is journaled (kind `issue`) and notified. Resolving, ignoring and reopening go through `POST /v1/issues/{resolve,ignore,reopen}` and are journaled too.

The book is kept in `<config dir>/issues.json`, mode 0600, replaced whole with `write_atomic`: when something changed, and every 5 minutes while an issue is active, so `last_seen_unix` survives a crash. It also stores which current finding belongs to which issue, so a restart does not count a finding that is still there again. Only the engine that holds the lock writes it (a one-off engine of the CLI keeps what it sees in memory, and refuses changes with `not_lock_holder`). A file that is not a plain file, is over 8 MB, has another format number or cannot be parsed is ignored. Issues not seen for 90 days are dropped from it, and beyond 500 the ones seen longest ago; no file is deleted. Nothing acts on an issue.

### Performance (`perf.rs`, `timings.rs`, `commits.rs`, `engine_perf.rs`)

A release with `duration_ms` (`POST /v1/release`, which `mulchie run` and the MCP tool `release_build_slot` send) that frees a ticket queues a timing; the hook queues a check of `HEAD` after a post-tool event whose command runs `git commit`, `merge`, `rebase`, `cherry-pick`, `pull` or `am`. Both only enqueue (a bounded queue of 256): no git runs on the API, hook or sample path. The `mulchie-perf` worker, started only by the lock holder, drains the queue, reads `HEAD` with `git::head` (one `git rev-parse` with a timeout) for workspaces with a running session or one active in the last 15 minutes (at most 32), and with a timing in the last hour, every `perf.head_check_secs`, and appends to `<config dir>/commits.jsonl` and `timings.jsonl` (mode 0600, opened with `O_NOFOLLOW`, renamed to `.1` at 2 and 4 MB). A move of `HEAD` is attributed to the session the hook named, else the most recently active session in that workspace; the first sighting of a workspace has none.

`perf::slower` compares each (workspace, normalized command) group of successful runs: the baseline is the median of up to `perf.baseline_runs` runs before the slow streak, and the last `perf.slow_runs` runs must each be over `perf.slower_ratio` times it and `perf.min_slowdown_secs` longer. A slower group is a `build_slower` finding in the `performance` category, with the commits recorded between the last normal and the first slow run. The worker replaces that category whenever timings, commits or the `perf` config change. Nothing acts on a timing or a finding.

### Feedback (`feedback.rs`)

Agents and people report problems with Mulchie or the machine: the MCP tool `report_feedback`, `mulchie feedback report`, or `POST /v1/feedback` (the read-only token is accepted, since a report only adds a line to Mulchie's own file). Each report is one JSON line in `<config dir>/feedback.jsonl`, mode 0600, opened with `O_NOFOLLOW`. The engine adds the machine's state from its last sample and a `feedback` entry in the action journal. Free text is cut to a fixed length and passed through the command-line redaction. At 4 MB the file is renamed to `feedback.jsonl.1`, replacing the earlier one, as the action journal does. With no engine, the CLI and the MCP server append to the file themselves, without the machine's state. Mulchie never sends the file anywhere.

### Activity record (`record.rs`)

Each session has an append-only record in `<config dir>/sessions/<id>.jsonl`: files `0600` in a `0700` directory. Events carry `seq`, `t` and `type`: `session_start`, `session_end`, `process_start`, `process_starts_summarised`, `limit_breach`, `limit_action`, `limit_cleared`, `memory_guard`, `runaway`, `hook_<event>` and `truncated`. Command lines are redacted by `redact.rs` before they are stored, which is pattern-based and best effort, and prompt text is never sent to the engine. Every string is cut at 4096 bytes.

Writes go through a bounded queue to one writer thread, so the sample loop never waits on the disk. A record stops at `sessions.max_record_mb` or `sessions.max_events` (one `truncated` event), process starts are limited to `sessions.process_events_per_minute` per session, and the directory is held to `sessions.max_total_mb`. Records of ended sessions are deleted after `sessions.retain_days`.

A record file is opened only through `open_plain` (not through a symlink, never blocking on a FIFO, only a regular file with one link). Readers use `GET /v1/sessions/{id}/events`, which seeks through an in-memory index of byte offsets and waits at most 250 ms for queued events.

The record is not tamper-proof. The first `(client, client_session_id)` that reports for a session binds it; events from any other caller are stored with `unbound: true`, but a process of the same user that learns the bound pair can pose as that client.

### Hook and guard (`engine_hook.rs`, `guard.rs`, and `hook.rs` in the CLI)

`mulchie hook <client> <event>` is the command an agent app's hook system runs. It reads the client's JSON event on stdin and posts `POST /v1/hook` to the engine with the **read-only token**, then prints the client's own answer format. The request carries the client name and session id, the neutral event (`session_start`, `prompt`, `pre_tool`, `post_tool`, `stop`, `session_end`), the hook process's parent pid, the working directory, and for shell tools the tool name and command text (up to the guard's 64 KiB, and a few bytes past it when longer, so the guard warns `too_long` instead of checking a cut copy). Prompt text is not read into the request. Copilot runs its tool hooks for every tool, so for Copilot only the shell tools (`bash`, `shell`, `powershell`) reach the engine; other tools are allowed at once without a connection.

The engine maps the pid to a session through its ancestors. When none covers it, the nearest agent ancestor (else the nearest non-shell) is registered as a hook session (at most 64 live). On `session_start` it returns the machine's state and house rules; on `pre_tool` with a command it runs the guard rules and may add `mulchie run` advice for a recognised heavy build while heavy work is not advised; on any event it returns the session's undelivered breaches once, and one sentence for each runaway chain Mulchie stopped in it. The work is a few process-table lookups and in-memory checks: no scan, no subprocess, no network.

`guard.rs` is a pure check of one command. Its rules are `recursive_delete`, `pipe_to_shell`, `sudo`, `force_push`, `disable_mulchie`, `credential_read`, `startup_write` and `runaway_repeat` (the command, in the directory it would run in, is one the engine just saw starting itself over and over; the engine hands the remembered commands in), each `off`, `warn` or `block` (default `warn`), plus an always-warning `too_long`. It is a tripwire for honest agents, not a sandbox: it reads command text and prefers a missed match to a false alarm, so obfuscated commands pass. The CLI translates the engine's decision to each client's format; whether a `warn` can be an "ask" depends on the client (Claude Code, Cursor and Copilot CLI can; Codex and Gemini CLI show text and let it run), and `--guard off|warn|block` on `hooks install` caps a client's answer.

The hook fails open: no engine, no answer within 300 ms for everything, bad input or any error allows the action, exits 0 and notes the reason in `<config dir>/hook.log` at most once a minute. When the hook's config directory is not `~/.config/mulchie` (its `XDG_CONFIG_HOME` is set), or no engine answered, it notes the directory per client in `~/.config/mulchie/hook-dirs.json`, and forgets that note once it runs in the default directory and reaches the engine; `mulchie hooks status` warns when a client's hook ran in a directory other than its own in the last 7 days, since an agent whose `XDG_CONFIG_HOME` differs from the engine's looks for the engine in the wrong place and allows everything. `mulchie hooks install` edits one user-level file per client (`~/.claude/settings.json`, `~/.codex/hooks.json`, `~/.cursor/hooks.json`, `~/.gemini/settings.json`, `~/.copilot/hooks/mulchie.json`), keeps other hooks, backs the file up to `<file>.mulchie-backup-<unix>-<random>` (keeping the newest five), writes atomically and supports `--dry-run`. `uninstall` removes exactly the entries it added. Only clients whose hook mechanism is documented are supported.

### Updates (`updates.rs`, `engine_updates.rs`)

Update checks produce findings in the `update` category and never install anything. Each finding names the command a person can run.

| Check | Source | Default |
| --- | --- | --- |
| Mulchie itself | GitHub releases API, latest release | on |
| Installed agent tools | The tool's installed version read from package metadata (never by running it), the latest from the npm registry's dist-tags for Claude Code, Codex and Gemini CLI | on |
| Homebrew | `brew outdated --json=v2` with a minimal `PATH` and no auto-update, run only when the local `homebrew/core` tap exists. A skipped check (off, no `brew`, no core tap) drops the earlier result and the report says why in `homebrew_skipped`; a run that fails keeps the last result and adds to `errors` | on |
| Advisories | OSV.dev, for package names and versions in each workspace's `Cargo.lock`, `package-lock.json` and `go.mod` | **off**; it tells a third party what a project depends on |

Every request is HTTPS with certificate verification, a 15 s timeout, a size cap and the user agent `mulchie/<version>`, behind an `Http` trait so tests use a fake. The first check waits five minutes after the engine starts, then runs every `updates.interval_hours` (sooner after errors, with backoff). Results are cached in `<config dir>/updates.json`. `updates.check: off` makes no request and runs no command.

### Production integrations (`integrations.rs`, `integrations_vercel.rs`, `integrations_sentry.rs`, `integrations_posthog.rs`, `engine_integrations.rs`)

Read-only links to Vercel, Sentry and PostHog. A `DeploySource` per provider has two operations: list recent deploys with their commit SHA and time, and fill in a performance number per deploy.

| Provider | Deploys | Number |
| --- | --- | --- |
| Vercel | `GET api.vercel.com/v7/deployments?projectId=…&target=production&state=READY`, commit from `meta.githubCommitSha` (or the GitLab or Bitbucket field) | Build duration, `ready - buildingAt`. Vercel's REST API has no stable per-deployment latency metric, so this measures the build, not serving; the finding says so |
| Sentry | `GET /api/0/organizations/{org}/releases/?project=…`, commit from `lastCommit.id`, else the version when it is (or ends in `@`) a SHA | `GET /api/0/organizations/{org}/events/?dataset=spans` with `p95(span.duration)`, `count()` per `release` and `is_transaction:true` over 30 days; releases under 20 transactions are skipped |
| PostHog | `POST /api/projects/{id}/query/` with a HogQL query: values of `release_property` (default `$app_version`) on `$web_vitals` events in 30 days, first seen | A second HogQL query: p75 of `$web_vitals_LCP_value` and `$web_vitals_INP_value` per release; releases under 20 measurements are skipped. The property name is checked against `^\$?[A-Za-z0-9_]+$` before it goes into the query |

Everything is off by default (`integrations.<provider>.enabled`) and behind the `network` feature. Requests go through the `AuthHttp` trait (tests use a fake with recorded replies shaped like the real APIs). The real client is ureq with rustls: HTTPS only, certificate verification, a 15 s timeout, a 4 MB reply cap, the user agent `mulchie/<version>`, and **no redirects** (a 3xx is an error). Before a request is sent, its URL must be `https://` to exactly the provider's configured host (`api.vercel.com`; `sentry.io` or `<region>.sentry.io`; `us.posthog.com` or `eu.posthog.com`), with no port or user info. A request carries the bearer token and the project identifiers from the config, nothing about the machine. Errors name the host and status, never the reply body, and every error string is passed through `scrub`, which replaces the token with `[redacted]`.

Tokens are in `<config dir>/integrations/<provider>.token`, mode 0600 in a 0700 directory, written with `write_atomic` (a symbolic link or non-regular file in its place is refused). Reading opens with `O_NOFOLLOW | O_NONBLOCK`, then refuses a file that is not regular, not owned by the user or readable by others, a looser directory, and anything over 4 KB or with non-printable characters. A token is held in a `Secret`: not `Serialize`, not `Clone`, with a `Debug` that prints nothing of it. Tokens never reach the config, argv (`mulchie integrations add` reads standard input, without echo on a terminal), logs, the journal, findings, API responses or errors; a test checks every response and every file Mulchie wrote after a provider echoed the token.

The `mulchie-integrations` worker runs only on the lock holder. Each enabled provider is polled no sooner than a minute after start, then every `integrations.interval_mins` (default 30, at least 15), doubling after each consecutive failure up to a day. `POST /v1/integrations/check` asks for a poll now, refused when no provider is on, without network support, or within a minute of the last poll. Results (deploys and each provider's last success, last error and failures; never the token) are cached in `<config dir>/integrations.json` (mode 0600). After a poll, a settings change or new recorded commits, the worker rebuilds the `production_slower` findings with `perf::slower` (one slow deploy against the median of up to `integrations.baseline_deploys` before it, `slower_ratio` and `min_increase_ms`). Each names the deploy, the number before and after, and the commits from the previous deploy's SHA to this one's (`commits::between` in the configured workspace, else the workspace whose record has the SHA; at most 20), each with its session and agent, or `made_elsewhere` when this machine never recorded it. `GET /v1/integrations` reports status, `GET /v1/deploys` the deploys with attribution. Nothing acts on a deploy or a finding.

### OpenTelemetry receiver (`otel.rs`, `otel_server.rs`, `endpoints.rs`, `engine_otel.rs`)

Off by default (`otel.enabled`). Only the engine that holds the lock runs the receiver, from the `mulchie-perf` worker, which starts, restarts or stops it within seconds of a config change (a config save wakes the worker). A port that cannot be bound is remembered and shown as an `otel_unavailable` posture finding, and tried again at most once a minute. While it runs, the posture check also reports its port (as `api_exposed`, id `api_exposed:otel:<port>`) when something listens on it beyond loopback. Turning it off drops the aggregates. Shutdown stops it.

- `otel_server.rs` is an OTLP/HTTP receiver. `serve` binds only a loopback address and refuses any other (`0.0.0.0`, `::`, LAN addresses, `::ffff:127.0.0.1`); the default is `127.0.0.1:4318`. It takes `POST /v1/traces` with `Content-Type: application/json` or `application/x-protobuf`, and `Host` must name `localhost`, `127.0.0.1`, `[::1]` or `host.docker.internal` (so apps in a container can post where the runtime forwards that name to the host's loopback, as Docker Desktop does; the bind stays loopback). A batch posted as `host.docker.internal` has its `process.pid` dropped, since a pid from inside a container names some other process on the host. A compressed body (`Content-Encoding: gzip`) gets 415, since decompressing would need a new dependency. Answers are 200 with an empty `ExportTraceServiceResponse`, or 400, 403, 404, 405, 408, 411, 413, 414, 415, 431 or 503 with a `google.rpc.Status`-shaped body in the request's encoding. Limits: 8 KB per line, 32 KB of headers, 4 MB body, 10 s to receive a request, 16 connections at once; one request per connection. It runs on its own thread with a stop handle and hands each decoded batch to a `Sink` (`Mutex<Aggregator>` is one).
- `otel.rs` decodes OTLP JSON (serde, folding each array while it is parsed, so spans and attributes past a cap are skipped unparsed and only kept values stay in memory) and protobuf (a hand-written wire-format reader that checks every length and skips array, key-value-list and bytes values unread, so nesting costs nothing). A resource with no spans is not kept. Decoding keeps only the span name, kind, times, status code, whether the span is a root, the span attributes `http.route`, `url.template`, `http.request.method` and `http.method`, and the resource attributes `service.name`, `process.pid`, `service.version`, `vcs.ref.head.revision`, `vcs.repository.ref.revision`, `deployment.environment` and `deployment.environment.name`. `url.full`, `db.statement` and every other attribute are skipped and never stored. Operation names lose their query string and go through `redact.rs`, and long id- or token-shaped path segments become `[redacted]`.
- `Aggregator` keeps server, consumer and root spans per (`service.name`, operation) per 5-minute window for 24 hours. The operation is `http.route` or `url.template` (after the method), else the span name. Each window holds a count, an error count (status `ERROR`), the sum and maximum duration, a fixed log-scale histogram of 133 buckets (each at most 25% wide, so windows merge exactly for p50, p95 and p99), and the resource attributes last seen. A span is placed by its end time; one ending more than a minute in the future counts at the time it was received.
- Caps: 20,000 spans per request, 128 attributes looked at per span or resource, 256 bytes per kept string, 32 services, 128 operations per service, 8,192 windows in all (about 4.4 MB of histograms). Past a cap the span or value is dropped and counted in `Drops`. Windows older than 24 hours roll off, and operations and services left with none are removed, which frees their place under the caps.
- Once a minute (`engine_otel.rs`) the worker attributes each service: its `process.pid`, when that process belongs to this user, through the process's working directory to the checkout; else its `vcs.ref.head.revision` matched against `commits.jsonl`. It reads that checkout's `HEAD` through the commit attribution path (a move is recorded with `via: otel`), and the checkout joins the workspaces polled every `perf.head_check_secs`. `Engine::attribute` names the session that moved `HEAD` to the running commit.
- `endpoints.rs` (pure) compares each operation's last 15 minutes (windows merged, so p95 comes from the merged histogram) with its baseline: the median p95 and error rate of the earlier windows with at least `otel.min_requests` requests, at least three of them. Slower: p95 above `perf.slower_ratio` times the baseline and more than `otel.min_slowdown_ms` above it, with at least `otel.min_requests` recent requests. Failing more: the error rate above `perf.slower_ratio` times the baseline rate and more than 5 points above it. Each raises a `performance` finding (`endpoint_slower`, `endpoint_errors`) whose evidence holds the figures, the attribution and the recorded moves of `HEAD` from the baseline's revision (the app's, else `HEAD` recorded at the baseline's end) to the running one. They are stored together with `build_slower` (one category, replaced at once) and tracked as issues keyed on service and operation.
- `GET /v1/otel` reads the aggregator and the cached attribution: no git and no process table on the API path or the receiver thread.
- None of these modules deletes files or signals processes.

### git

All git calls go through one helper: fixed arguments `-c core.fsmonitor=false -c core.hooksPath=/dev/null`, a 30 s timeout, and a pool of at most 8 concurrent repos. Scanning a repo never runs code from that repo.

### Configuration

`<config dir>/config.json` (`~/.config/mulchie`, or under `$XDG_CONFIG_HOME`), with a `version` field. Loading validates ranges; an invalid or unparsable file is reported in state and in `mulchie doctor`, and is never overwritten by a save. Saves are atomic (temp file, then rename). `MULCHIE_PORT` affects the running process only.

Writes go through the engine that holds the lock, one read-modify-write under a config lock:

- `PATCH /v1/config` applies a JSON merge patch to the file's current content. `PUT /v1/config` replaces the config.
- `GET /v1/config` returns a `revision`, a 64-bit FNV-1a hash of the file's bytes as 16 hex digits. `PUT` and `PATCH` with it (`If-Match` or `if_revision`) write only if the file is still at that revision, else 409 `config_changed`.
- A `PUT` also refuses when the file changed since the engine loaded it.
- A key the config does not have is 400 `unknown_key`, naming its path.

### HTTP API

- TCP on `127.0.0.1` with a bearer token. This stays because agents and scripts call it with `curl`.
- Every route needs the token except `GET /v1/health`. The full token is accepted everywhere. The read-only token is accepted for every `GET`, the plan routes, build leases with a TTL, and the hook routes (`POST /v1/hook`, `POST /v1/guard/check`).
- Requests with an `Origin` header are refused. `Host` must be `127.0.0.1:<port>` or `localhost:<port>`.
- Limits: 8 KB per line, 64 KB of headers, 1 MB body, 10 s to receive a request, 64 concurrent connections.
- The token files are created with mode `0600` in a `0700` directory; a token file with looser permissions or another owner is refused.
- `docs/openapi.yaml` describes every route. A test keeps it in step with `ROUTES` in `api.rs`.

### Build queue

A lease needs a live pid or a TTL. `mulchie run` leases against its own pid, forwards SIGINT and SIGTERM to the child and exits with the child's status (128 + signal when signalled). MCP leases carry a TTL (at most 3600 s over MCP) and expire if not renewed. Releasing or renewing a ticket needs the secret returned when it was created; `mulchie slots release` uses the full token instead.

### Platforms

| | macOS | Linux |
| --- | --- | --- |
| Core, CLI, `serve`, MCP, hooks | yes | yes |
| Menu bar app | yes | no |
| Notifications | app, or `osascript` from `serve` | `notify-send` when present |
| Service | launchd agent | systemd user unit |

Windows is out of scope.

## Clients

**CLI.** Parsed with clap: per-command help, shell completions, strict flags. Read commands fall back to a local scan when no engine is running and say so. Commands that need the engine (`signal`, `group`, `sessions`, `limits`, `updates`, `guard`, `slots`, queueing in `run`) say that it is not running and how to start it. `mulchie doctor` checks the install end to end. `mulchie service install` sets up the headless engine.

**MCP.** Stdio JSON-RPC. Negotiates the protocol version, marks each tool read-only or destructive with annotations, and returns spec-correct errors. The server keeps each plan it shows in memory; `execute_plan` accepts only ready items of a stored plan, once, within 30 minutes. `--read-only` connects with the read-only token and offers only tools that change nothing, plus the two build-slot tools (`check_updates` and `check_integrations`, which reach the network from the engine, are not offered). `list_timings` lists build and test timings. Tools for sessions, limits, updates and guard checks are `list_sessions`, `get_session`, `limits_status`, `list_updates`, `check_updates` and `check_command`; `list_deploys` and `check_integrations` are for production integrations.

**App.** Tray panel and dashboard. The UI is static files (HTML, CSS, ES modules) with no inline script, so the CSP has no `'unsafe-inline'` for scripts. In-app updates use the Tauri updater plugin (`crates/mulchie-app/src/update.rs`), which reads the `latest.json` that `mulchie_core::updates::app_manifest_url` picks: it lists the newest ten releases through the GitHub API and takes the newest one with a manifest, pre-releases included only when the installed app is itself a pre-release. GitHub's `releases/latest` never names a pre-release, so the endpoint in `tauri.conf.json` is not used: `update.rs` hands the updater the URL it picked. The plugin is registered only when `plugins.updater.pubkey` is set in `tauri.conf.json`; without it "Check for Updates…" says this build cannot update itself and offers the releases page. "Check for Updates…" is in the app menu (set in `main.rs`, with the standard About, Services, Hide and Quit items and Edit and Window menus) and in the tray menu. A build with the key also checks a minute after launch and every six hours, and the window shows an update button while a newer version is known (`app_update`, `start_update`); a build without it takes the newer version from the engine's own update report.

## Distribution

Merging a version change to `main` produces everything, including the tag `vX.Y.Z`. The version lives in the workspace `Cargo.toml` only (and `server.json` follows it; a test checks).

| Artifact | Built by | Reaches users as |
| --- | --- | --- |
| `Mulchie_X.Y.Z_universal.dmg`, signed and notarized | `release.yml`, macOS runner | Download button on mulchie.com; `brew install --cask williamrogersdev/tap/mulchie` |
| `mulchie-<target>.tar.gz` + `.sha256` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `release.yml` matrix | `curl -fsSL https://mulchie.com/install.sh \| sh`; `brew install williamrogersdev/tap/mulchie-cli` |
| `latest.json` + updater signatures | `release.yml` | In-app updates |
| crates `mulchie-core`, `mulchie` | `publish.yml`, called by `release.yml` after it publishes the release | `cargo install mulchie` |

No person tags or publishes. `main.yml` runs CI on every push to `main` and starts `release.yml` when the push changed the workspace version; `release.yml` creates the tag and the published release together, then calls `publish.yml` for the Homebrew tap bump and the crates.io publish. In a full release the macOS CLI binaries are signed with the Developer ID certificate, so the standalone tarball runs without a Gatekeeper prompt; in a CLI-only release they are ad-hoc signed. `docs/RELEASING.md` has the steps.

## Public site

`site/` is an Astro static site deployed to Cloudflare Workers static assets on mulchie.com.

- `/`: what Mulchie is, who it is for, install commands.
- `/install/`: every install path, verification, update, uninstall.
- `/docs/…`: Starlight. Getting started, sessions and limits, hooks and guard rules, update checks, production integrations, CLI, MCP, HTTP API, configuration, findings, safety model, headless, troubleshooting, FAQ.
- `/install.sh`: the installer script, copied from `scripts/install.sh` at build time.

SEO is part of the build, and `scripts/check-seo.mjs` fails it on a missing or duplicate title or description, a broken link or anchor, a missing canonical, and scripts on the landing and install pages: one canonical URL per page, a sitemap, `robots.txt`, Open Graph images generated at build time, `SoftwareApplication` JSON-LD on the landing page, and `llms.txt`. Pages ship no JavaScript except docs search.

Docs describe only behaviour the code has.

## Quality gates

CI on every pull request, on macOS and Ubuntu: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, the MCP smoke test over real stdio, and the discovery test (`openapi.yaml` routes, `server.json` version). Ubuntu also runs clippy for `--no-default-features`. Scripts and workflows are checked with ShellCheck and actionlint, and the site workflow builds the site. Tests never touch the real home directory: paths, clock and process table are injected, and a test that starts an engine first writes a config with build throttling off, no budgets, the memory guard and runaway chains off and update checks and integrations off, so it cannot renice or close real processes or reach the network. The safety gate is tested against real temporary git repositories, one test per rule above.
