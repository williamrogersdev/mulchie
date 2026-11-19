---
title: Production integrations
description: "Read-only links to Vercel, Sentry and PostHog: which deploy made production slower, the commits in it and the agent sessions that made them. Off by default."
---

Use integrations to find out which deploy made production slower, and which commits and agent sessions were in it. Mulchie reads deploys and one performance number per deploy from tools you already use, matches each deploy's commit to the commits it saw agents make on this machine, and raises a `production_slower` finding when a deploy made a number worse.

Integrations are **read-only** and **off by default**. Mulchie never deploys, rolls back or changes anything at a provider. Like every finding, `production_slower` only reports.

## Connect a provider

```sh
mulchie integrations add vercel --project prj_123 --team team_456
mulchie integrations add sentry --org acme --project web
mulchie integrations add posthog --project 12345 --release-property commit_sha
```

The token is read from standard input, never from an argument. On a terminal, Mulchie asks for it and does not echo it. You can also pipe it in:

```sh
op read op://dev/sentry/token | mulchie integrations add sentry --org acme --project web
```

`--workspace DIR` names a checkout of the project on this machine, for commit attribution. Without it, Mulchie looks in every workspace whose recorded commits include the deploy's commit.

```sh
mulchie integrations list           # each provider: on or off, token saved, last poll
mulchie integrations deploys        # recent deploys, their numbers, the commits in them
mulchie integrations check          # ask the engine to poll now
mulchie integrations remove sentry  # turn it off and delete its token
```

Over MCP, use `list_deploys` and `check_integrations`. Over HTTP, use `GET /v1/integrations`, `GET /v1/deploys` and `POST /v1/integrations/check`.

## What each provider gives

| Provider | Deploys | Number per deploy | Token |
| --- | --- | --- | --- |
| Vercel | Production deployments in state `READY` (`GET api.vercel.com/v7/deployments`), with the commit from `meta.githubCommitSha` (or the GitLab or Bitbucket field). | **Build duration**: `ready` minus `buildingAt`. | An access token from your Vercel account settings. |
| Sentry | Releases of the project (`GET /api/0/organizations/{org}/releases/`). The commit is `lastCommit.id` when Sentry has commits for the release, or else the version itself when it is a SHA (or ends in `@<sha>`). | **p95 of transaction duration** per release over the last 30 days, from the events API on the spans dataset (`p95(span.duration)` with `is_transaction:true`). Releases with fewer than 20 transactions are left out. | An auth token with `org:read` and `project:read`. |
| PostHog | Values of the release property (`release_property`, default `$app_version`) on `$web_vitals` events in the last 30 days. The release's time is when it was first seen. A value that is a SHA is its commit. | **LCP and INP at the 75th percentile**, from a HogQL query (`POST /api/projects/{id}/query/`). Releases with fewer than 20 measurements are left out. | A personal API key with the Query Read scope. |

:::caution[Vercel's number is build time]
Vercel's REST API has no stable per-deployment latency metric: Speed Insights data leaves Vercel only through drains. So for Vercel Mulchie measures how long each deploy took to **build**, not how fast it serves. The finding says so.
:::

PostHog's web SDK does not set `$app_version` on its own. Register a property with your commit SHA (for example `commit_sha`) on every event and pass `--release-property commit_sha`. Only letters, digits and `_`, with an optional leading `$`, are accepted, because the name goes into the query as it is.

## When a deploy counts as slower

A deploy is slower when its number is more than `integrations.slower_ratio` times (default 1.5) the median of the `integrations.baseline_deploys` deploys before it (default 5, at least 3 are needed), and more than `integrations.min_increase_ms` higher (default 100 ms). Several slower deploys in a row are dated from the first one. The statistics are the same as for [slower builds](/docs/watch/#slower-builds).

The finding's title reads like *"Production transaction p95 on Sentry went from 120 ms to 380 ms after deploy abc1234"*. Its evidence (`evidence.deploy`) holds the provider, the deploy and the one before it, the number before and after, and the commits in the deploy: the moves of `HEAD` this machine recorded from the previous deploy's commit to this one's, each with the agent session and agent working there then. A commit this machine never recorded is listed as `made_elsewhere`. Attribution is a heuristic: it names who was working in the workspace when `HEAD` moved, not who wrote the code. See [Which session moved HEAD](/docs/watch/#which-session-moved-head).

`production_slower` findings are in the `performance` category and are grouped into [issues](/docs/watch/#issues) by provider and metric.

## What goes over the network

Only for providers you turned on, only from the engine that holds the lock, and only in a build with network support:

- authenticated read requests to that provider's own API host: `api.vercel.com`, your Sentry host (`sentry.io` or a region such as `de.sentry.io`) or your PostHog host (`us.posthog.com` or `eu.posthog.com`). For PostHog this is a `POST` of a HogQL query, which reads and changes nothing;
- over HTTPS with certificate verification, a 15 second timeout and a cap on the reply size. No redirect is followed, so the token only goes to the host asked for;
- carrying the token and the project identifiers from the config, and nothing about this machine.

Each enabled provider is polled no sooner than a minute after the engine starts, then every `integrations.interval_mins` minutes (default 30, at least 15). After a failed poll the wait doubles each time, up to a day. `mulchie integrations check` polls now, no more than once a minute. A build without the `network` feature polls nothing and says it has no network support.

## Where the token is kept

Each token is in `integrations/<provider>.token` in the config directory, mode `0600` in a `0700` directory, written through a temporary file and a rename. Mulchie refuses to read it when it is a symbolic link, not a regular file, owned by someone else or readable by others. The token is never in `config.json`, in command arguments, in logs, in the action journal, in findings, in API responses or in error messages; a provider error that echoes it is cleaned before it is kept. `mulchie integrations remove` deletes the token file (only a regular file of that name; a symbolic link is left alone).

The last poll of each provider (its deploys, numbers and status, never the token) is kept in `integrations.json` in the config directory, mode `0600`, so a restart shows it without asking the network again.

## Settings

See [`integrations`](/docs/configuration/#integrations) in the configuration reference.
