---
title: HTTP API
description: "The Mulchie local HTTP API on 127.0.0.1: tokens, every endpoint, plan-then-execute requests, and build leases, for scripts and tools that do not speak MCP."
---

Use the HTTP API from scripts and tools that do not speak MCP. The CLI, the app and the MCP server all use it too.

The full description, with request and response shapes, is [docs/openapi.yaml](https://github.com/williamrogersdev/mulchie/blob/main/docs/openapi.yaml) in the repository.

## Connect

- Base URL: `http://127.0.0.1:47474`. The port is `api_port` in the config. `MULCHIE_PORT` sets another port for one engine process.
- The engine listens on `127.0.0.1` only.
- Send `Authorization: Bearer <token>`. `GET /v1/health` is the only route that needs no token.
- Requests with a browser `Origin` header are refused. The `Host` header must be `127.0.0.1:<port>` or `localhost:<port>`.

```sh
curl -H "Authorization: Bearer $(cat "$(mulchie token)")" \
  http://127.0.0.1:47474/v1/advice
```

The engine must be running (the app, `mulchie serve` or the service).

## Tokens

The engine writes two tokens when it starts, in the config directory (`~/.config/mulchie/`). `mulchie token` prints the path of the full token, and `mulchie token --read-only` the path of the read-only one. Add `--show` to print the token itself.

| Token | File | Accepted for |
| --- | --- | --- |
| Full | `token` | Every route. |
| Read-only | `token.read` | Every `GET` route, `POST /v1/clean/plan`, `POST /v1/worktree/plan`, `POST /v1/process/plan`, and `POST /v1/lease` (only with `ttl_secs` and no `pid`) and `POST /v1/release`. Any other route answers 403 `read_only_token`. |

Both files have mode `0600` in a `0700` directory. A token file that other users can read, or that another user owns, is refused.

## Limits

Requests are capped at 8 KB per line, 64 KB of headers and a 1 MB body. The server allows 64 concurrent connections and gives a client 10 seconds to send a request.

## Endpoints

| Method | Path | What it does |
| --- | --- | --- |
| GET | `/v1/health` | Open. Health, disk, load, free build slots, finding counts. |
| GET | `/v1/state` | Everything the engine knows: snapshot, consumer groups, health, build slots, forecast, last scan, disk map, recent actions and findings. |
| GET | `/v1/history` | Samples from the last 30 minutes, every 3 seconds. |
| GET | `/v1/advice` | Whether to start heavy work now, free slots and the suggested job count. |
| POST | `/v1/scan` | Queue a scan of build caches and worktrees. |
| GET | `/v1/clean/plan` | Plan deleting stale build caches. |
| POST | `/v1/clean/plan` | Plan deleting specific caches (`{"paths": [...]}`) or stale ones (`{"stale": true}`). |
| POST | `/v1/clean` | Delete build caches from approved plan items. |
| GET | `/v1/sweep` | Plan removing idle agent worktrees. |
| POST | `/v1/sweep` | Remove approved idle agent worktrees. |
| POST | `/v1/worktree/plan` | Plan removing specific worktrees (`{"paths": [...], "allow_dirty": false}`). |
| POST | `/v1/worktree/remove` | Remove approved worktrees. |
| POST | `/v1/prune` | Run a package manager's own prune command (`{"key": "npm"}`). |
| POST | `/v1/process/plan` | Plan acting on a process, a process tree or a group. |
| POST | `/v1/process/execute` | Run approved process requests from a plan. |
| GET | `/v1/process/{pid}` | Everything known about one process. |
| POST | `/v1/process` | Signal or renice one process (`{"pid", "action"}`). |
| POST | `/v1/workspace` | Act on a workspace's processes (`pause`, `resume`, `deprioritize`, `stop_builds` or `kill_all`). |
| GET | `/v1/findings` | What the watch found. Query: `severity`, `kind`, `dismissed=1`. |
| POST | `/v1/findings/dismiss` | Hide a finding (`{"id", "days"}`). |
| POST | `/v1/findings/undismiss` | Show a dismissed finding again (`{"id"}`). |
| GET | `/v1/config` | The config in use, why the file was not used (if so), and its path. |
| PUT | `/v1/config` | Replace the config (`{"config": {...}}`). Full token only. |
| POST | `/v1/lease` | Join the build queue, poll a ticket, or renew a slot. |
| POST | `/v1/release` | Release a slot or leave the queue. |

Errors are JSON: `{"error": "...", "code": "..."}`, with status 400 for a bad body, 401 for a bad token, 403 for a refused origin, host or read-only token, and 404, 405, 413 or 500 otherwise.

## Plan, then execute

Deleting and signalling take two requests. Get a plan, show it, then post back the approved items.

1. Ask for a plan: `GET /v1/sweep`, `GET` or `POST /v1/clean/plan`, `POST /v1/worktree/plan`, or `POST /v1/process/plan`. Nothing changes. Each item has a `request` object, a `ready` flag and, if it is not ready, a reason.
2. Post the `request` objects of the items you approve, unchanged, as `{"items": [...]}` to `POST /v1/clean`, `POST /v1/sweep`, `POST /v1/worktree/remove` or `POST /v1/process/execute`.

The response has one result per item, in order: `done`, with any backups and the bytes freed, or `skipped` with a reason. The engine's safety gate checks every item again when it runs, so an item whose facts changed since the plan is skipped. See the [safety model](/docs/safety/).

`POST /v1/clean` also takes `{"paths": [...]}`, which deletes those cache directories directly, because an explicit list is its own approval. The gate still checks each path.

## Build leases

`POST /v1/lease` needs a live `pid`, a `ttl_secs` (1 to 86400), or both. It returns `granted`, `ticket`, `jobs` and `position`. Poll by sending the `ticket` again until `granted` is true. A lease with a `ttl_secs` must be renewed the same way before it expires, or it is freed.

The reply that creates a ticket also carries a `release_secret`. Only that caller sees it. Send it as `secret` when you poll, renew or call `POST /v1/release`; without it the reply is 403 `release_secret_required`.

`mulchie run` leases against its own pid. A lease for a pid is freed when that process ends.
