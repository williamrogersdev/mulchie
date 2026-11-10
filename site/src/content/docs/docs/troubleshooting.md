---
title: Troubleshooting
description: "Fix common Mulchie problems: engine not running, port in use, API errors, a skipped sweep, a broken config, a missing agent connection, a stale status."
---

Start with `mulchie doctor`. It checks the install end to end and prints pass, warn or fail for each check. It changes nothing.

## "no engine is running"

Commands such as `mulchie status` print this on stderr and carry on with a local look:

```
mulchie: no engine is running; sampling this machine locally (start one with `mulchie serve`, `mulchie service install`, or the Mulchie app)
```

That is fine for reading. `signal`, `group` and the build queue in `run` need the engine. Open the Mulchie app, run `mulchie serve`, or set up a [service](/docs/headless/).

If the message says the engine could not be reached, an engine holds the lock but does not answer. Run `mulchie doctor`.

## The API port is in use

The default port is 47474. If another program holds it, `mulchie serve` prints:

```
mulchie: could not start the API on 127.0.0.1:47474: Address already in use
```

Set another port with `mulchie config set api_port <port>`, or start the engine with `MULCHIE_PORT=<port>`. The CLI and MCP server find the engine through its lock file, so they follow its port.

`mulchie serve` also exits with status 1 and says `another Mulchie engine is running` when the app or another `mulchie serve` already holds the lock. Use that engine.

## The API rejects my request

Errors are JSON with a stable `code`:

| Status and code | Cause and fix |
| --- | --- |
| 401 `unauthorized` | Missing or wrong token. Send `Authorization: Bearer <token>`. `mulchie token` prints the path of the token file. |
| 403 `read_only_token` | You used the read-only token on a route that changes things. Use the full token, or only read and plan. |
| 403 `origin_refused` | The request has a browser `Origin` header. The API is not meant for web pages. Call it from a script or `curl`. |
| 403 `bad_host` | The `Host` header must be `127.0.0.1:<port>` or `localhost:<port>`. |

If the engine refuses to start its API because a token file has loose permissions, fix it with `chmod 600` on the file (`token` or `token.read` in the config directory), and `chmod 700` on the directory.

## A sweep or clean skipped an item

Each skipped item has a reason. The usual ones:

- A process is running in the project. Stop it and run again.
- The worktree `HEAD` moved since the plan. Plan again and review.
- Ignored files are larger than `backup_ignored_max_mb`, so no backup could be made. Remove or move the large files, or raise the setting.
- The path is not under a scan root, or is a symlink.
- The directory is not a recognised cache. A `target/` without `CACHEDIR.TAG` is left alone.
- The worktree is not idle past `worktree_idle_days`, or is not an agent worktree.
- The worktree has a checked-out submodule, holds another git repository, has local edits git hides, or is locked.

`actions.jsonl` in the config directory has the record of every executed and skipped action. See the [safety model](/docs/safety/).

## Where did my uncommitted work go?

If a worktree was removed, look in the `backups` folder of the config directory (`~/.config/mulchie/backups/`). There is one folder per removal, with `changes.patch`, `ignored.tar.gz` when there were ignored files, and a `manifest.json` that says how to restore it. A removed detached worktree also leaves a `backup/<name>-<shortsha>` branch in the repo.

To restore, from the repository:

```sh
git worktree add <path> <head or branch>
git -C <path> apply <backup folder>/changes.patch
tar -xzf <backup folder>/ignored.tar.gz -C <path>
```

## Health says "stale"

`/v1/health` reports `sample_age_secs` and says `stale` when the last sample is more than 15 seconds old. The engine has stopped sampling. Restart the app or the service. If it keeps happening, run `mulchie doctor` and open an issue with the output.

## The config file is ignored

If `config.json` cannot be parsed or has a value out of range, Mulchie reports it in `mulchie config`, in `mulchie doctor` and as a `config_not_loaded` finding. It runs on the defaults with automatic reclaim and sweep off. Fix the file, or set a value with `mulchie config set`. See [Configuration](/docs/configuration/).

## `mulchie config set` says "unknown setting"

The key must be one of the names in [Configuration](/docs/configuration/), with a dot for nested settings, such as `watch.runaway_minutes`. `mulchie config` prints every key.

## The agent does not see Mulchie

- Run `mulchie mcp install` with no `--client` to see which agent apps Mulchie found and what it would do.
- Restart the agent app after `mulchie mcp install --client <app>`. In Claude Code, start a new session or run `/mcp`.
- If an agent's tools say the engine is not running, start it. The MCP server needs the engine.
- In `--read-only` mode, tools that delete, signal, prune or dismiss are not offered, and plans have no `plan_id`.
- If `mulchie mcp install` stops because a different `mulchie` entry exists, check it, then add `--force` to replace it.

## A finding I do not understand

Findings are observations from simple heuristics. They are not malware detection. Run `mulchie inspect <pid>` for everything known about the process, and read [Findings](/docs/watch/) for what each kind means. If you know it is expected, dismiss it with `mulchie findings dismiss <id>`.

## Memory or CPU looks wrong for a build

Mulchie attributes a process to the repo or worktree of its working directory, then falls back to its parent chain. If a process shows no workspace, it has no git working directory in its ancestry.

## Still stuck

Open an issue on [GitHub](https://github.com/williamrogersdev/mulchie/issues) with your OS, the output of `mulchie doctor` and what you expected.
