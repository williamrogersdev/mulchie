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
`scripts/test-install.sh` tests the installer against a fake release, and `scripts/test-bump-version.sh`
tests the version script on a copy of the files it edits.

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

Every change reaches `main` through a pull request, and a pull request merges by itself:

```sh
git push -u origin HEAD
gh pr create --fill
gh pr merge --auto --squash     # merges when the "CI passed" check is green
```

- `main` is protected by a ruleset: no direct pushes, no force pushes, and one required check, `CI passed`. That job
  passes only when every CI job passed (Rust on macOS and Ubuntu, scripts and workflows, the site build). No review
  is required.
- The merge is a squash, so the pull request title becomes the commit subject on `main`. The branch is deleted after the merge.
- A branch does not have to be up to date with `main` to merge. If it conflicts, merge or rebase `main` and push; auto-merge stays on.
- If a check fails, push a fix; auto-merge stays on. To take a pull request out of the queue: `gh pr merge --disable-auto`.
- After the merge, `main.yml` runs CI again on `main`, `site.yml` deploys the site if the merge changed it, and a merge
  that changed the version is released ([docs/RELEASING.md](docs/RELEASING.md)).
- Dependabot pull requests turn on auto-merge themselves, except major version updates.

What to put in one:

- One change per pull request. Say what changed and why.
- Add or update a test. A change to a deletion or signal rule needs a test for that rule. If you add code that deletes, signals or renices outside `safety.rs`, list it in `AGENTS.md` and `docs/ARCHITECTURE.md`.
- Changing a route also means updating `docs/openapi.yaml`; the `discovery` test checks that it matches `ROUTES` in `api.rs`.
- Run fmt, clippy and tests before you push. CI runs them on macOS and Ubuntu.
- Update `CHANGELOG.md` under the unreleased version when users would notice the change.
- Do not add dependencies without a reason in the description.
- Docs describe only what the code does.

## Repository settings

The pipeline depends on three settings outside the repository files. They are applied; these commands apply them again.

```sh
# Auto-merge, squash only, delete merged branches
gh api -X PATCH repos/williamrogersdev/mulchie -F allow_auto_merge=true -F delete_branch_on_merge=true \
  -F allow_squash_merge=true -F allow_merge_commit=false -F allow_rebase_merge=false \
  -f squash_merge_commit_title=PR_TITLE -f squash_merge_commit_message=PR_BODY

# The ruleset on main: pull requests only, "CI passed" required, no force push, no deletion
gh api -X POST repos/williamrogersdev/mulchie/rulesets --input .github/rulesets/main.json
```

`.github/rulesets/main.json` is the ruleset as it was applied. GitHub does not read that file; change the ruleset
with `gh api -X PUT repos/williamrogersdev/mulchie/rulesets/<id> --input .github/rulesets/main.json` and commit the file.

Security problems go through [SECURITY.md](SECURITY.md), not a public issue.
