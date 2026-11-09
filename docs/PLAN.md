# Implementation plan

Target: `docs/ARCHITECTURE.md`. This file is deleted when the last package lands.

Each package is owned by one agent and touches only the paths listed, so packages in the same wave can run at once. Every package ends with its checks passing and its work committed.

## Wave 0: groundwork

### P0. Workspace hygiene
Paths: root `Cargo.toml`, each crate's `Cargo.toml`, `crates/mulchie-app/build.rs`, `crates/mulchie-app/tauri.conf.json` (version only), `.gitignore`, `rust-toolchain.toml`.

- `[workspace.dependencies]` for shared crates; `rust-version`; full crates.io metadata on `mulchie-core` and `mulchie`; `publish = false` on `mulchie-app`; core dependency carries a version.
- Version only in the workspace `Cargo.toml` (Tauri reads it from Cargo).
- `cargo check --workspace` works on a clean checkout: `default-members` excludes the app, and the app's `build.rs` creates a placeholder sidecar when none exists so `cargo check -p mulchie-app` also works.
- Fix the 12 clippy warnings.

Done when: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass from a clean checkout.

## Wave 1: in parallel

### P1. Core: safety gate and destructive paths
Paths: `crates/mulchie-core/**`.

- New `safety.rs` implementing every rule in the architecture's safety-gate section. `reclaim`, `diskmap` and `engine` stop deleting or signalling directly.
- Fixes audit defects 1–11: ignored-file backup, unique backup branches, re-validation at act time with expected `HEAD`, prefix-match busy detection with refreshed working directories, staleness recheck, `need == 0` auto-reclaim, prunable-only pruning, config validation and atomic save, exclusive temp files, `CACHEDIR.TAG` and config flags honoured, pid start-time check.
- git helper with safe flags, timeout and bounded pool.
- Action journal.
- Seams for tests: explicit `Paths` (no env reads in logic), `now` passed in, `ProcessSource` trait.
- Tests: one per safety rule against temporary git repos; the suite runs in under 10 s and never touches the real home.

### P2. Core: engine, API, queue, Linux
Paths: `crates/mulchie-core/**`. Same agent as P1, after P1.

- Supervised loops, poison recovery, sample age and `stale` health, reclaim off the sample loop, engine lock, graceful shutdown hook.
- API: deny by default, limits, `Host` check, token creation and permission checks, short client health timeout, plan/execute endpoints for sweep and clean.
- Queue: reject pid 0 without a TTL, TTL leases, EPERM means alive, priority restored when pressure clears, per-thread renice on Linux.
- Linux: cache and store locations, 15-character process names, scan roots outside `$HOME`, `notify-send`; macOS-only code behind `cfg`.
- Tests for `route`, `read_req`, health, leases, config.

Done when: clippy and tests pass on macOS, and `cargo check --target x86_64-unknown-linux-gnu -p mulchie-core -p mulchie` passes.

### P3. Release pipeline and packaging
Paths: `.github/**`, `scripts/**`, `packaging/**`, `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`, `crates/mulchie-app/package.json`.

- `ci.yml`: fmt, clippy, test on macOS and Ubuntu; site build.
- `release.yml` on `v*` tags: CLI matrix (4 targets, macOS binaries signed), universal signed and notarized DMG with the universal sidecar, updater artifacts and `latest.json`, checksums, draft GitHub Release. Actions pinned by SHA. Modelled on the `eavesy` repo's `macos-release.yml`.
- `publish.yml` on release published: bump the Homebrew tap, `cargo publish` both crates.
- `scripts/install.sh`: detects OS and architecture, downloads from the latest release, verifies the checksum, installs to `~/.local/bin`, tells the user if that is not on `PATH`. POSIX `sh`, passes `shellcheck`.
- `scripts/bundle.sh`: universal build, pinned Tauri CLI.
- `scripts/release-setup.sh`: generates the updater key, writes the public key into `tauri.conf.json`, prints the `gh secret set` commands. Nothing secret is committed.
- `packaging/homebrew/`: cask `mulchie` (app plus `binary` stanza) and formula `mulchie-cli` templates.
- `docs/RELEASING.md`: the one-time setup and the per-release steps.

Done when: `actionlint` and `shellcheck` pass, and `install.sh` installs a locally built tarball from a `file://` base URL in a test.

### P4. Site
Paths: `site/**`, `.github/workflows/site.yml`.

- Astro + Starlight, static, Cloudflare Workers static assets (`wrangler.jsonc`, `_headers`).
- Landing page, install page, docs skeleton from the README.
- SEO: canonical, sitemap, `robots.txt`, build-time OG images, `SoftwareApplication` JSON-LD, `llms.txt`, custom 404.
- `site.yml`: build on pull requests; deploy on `main` when the Cloudflare secret exists.

Done when: `bun run build` passes, every page has a unique title, description and canonical, and Lighthouse on the built landing page scores at least 95 for performance, accessibility and SEO.

## Wave 2: in parallel, after P2

### P5. CLI and MCP
Paths: `crates/mulchie-cli/**`.

- clap with per-command help and `completions`; strict flags; `--version`.
- `doctor`, `service install|uninstall|status` (launchd, systemd user unit).
- `run`: signal forwarding, correct exit status, clear message when no engine is running.
- Honest fallbacks: each command either uses the engine, falls back and says so, or explains how to start the engine.
- `sweep` and `clean` show the plan and execute exactly that plan.
- MCP: version negotiation, tool annotations and titles, correct error codes, TTL leases; `serve` generic over reader and writer, with protocol tests.

### P6. App
Paths: `crates/mulchie-app/**`.

- Split `ui/index.html` into HTML, CSS and ES modules; no inline script; CSP without `'unsafe-inline'` for scripts.
- Use the engine lock: become a client when `serve` already runs.
- Sweep and clean confirm a plan and execute that plan.
- Updater plugin, active only when a public key is configured; "Check for updates" in the tray menu.
- Show config errors and stale-engine state.

Done when: `scripts/bundle.sh` builds the app, it launches, and the dashboard and panel render against a live engine.

## Wave 3: verify

### P7. Review
Independent reviewers on the whole diff: correctness, silent failures, test coverage. Findings go back to the owning agent.

### P8. Docs truth-up
Paths: `README.md`, `site/src/content/docs/**`.

Rewrite the README and docs against the shipped behaviour and real `--help` output. Remove any claim the code does not back.

### P9. End-to-end check
Build the DMG locally with the Developer ID identity, install from it, run `mulchie doctor`, exercise sweep against a throwaway repo, run `install.sh` against local artifacts, load the site build.

## Steps only the maintainer can take

These change things outside the repository. `docs/RELEASING.md` has the commands.

1. Run `scripts/release-setup.sh` and add the secrets it lists (Apple certificate and notarization key, updater key, tap token, crates.io token, Cloudflare token).
2. Create `williamrogersdev/homebrew-tap`.
3. Merge to `main`, push tag `v0.3.0`, review the draft release, publish it.
4. Confirm the first site deploy and the `mulchie.com` route in Cloudflare.
