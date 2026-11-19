---
title: Traces
description: "Point a local app's OpenTelemetry exporter at Mulchie to see each endpoint's p95, and which commit and agent made one slower."
---

Use traces to see how fast the endpoints of an app you run locally answer, and to learn when an agent's change made one slower or made it fail more. Mulchie receives OpenTelemetry traces from the app, keeps p50, p95 and p99 per endpoint, and names the commits and agent sessions since the endpoint was last normal. **It only reports**: nothing is stopped, changed or rolled back.

The receiver is **off by default**.

## Turn it on

```sh
mulchie config set otel.enabled true
mulchie traces
```

The engine that holds the lock (`mulchie serve` or the app) starts listening on `127.0.0.1:4318` within a few seconds, without a restart. Setting `otel.enabled` back to `false` stops it and drops what it had. It listens on loopback only, so nothing on the network can send to it.

If another program already uses port 4318 (an OpenTelemetry Collector, say), Mulchie raises an `otel_unavailable` finding and tries again every minute. Stop the other program, or pick another port:

```sh
mulchie config set otel.port 4319
```

## Point an app at it

Mulchie takes OTLP over HTTP at `/v1/traces`, as JSON or protobuf. Set these in the environment the app starts in:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # or http/json
export OTEL_EXPORTER_OTLP_COMPRESSION=none
export OTEL_SERVICE_NAME=shop-web
```

- **Compression must be off.** A gzip body gets 415 (`Unsupported Media Type`).
- **gRPC (port 4317) is not served.** Use an HTTP protocol.
- Only traces are read. Metrics and logs sent to `/v1/metrics` or `/v1/logs` get 404.
- The endpoint name comes from `http.route` (or `url.template`) after the HTTP method, else the span name: `GET /api/orders/{id}`. Query strings are dropped and id- or token-shaped path segments are redacted. Server, consumer and root spans count; client and internal spans inside a trace do not.

### Where the service runs

To name the workspace and commit, Mulchie needs one of these resource attributes:

- `process.pid`: most SDKs send it. Mulchie reads that process's working directory, when the process is yours, and finds the git checkout around it.
- `vcs.ref.head.revision`: the commit the app was built from. Mulchie looks for it among the commits it has seen in your workspaces.

The engine then reads the checkout's `HEAD` and names the agent session that was working there when `HEAD` moved to it. That is a heuristic, not proof of authorship; see [which session moved HEAD](/docs/watch/#which-session-moved-head).

### Apps in a container

The receiver accepts requests that name the host `host.docker.internal`, so a container can export to `http://host.docker.internal:4318`. This works where the container runtime forwards that name to the host's loopback, as Docker Desktop does. Mulchie still listens on `127.0.0.1` only. On Docker Engine for Linux, run the container with `--network=host` and use `http://127.0.0.1:4318`.

A container's `process.pid` names a process inside the container, not on your machine, so Mulchie drops it from requests sent as `host.docker.internal`. Set `vcs.ref.head.revision` (for example `OTEL_RESOURCE_ATTRIBUTES=vcs.ref.head.revision=$(git rev-parse HEAD)`) to get the workspace and commit.

## Read the endpoints

```sh
mulchie traces
mulchie traces --service shop-web --minutes 60
mulchie traces --json
```

Each service shows where it runs and each endpoint with requests in the range: count, errors, p50, p95, p99 and the usual p95, marked `slower` or `more errors`. Agents use the MCP tool `list_endpoints`; over HTTP it is `GET /v1/otel`. `mulchie diagnose` says whether the receiver is listening and lists slower endpoints.

Mulchie keeps 5-minute windows for 24 hours, in memory only: a restart starts empty. It keeps at most 32 services, 128 endpoints per service and 8,192 windows in all; what does not fit is counted as dropped, and `mulchie traces --json` shows the counts.

## Slower endpoints

Once a minute the engine compares each endpoint's last 15 minutes with its usual figures: the median p95 and median error rate of its earlier 5-minute windows with at least `otel.min_requests` requests (default 20). It needs at least three such windows, and at least `otel.min_requests` requests in the last 15 minutes.

- **`endpoint_slower`**: the p95 is more than `perf.slower_ratio` times the usual p95 (default 1.5) and more than `otel.min_slowdown_ms` above it (default 100). For example: "`GET /api/orders` in `shop-web` is 4.2× slower at p95 (420 ms vs 100 ms)".
- **`endpoint_errors`**: the error rate (spans with status `ERROR`) is more than `perf.slower_ratio` times the usual rate and more than 5 percentage points above it.

The evidence names the service, the endpoint, both sets of figures, the workspace, the commit at the end of the usual period (the app's `vcs.ref.head.revision`, else the `HEAD` Mulchie recorded then), the commit running now, and the moves of `HEAD` in between with the agent session that made each. When the commit did not change, the finding says the cause is elsewhere: data, a dependency, configuration or a busy machine. The finding goes away when the endpoint is back to normal. These findings are tracked as [issues](/docs/watch/#issues), keyed on the service and the endpoint.

The settings are under [`otel`](/docs/configuration/#traces) and `perf.slower_ratio`.
