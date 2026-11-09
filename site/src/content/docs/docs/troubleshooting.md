---
title: Troubleshooting
description: "Fix common Mulchie problems: engine not running, port in use, token refused, a skipped sweep, an invalid config, and a stale health status."
---

<!-- VERIFY: messages quoted in prose are paraphrased. Replace with the exact text the CLI prints once the CLI rework lands. -->

Start with `mulchie doctor`. It checks the install end to end and reports what is wrong.

## "Engine not running"

Commands such as `signal` and queueing in `run` need the engine. Open the Mulchie app, or run `mulchie serve`, or set up a [service](/docs/headless/). Read commands such as `status` and `worktrees` still work by scanning locally, and say so.

## The API port is in use

The default port is 47474. Set another one with the `api_port` setting, or start a process with `MULCHIE_PORT=<port>`. The CLI and MCP server must use the same port as the engine.

## The API rejects my request

- `401`: send `Authorization: Bearer $(cat ~/.config/mulchie/token)`.
- Refused with a browser `Origin` header: the API is not meant for web pages. Call it from a script or `curl`.
- Refused for the `Host` header: use `127.0.0.1:<port>` or `localhost:<port>`.
- Token file refused: it must be owned by you with mode `0600`, in a directory with mode `0700`. Fix with `chmod 600 ~/.config/mulchie/token`.

## A sweep or clean skipped an item

Each skipped item has a reason. The usual ones:

- A process is running in the project. Stop it and run again.
- The worktree `HEAD` moved since you saw it. Rescan and review the plan again.
- Ignored files are larger than `backup_ignored_max_mb`, so the backup was not made. Remove or move the large files, or raise the limit.
- The path is not under a scan root, or is a symlink.
- The directory is not a recognised cache. A `target/` without `CACHEDIR.TAG` is left alone.

Look in `~/.config/mulchie/actions.jsonl` for the record of every executed and skipped action.

## Where did my uncommitted work go?

If a worktree was removed, look in `~/.config/mulchie/backups/` for a patch, and for a `tar.gz` of ignored files. A removed detached worktree also leaves a `backup/<name>-<shortsha>` branch in the repo. Apply a patch from inside the repo with `git apply <patch>`.

## Health says "stale"

`/v1/health` reports `sample_age_secs` and says `stale` when the last sample is older than 15 seconds. The engine has stopped sampling. Restart the app or the service. If it keeps happening, run `mulchie doctor` and open an issue with the output.

## The config file is ignored

If `config.json` cannot be parsed or has a value out of range, Mulchie reports it in the dashboard and in `mulchie doctor`, uses defaults, and does not overwrite your file. Fix the file, or move it aside and let Mulchie write a new one.

## Memory or CPU looks wrong for a build

Mulchie attributes a process to the repo or worktree of its working directory, then falls back to its parent chain. Tools that run inside a shared directory such as `~/.cargo/registry` are still counted against the worktree that started them. If a process shows no workspace, it has no git working directory in its ancestry.

## Still stuck

Open an issue on [GitHub](https://github.com/williamrogersdev/mulchie/issues) with your OS, the output of `mulchie doctor` and what you expected.
