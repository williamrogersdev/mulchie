---
title: Sessions and limits
description: "Agent sessions, per-session budgets with notify, deprioritize, pause or stop, and the activity record. What each limit does and where it does not apply."
---

Use this page to see what each agent is using, set a budget for it, and read what it did.

Mulchie is a supervisor, not a sandbox and not an antivirus. It watches agents on your machine, tells you and them when one is causing trouble, and acts only where you configured it to. An agent that is determined to avoid Mulchie can.

## Sessions

A session is an agent and everything it started. Mulchie finds the agent in the process table by its name, its executable or the script a runtime such as `node` runs. Everything below that process belongs to the session. The workspace is the git checkout of the agent's working directory.

The built-in names are `claude`, `codex`, `gemini`, `copilot`, `cursor-agent`, `aider`, `goose` and `opencode`. These names are also built in, taken from the tools' package names and common installs and not checked against vendor documentation: `droid`, `kimi`, `amp`, `qwen`, `crush` and `cline`. Add your own with `agents.extra`, a list of executable file names:

```sh
mulchie config set agents.extra '["my-agent"]'
```

A session's id is `s-<start time>-<pid>` of its root process, so a reused process id is a new session. An agent started by another agent is its own session, and no process is counted twice. A session ends when its root process exits. Ended sessions stay in the list for a while.

For each session Mulchie keeps CPU cores in use, memory, process count, bytes written (and the write rate per hour, on systems that report per-process writes), runtime, build slots held, listening ports from the last watch run, and the highest value of each.

```sh
mulchie sessions                              # every session, running first
mulchie sessions s-1700000000-4242            # its processes, breaches, what Mulchie paused
mulchie sessions s-1700000000-4242 --events   # its activity record
```

Over MCP these are `list_sessions` and `get_session`. Over HTTP, `GET /v1/sessions` and `GET /v1/sessions/{id}`. Sessions need an engine. Setting `sessions.enabled` to `false` turns off sessions, the record and limits.

A session can also be registered by an agent hook (see [Hooks and guard rules](/docs/hooks/)) when no agent process matched. Such a session is reported but never limited (see below).

## Limits

A budget sets caps for one session. A cap is a ceiling, how long the session must stay over it (`sustain_secs`, default 60) and one action.

| Cap | Unit |
| --- | --- |
| `cpu_cores` | Cores in use. 1.0 is one full core. |
| `memory_mb` | Memory of all the session's processes, in MB. |
| `processes` | Number of processes. |
| `write_mb_per_hour` | MB written per hour, over the last minute. Not checked where the system does not report writes. |
| `runtime_hours` | Hours since the session started. |

| Action | What happens |
| --- | --- |
| `notify` (default) | A finding of kind `limit_exceeded` and, if notifications are on, a notification. Nothing is signalled. |
| `deprioritize` | Lowers the priority of the session's processes (nice 10, or `build_nice` if higher). When the session is back under the cap Mulchie tries to put it back. Raising priority usually needs privilege, so this can fail, and the breach says so. |
| `pause` | Sends SIGSTOP to the session's processes. They are resumed with SIGCONT after `resume_after_secs` (default 300), and when Mulchie shuts down or starts after a crash. |
| `stop` | Sends SIGTERM, then SIGKILL after `stop_grace_secs` (default 10) to what is still running. |

### Set a budget

`limits.default` is the budget for every session. `limits.workspaces` is a list of `{path, budget}` entries for sessions working at or under a path. The entry with the longest matching path replaces the default completely. Caps are not merged.

```sh
mulchie config set limits.default '{"memory_mb": {"max": 8192, "sustain_secs": 120, "action": "pause"}, "cpu_cores": {"max": 6}}'
mulchie limits
```

The `cpu_cores` cap above has no action, so it is `notify`. Remove a budget with `mulchie config set limits.default null`.

### What limits do not do

- **They report by default.** With no budget, or with caps whose action is `notify`, nothing is ever signalled. `mulchie limits` says "Report only".
- **The agent itself is spared.** `deprioritize`, `pause` and `stop` act on what the agent started, not on the agent's own root process, unless the budget sets `include_agent` to `true`. The agent stays alive so a hook or `diagnose` can tell it what happened. If a session has nothing but the agent, the breach is recorded as skipped.
- **Hook-registered sessions are reported, never enforced.** A hook call can come from any client holding the read-only token, so a session that exists only because of a hook is only noted.
- **Only the engine that holds the lock acts.** When the app and `mulchie serve` both run, one holds the lock. The other reports and never signals. The same applies while the config file fails to load. `GET /v1/limits/status` and the `limits_status` tool give the reason as `not_enforcing_reason`: `not_lock_holder` or `config_error`.
- **A pause ends.** Processes are resumed `resume_after_secs` after the pause, whether or not the session is back under the cap. If it is still over, a new breach and a new pause can follow after `sustain_secs`.
- **Every action goes through the safety gate.** The gate checks the owner, refuses pid 1 and Mulchie's own processes, and compares each process's start time with the one sampled, so a reused process id is skipped. Every action is written to `actions.jsonl` and to the session's record.

A breach is handed to the agent once, through its next hook call, and appears in `diagnose`, `mulchie limits`, the MCP `limits_status` tool and `GET /v1/limits/status`.

### Hogs

With or without budgets, a session that uses at least `limits.hog_share_pct` (default 50) of the machine's CPU or memory for `limits.hog_sustain_secs` (default 120) while the machine is strained or critical becomes a `session_hog` finding. This only reports.

## The memory guard

Limits look at one session. The memory guard looks at the whole machine: when memory stays low, it finds the biggest consumer it may close. By default it only reports (a `memory_low` finding and a notification that names what it would close). To let it close things:

```sh
mulchie config set memory_guard.action stop
```

- **When it fires.** Less than `memory_guard.free_pct` (default 5) percent of memory is available, or the kernel reports critical memory pressure, for `sustain_secs` (default 15).
- **What it closes.** The biggest unit using at least `min_mb` (default 300). With `scope: sessions` (the default) a unit is everything one agent session started; the agent itself is spared unless `include_agent` is set, so the agent sees its build or test run end and can carry on. With `scope: all` a unit can also be any other app, workspace or program of yours. That can be an app you are using, and unsaved work in it is lost.
- **How.** SIGTERM to every process of the unit, then SIGKILL after `stop_grace_secs` (default 5) to what is left. Then it waits `cooldown_secs` (default 30) and, if memory is still low, closes the next biggest.
- **A cap per unit.** `unit_max_mb` closes (or reports) any unit that stays over that much memory, even when the machine is not low. 0, the default, turns it off.
- **What it never closes.** System processes, other users' processes, Mulchie, whatever started Mulchie, the terminal, editor or app an agent session runs in, sessions that only a hook registered, and any process named in `memory_guard.never`. Every signal goes through the safety gate, which checks each process again by its start time.
- **Only the engine that holds the lock acts.** Any other engine reports.

`mulchie limits` shows whether the guard acts, whether memory is low and what it closed. Closings are also written to `actions.jsonl` and, for a session, to its activity record as `memory_guard` events. Closing a process loses whatever it had not saved; the guard is a last resort that trades one process for a machine that keeps running.

### Act on a session yourself

```sh
mulchie sessions stop s-1700000000-4242 --dry-run
mulchie group session:s-1700000000-4242 pause
```

Both show a plan and ask first, like every action on processes. The agent's own process is included only with `--include-agent`. Over MCP, `plan_process_action` takes a `session` and an `action`.

## The activity record

Each session has an append-only record, one JSON event per line, in `<config dir>/sessions/<id>.jsonl`. The directory is mode `0700` and the files `0600`. Only the engine that holds the lock writes it, and only while `sessions.enabled` is on.

It holds these events:

| Event | What it records |
| --- | --- |
| `session_start`, `session_end` | The agent, root process, workspace and, at the end, runtime, peaks and bytes written. |
| `process_start` | A process that joined the session: pid, name and command line. At most `sessions.process_events_per_minute` a minute; the rest are summed in a `process_starts_summarised` event. |
| `limit_breach`, `limit_action`, `limit_cleared` | A cap that was exceeded, what Mulchie did about it, and when it ended. |
| `memory_guard` | The memory guard closed what the session started: why, how much memory, and the outcome. |
| `hook_session_start`, `hook_prompt`, `hook_pre_tool`, `hook_post_tool`, `hook_stop`, `hook_session_end` | What the agent's hooks reported: the client, the tool, the command, the guard's decision and the rule. |
| `truncated` | The record hit a size cap. |

Read it with `mulchie sessions <id> --events`, the MCP `get_session` tool with `events=true`, or `GET /v1/sessions/{id}/events`. Do not scan the files.

Limits on the record, all settings under `sessions` (see [Configuration](/docs/configuration/#sessions)):

- One record stops growing at `max_record_mb` (default 4) or `max_events` (default 20000). Every string in an event is cut at 4096 bytes.
- Records of ended sessions are deleted after `retain_days` (default 14), and the oldest ones go first when all records together pass `max_total_mb` (default 200). The engine checks this hourly. Deleting these files is the one thing besides the safety gate that Mulchie deletes, and they are Mulchie's own.

What the record is not:

- **Command lines are redacted, as well as Mulchie can.** Flags and variables with secret names, bearer headers, credentials in URLs and well-known token shapes are replaced. A secret passed as a bare argument with no telling flag, in a shape Mulchie does not know, is not recognised.
- **Prompt text is never in it.** Hooks do not send it to the engine.
- **It is not tamper-proof.** The engine writes what its callers report. An event from a hook caller other than the client the session is bound to is stored with `unbound: true`, but a process of the same user that learns the bound client's id can still pose as it. Treat the record as a log for you, not as evidence.
- **Process starts are seen on a sample.** A process that starts and ends between two samples (every 3 seconds) can be missed.
