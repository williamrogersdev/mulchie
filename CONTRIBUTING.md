# Contributing to Mulchie

Thanks for helping. This page covers how to build, test and send a change.

## Layout

| Path | What it is |
| --- | --- |
| `crates/mulchie-core` | Engine, scanners, safety gate, sessions, limits, guard rules, update checks, HTTP API. No UI. |
| `crates/mulchie-cli` | The `mulchie` command, `serve`, the MCP server and the agent hooks. |
| `crates/mulchie-app` | Tauri desktop app (tray and dashboard). macOS only for now. |
| `site/` | The public site (Astro). |
| `scripts/` | Installer, bundling and release helpers. |
| `packaging/` | Homebrew templates, systemd unit, launchd plist. |
| `docs/ARCHITECTURE.md` | How the pieces fit and the rules the code keeps. |

Read `docs/ARCHITECTURE.md` before changing anything that deletes files or signals processes.

## Build and test

You need the Rust toolchain (`rust-toolchain.toml` picks it up through rustup).

```sh
cargo build                                  # core and CLI; the app is not a default member
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

On Linux the app needs the WebKitGTK libraries, so skip it there:

```sh
cargo clippy -p mulchie-core -p mulchie --all-targets -- -D warnings
cargo clippy -p mulchie-core -p mulchie --all-targets --no-default-features -- -D warnings
cargo test -p mulchie-core -p mulchie
```

The second clippy line builds without the `network` feature (no HTTPS client, so update checks report that the build has no network support); CI runs it on Ubuntu. To check that the CLI still builds for Linux from a Mac without a C cross-compiler:

```sh
cargo check -p mulchie --target x86_64-unknown-linux-gnu --no-default-features
```

Tests must not touch your real home directory, clock or process table. Inject them. A test that starts an engine must first write a config with build throttling off, no limit budgets and update checks off, so the engine cannot renice your real processes or reach the network (see `safe_config` in `crates/mulchie-cli/tests/cli.rs`).

Scripts are checked with `shellcheck scripts/*.sh`, and workflows with `actionlint`.
`scripts/test-install.sh` tests the installer against a fake release.

## Run the app in development

The app needs [Bun](https://bun.sh) for the pinned Tauri CLI.

```sh
cd crates/mulchie-app
bun install
bun run tauri dev
```

In dev the bundled CLI sidecar is an empty placeholder. To build a real `.app` for your Mac:

```sh
scripts/bundle.sh            # host architecture
scripts/bundle.sh --universal --bundles app,dmg   # what releases use
```

## Pull requests

- One change per pull request. Say what changed and why.
- Add or update a test. A change to a deletion or signal rule needs a test for that rule. If you add code that deletes, signals or renices outside `safety.rs`, list it in `AGENTS.md` and `docs/ARCHITECTURE.md`.
- Changing a route also means updating `docs/openapi.yaml`; the `discovery` test checks that it matches `ROUTES` in `api.rs`.
- Run fmt, clippy and tests before you push. CI runs them on macOS and Ubuntu.
- Update `CHANGELOG.md` under the unreleased version when users would notice the change.
- Do not add dependencies without a reason in the description.
- Docs describe only what the code does.

Security problems go through [SECURITY.md](SECURITY.md), not a public issue.
