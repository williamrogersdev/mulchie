# Mulchie architecture

This is the target architecture. `docs/PLAN.md` lists the work that gets the code there.

## What the product is

Mulchie keeps one machine healthy while many AI coding agents work on it. It does three jobs:

1. **Monitor**: sample CPU, memory, disk and per-process I/O, and attribute them to a git repo or worktree.
2. **Optimize**: lower build priority under pressure and queue heavy builds machine-wide.
3. **Reclaim**: delete stale build caches and idle agent worktrees without losing work.

The third job deletes directories and the second signals processes. Every design choice below follows from one rule: **a destructive action is decided from facts read at the moment it runs, never from a cached scan.**

## Components

```
                 ┌────────────────────── mulchie-core ──────────────────────┐
                 │                                                          │
 sysinfo, git ──▶│ monitor ──▶ engine (state, optimizer, build queue)       │
 filesystem   ──▶│ reclaim, diskmap (scanners: read-only)                   │
                 │                 │                                        │
                 │                 ▼                                        │
                 │           safety gate ──▶ delete / remove / signal       │
                 │                 │                                        │
                 │                 ▼                                        │
                 │        backups + action journal                          │
                 │                                                          │
                 │ api (HTTP on 127.0.0.1, bearer token)                    │
                 └───────▲──────────────▲───────────────────▲───────────────┘
                         │ in-process   │ HTTP              │ HTTP
                 ┌───────┴──────┐  ┌────┴─────────┐  ┌──────┴───────┐
                 │ mulchie-app  │  │ mulchie CLI  │  │ mulchie mcp  │
                 │ tray + UI    │  │ + `serve`    │  │ (stdio)      │
                 └──────────────┘  └──────────────┘  └──────────────┘
```

| Crate | Package | Role |
| --- | --- | --- |
| `crates/mulchie-core` | `mulchie-core` | Engine, scanners, safety gate, HTTP API and client. No UI. Builds on macOS and Linux. |
| `crates/mulchie-cli` | `mulchie` | The `mulchie` binary: CLI, `serve` (headless engine), `mcp` (stdio server), `service`, `doctor`. |
| `crates/mulchie-app` | `mulchie-app` | Tauri 2 menu bar app for macOS. Hosts an engine in-process and bundles the CLI as a sidecar. Not published to crates.io. |
| `site/` | n/a | Astro site for mulchie.com: landing page, install page, docs. |

Exactly one engine runs per user. The app and `mulchie serve` both take an exclusive lock on `~/.config/mulchie/engine.lock`; the one that loses the lock becomes a client of the other.

## Core

### Engine

- Two worker threads: the **sample loop** (every 3 s) and the **scan loop** (every `scan_interval_mins`, and on demand). Both are supervised: a panic is caught, logged, and the loop restarts with backoff.
- The sample loop never does slow work. Reclaim, sweep and scans are requested through a channel and run on the scan loop.
- State locks recover from poisoning. `/v1/health` reports `sample_age_secs`; health is `stale` when the last sample is older than 15 s, so a frozen engine cannot keep reporting "Good".
- `serve` handles SIGTERM and SIGINT, releases the lock and exits cleanly.

### Safety gate (`safety.rs`)

Every delete, worktree removal and signal goes through this module. Scanners only produce *candidates*. The gate re-reads the facts for each candidate and returns a per-item outcome: `done`, or `skipped` with a reason.

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
- A "missing" worktree is pruned only when git itself reports it prunable, only if it is an agent worktree, and with `--expire`.

Checks for a **signal**:

- The target is owned by the current user, is not pid 1, and is not a Mulchie process (engine, `mcp`, `run`).
- The pid's start time is compared with the snapshot immediately before the signal, so a reused pid is not hit.

Sweep and clean are two-step for interactive callers: `GET` returns a plan, `POST` takes the items the user approved. Automatic sweeps and reclaims build and execute a plan in one step, through the same gate.

Every executed or skipped action is appended to `~/.config/mulchie/actions.jsonl` (rotated at 5 MB).

### git

All git calls go through one helper: fixed arguments `-c core.fsmonitor=false -c core.hooksPath=/dev/null`, a 30 s timeout, and a pool of at most 8 concurrent repos. Scanning a repo never runs code from that repo.

### Configuration

`~/.config/mulchie/config.json`, with a `version` field. Loading validates ranges; an invalid or unparsable file is reported in state and in `mulchie doctor`, and is never overwritten by a save. Saves are atomic (temp file, then rename). `MULCHIE_PORT` affects the running process only.

### HTTP API

- TCP on `127.0.0.1` with a bearer token. This stays because agents and scripts call it with `curl`.
- Every route needs the token except `GET /v1/health`.
- Requests with an `Origin` header are refused. `Host` must be `127.0.0.1:<port>` or `localhost:<port>`.
- Limits: 8 KB per line, 64 KB of headers, 1 MB body, 10 s to receive a request, 64 concurrent connections.
- The token file is created with mode `0600` in a `0700` directory; a token file with looser permissions or another owner is refused.

### Build queue

A lease needs a live pid or a TTL. `mulchie run` leases against its own pid, forwards SIGINT and SIGTERM to the child and exits with the child's status (128 + signal when signalled). MCP leases carry a TTL and expire if not renewed.

### Platforms

| | macOS | Linux |
| --- | --- | --- |
| Core, CLI, `serve`, MCP | yes | yes |
| Menu bar app | yes | no |
| Notifications | app, or `osascript` from `serve` | `notify-send` when present |
| Service | launchd agent | systemd user unit |

Windows is out of scope.

## Clients

**CLI.** Parsed with clap: per-command help, shell completions, strict flags. Read commands fall back to a local scan when no engine is running and say so. Commands that need the engine (`signal`, queueing in `run`) say that it is not running and how to start it. `mulchie doctor` checks the install end to end. `mulchie service install` sets up the headless engine.

**MCP.** Stdio JSON-RPC. Negotiates the protocol version, marks each tool read-only or destructive with annotations, and returns spec-correct errors. Destructive tools go through the same plan-then-execute gate.

**App.** Tray panel and dashboard. The UI is static files (HTML, CSS, ES modules) with no inline script, so the CSP has no `'unsafe-inline'` for scripts. Updates come from the Tauri updater reading `latest.json` on the latest GitHub Release.

## Distribution

One tag, `vX.Y.Z`, produces everything. The version lives in the workspace `Cargo.toml` only.

| Artifact | Built by | Reaches users as |
| --- | --- | --- |
| `Mulchie_X.Y.Z_universal.dmg`, signed and notarized | `release.yml`, macOS runner | Download button on mulchie.com; `brew install --cask williamrogersdev/tap/mulchie` |
| `mulchie-<target>.tar.gz` + `.sha256` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `release.yml` matrix | `curl -fsSL https://mulchie.com/install.sh \| sh`; `brew install williamrogersdev/tap/mulchie-cli` |
| `latest.json` + updater signatures | `release.yml` | In-app updates |
| crates `mulchie-core`, `mulchie` | `release.yml`, after the release is published | `cargo install mulchie` |

The release is created as a draft and published by a person. Publishing triggers the Homebrew tap bump and the crates.io publish.

The macOS CLI binaries are signed with the Developer ID certificate, so the standalone tarball runs without a Gatekeeper prompt.

## Public site

`site/` is an Astro static site deployed to Cloudflare Workers static assets on mulchie.com.

- `/`: what Mulchie is, who it is for, install commands.
- `/install/`: every install path, verification, uninstall.
- `/docs/…`: Starlight. Getting started, CLI, MCP, HTTP API, configuration, safety model, troubleshooting.
- `/install.sh`: the installer script, copied from `scripts/install.sh` at build time.

SEO is part of the build: one canonical URL per page, a sitemap, `robots.txt`, Open Graph images generated at build time, `SoftwareApplication` JSON-LD on the landing page, and `llms.txt`. Pages ship no JavaScript except docs search.

Docs describe only behaviour the code has. CLI reference pages are checked against `mulchie --help` in CI.

## Quality gates

CI on every pull request, on macOS and Ubuntu: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and a site build. Tests never touch the real home directory: paths, clock and process table are injected. The safety gate is tested against real temporary git repositories, one test per rule above.
