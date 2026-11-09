---
title: HTTP API
description: "The Mulchie local HTTP API on 127.0.0.1: authentication, endpoints for state, advice, clean, sweep, process control and build leases."
---

The engine serves a JSON API for scripts and tools that do not speak MCP.

## Connect

- Base URL: `http://127.0.0.1:47474`. Set `MULCHIE_PORT` to use another port.
- It listens on `127.0.0.1` only.
- Send `Authorization: Bearer <token>`. The token is in `~/.config/mulchie/token`, a file with mode `0600`.
- `GET /v1/health` is the only route that needs no token.
- Requests with a browser `Origin` header are refused.

```bash
curl -H "Authorization: Bearer $(cat ~/.config/mulchie/token)" \
  http://127.0.0.1:47474/v1/advice
```

<!-- VERIFY: README says the token file is ~/.config/mulchie/token. Confirm the path and the request limits below in mulchie-core/src/api.rs. -->

## Endpoints

| Method | Path | Body or result |
| --- | --- | --- |
| GET | `/v1/health` | Open. Health, disk, load and free build slots. |
| GET | `/v1/state` | Full snapshot. |
| GET | `/v1/history` | 30 minutes of 3 s samples. |
| GET | `/v1/advice` | `ok_to_start_heavy_work`, `suggested_jobs`, `reasons`. |
| GET | `/v1/sweep` | Worktrees a sweep would remove. |
| POST | `/v1/scan` | Rescan caches and the disk map. |
| POST | `/v1/clean` | `{paths}` or `{stale: true}` |
| POST | `/v1/process` | `{pid, action, nice?}` where `action` is `term`, `kill`, `stop`, `cont` or `renice`. |
| POST | `/v1/workspace` | `{path, action}` where `action` is `pause`, `resume`, `deprioritize`, `stop_builds` or `kill_all`. |
| POST | `/v1/worktree/remove` | `{paths, backup}` |
| POST | `/v1/sweep` | Remove idle agent worktrees. |
| POST | `/v1/prune` | `{key}` |
| POST | `/v1/lease` | `{label, pid, ticket?}`. Returns `granted`, `jobs`, `position`. |
| POST | `/v1/release` | `{ticket}` |

## Limits

Requests are capped at 8 KB per line, 64 KB of headers and a 1 MB body. The server allows 64 concurrent connections and gives a client 10 seconds to send a request. The `Host` header must be `127.0.0.1:<port>` or `localhost:<port>`.

## Plan, then execute

`GET /v1/sweep` returns the plan. `POST` takes the items you approved. Every item in the response is `done` or `skipped` with a reason. See the [safety model](/docs/safety/).

<!-- VERIFY: the exact request and response shapes for /v1/clean, /v1/sweep and /v1/worktree/remove are changing with plan-then-execute (items carry the HEAD commit the caller saw). Update from mulchie-core/src/api.rs. -->

## Build leases

A lease needs a live process id or it expires. `mulchie run` leases against its own pid. Call `/v1/release` with the ticket when the build ends.
