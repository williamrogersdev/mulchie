# Releasing Mulchie

One tag, `vX.Y.Z`, builds everything and creates a draft GitHub Release. A person
publishes the draft. Publishing updates the Homebrew tap and crates.io.

What the tag produces:

| Asset | Used by |
| --- | --- |
| `Mulchie_X.Y.Z_universal.dmg` (signed, notarized, stapled) | Download page, Homebrew cask |
| `mulchie-<target>.tar.gz` and `.sha256` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `install.sh`, Homebrew formula |
| `Mulchie_X.Y.Z_universal.app.tar.gz` and `.sig`, `latest.json` | In-app updates |
| `SHA256SUMS` | Manual verification |

The workflows are `.github/workflows/release.yml` (build, draft release) and
`.github/workflows/publish.yml` (tap and crates.io, on publish).

## One-time setup

Do these once, in this order.

### 1. Create the Homebrew tap

Create a public repository `williamrogersdev/homebrew-tap` with a `README.md` so it has a
default branch. The publish workflow adds `Casks/mulchie.rb` and `Formula/mulchie-cli.rb`.
Users then run `brew install williamrogersdev/tap/mulchie-cli`.

### 2. Run the release setup script

```sh
scripts/release-setup.sh
```

It generates the updater key at `~/.tauri/mulchie-updater.key` (it refuses to overwrite one),
writes the public key and the update endpoint into `crates/mulchie-app/tauri.conf.json`, and
prints the `gh secret set` commands. It does not run them. Commit the `tauri.conf.json` change.

Back up the private key and its password. If you lose them, installed apps cannot update.

The updater is wired in the app (`crates/mulchie-app/src/update.rs`). The app registers the Tauri
updater plugin only when `plugins.updater.pubkey` in `tauri.conf.json` is set, and "Check for
Updates…" in its menu then reads `latest.json` from the latest GitHub Release. Until you have run
the script and committed the result, a build says that updates are not configured, and
`release.yml` refuses a tagged release (it checks `bundle.createUpdaterArtifacts` and the public
key) so a release cannot ship an app that cannot update.

### 3. Create the `macos-signing` environment

Repository Settings, Environments, New environment, name `macos-signing`.

- Deployment branches and tags: select "Selected branches and tags" and add the tag
  pattern `v*`. Manual dry runs from a branch will be refused unless you also allow that branch.
- Required reviewers: optional. If you add yourself, each signing job pauses for approval.
  A release has three: the two macOS CLI builds and the desktop build.

All signing secrets live in this environment, so only workflow runs that reference it can read them.

### 4. Add the secrets

Environment secrets (`macos-signing`):

| Secret | What it is | Where to get it |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | Base64 of the Developer ID Application certificate with its private key, as a `.p12` | Keychain Access, My Certificates, right-click "Developer ID Application: ... (UL34VFAKLJ)", Export. Then `base64 -i devid.p12 \| gh secret set APPLE_CERTIFICATE --env macos-signing`. Delete `devid.p12`. |
| `APPLE_CERTIFICATE_PASSWORD` | The password you set on the `.p12` | Chosen at export |
| `KEYCHAIN_PASSWORD` | Any random string. It locks the temporary keychain on the runner. | `openssl rand -base64 32 \| gh secret set KEYCHAIN_PASSWORD --env macos-signing` |
| `APPLE_ID` | Apple ID email of the developer account | Your account |
| `APPLE_PASSWORD` | An app-specific password, not your Apple ID password | account.apple.com, Sign-In and Security, App-Specific Passwords |
| `APPLE_TEAM_ID` | 10-character team ID | `UL34VFAKLJ`, also shown in `security find-identity -v -p codesigning` |
| `TAURI_SIGNING_PRIVATE_KEY` | Contents of `~/.tauri/mulchie-updater.key` | `gh secret set TAURI_SIGNING_PRIVATE_KEY --env macos-signing < ~/.tauri/mulchie-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The key's password | Chosen in `release-setup.sh` |

Repository secrets (for `publish.yml` and `site.yml`):

| Secret | What it is | Where to get it |
| --- | --- | --- |
| `CLOUDFLARE_API_TOKEN` | API token that can deploy Workers (used by `site.yml` to publish `site/dist`) | Cloudflare dashboard, My Profile, API Tokens |
| `CLOUDFLARE_ACCOUNT_ID` | The Cloudflare account that owns the `mulchie-site` Worker | Cloudflare dashboard, Workers & Pages |
| `HOMEBREW_TAP_TOKEN` | Fine-grained personal access token with Contents read and write on `williamrogersdev/homebrew-tap` only | github.com/settings/personal-access-tokens |
| `CARGO_REGISTRY_TOKEN` | crates.io API token with the publish-new and publish-update scopes | crates.io, Account Settings, API Tokens |

`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD`, `APPLE_ID`,
`APPLE_PASSWORD`, `APPLE_TEAM_ID`, `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` use the same names as the `eavesy` repository.
The workflows use an Apple ID and app-specific password for notarization. They do not read an App Store Connect API key.

If `HOMEBREW_TAP_TOKEN` or `CARGO_REGISTRY_TOKEN` is missing, `publish.yml` skips that job with a notice. If a Cloudflare secret is missing, `site.yml` builds and checks the site but skips the deploy with a notice.
If a signing secret is missing, a tagged release fails early and names it.

The first crates.io publish of `mulchie-core` and `mulchie` claims those names. Check they are free.

### 5. Try a dry run

Actions, Release, Run workflow, leave "dry_run" on. It builds all four CLI targets and the universal
app with a throwaway updater key, skips signing, notarization and upload, and keeps the result as
the `release-assets` artifact. Run it before the first real release and after any change to the workflows.

## First release (0.3.0): steps only the maintainer can take

These change things outside the repository. Do them in this order. The sections above have the commands.

1. Run `scripts/release-setup.sh` and add the secrets it lists: the Apple certificate and the Apple ID, app-specific password and team ID used for notarization, the updater key, the tap token, the crates.io token and the Cloudflare token and account ID.
2. Create `williamrogersdev/homebrew-tap`.
3. Merge to `main`, push tag `v0.3.0`, review the draft release, publish it.
4. Confirm the first site deploy and the `mulchie.com` route in Cloudflare. `site/wrangler.jsonc` has no route: attach the custom domain in the Cloudflare dashboard (or add a `routes` entry with `custom_domain: true`) once DNS is ready.

## Each release

1. Bump the version in `Cargo.toml` (`[workspace.package] version`) and set `[workspace.dependencies] mulchie-core`
   to the same version, then `cargo update -w` so `Cargo.lock` follows. Two more files must carry it, and the
   `discovery` test fails if they do not: `server.json` (`version` and the package `version`) and `docs/openapi.yaml`
   (`info.version`). The app's `package.json` is not checked by anything, but keep it equal. The app's bundle version
   comes from `Cargo.toml`; `tauri.conf.json` has no version.
2. In `CHANGELOG.md`, move the unreleased entries under `## [X.Y.Z] - YYYY-MM-DD`. The release is refused while the heading says Unreleased.
3. Open a pull request, let CI pass, merge to `main`.
4. Tag the merge commit and push the tag:
   ```sh
   git switch main && git pull
   git tag -s vX.Y.Z -m "Mulchie X.Y.Z"   # or git tag without -s
   git push origin vX.Y.Z
   ```
   The workflow checks the tag equals the workspace version.
5. Watch the run. It takes about 30 to 60 minutes, most of it notarization.
6. Open the draft release. Check the notes and that these assets exist: four tarballs with `.sha256`, the DMG,
   `Mulchie_X.Y.Z_universal.app.tar.gz` with `.sig`, `latest.json`, `SHA256SUMS`.
7. Publish the draft. This runs `publish.yml`: tap update, then crates.io.
8. Run the checks below.

If a release build fails, fix the cause, delete the tag locally and on GitHub, and tag again. A draft
release from the failed run can be deleted. Never reuse a version that reached crates.io; bump instead.

## Release candidates

Cut `vX.Y.Z-rc.N` first when you want signing, notarization, the updater and the artifacts to fail on a version
nobody installs. Anything with a `-` suffix after `X.Y.Z` is a pre-release. The pipeline builds, signs and
notarizes it exactly like a release, and then differs in four ways:

- The draft GitHub Release is created with the pre-release flag, so `releases/latest` does not move. `install.sh` with no
  version, the in-app updater (`releases/latest/download/latest.json`) and Mulchie's own update check keep seeing the last
  stable release. Keep the flag on when you publish the draft.
- `publish.yml` skips Homebrew and crates.io (a crates.io publish cannot be undone). The crates job prints a notice. It
  also skips on a tag with a `-` even if the pre-release flag was cleared.
- The release notes are the `## [X.Y.Z]` section of `CHANGELOG.md`, with a first line saying they are for a release
  candidate. The section may still say Unreleased. A `## [X.Y.Z-rc.N]` section, if you write one, is used instead.
- Nothing is published to the MCP registry by the workflows (that is a manual step, see below).

Steps:

1. On a branch, set the version to `X.Y.Z-rc.N` in `Cargo.toml` (both places), `server.json` (both places) and
   `docs/openapi.yaml`, then `cargo update -w`. Keep `package.json` equal. Leave `CHANGELOG.md` as it is.
2. Run `cargo test --locked -p mulchie --test discovery` and `scripts/workspace-version.sh v$VERSION`, then run the
   Release workflow by hand from the branch with dry_run on.
3. Open a pull request, merge, and tag the merge commit `git tag -s vX.Y.Z-rc.N` and push it. The workflow checks the tag
   equals the workspace version.
4. Check the draft: it is marked Pre-release, has the same assets as a release, and `latest.json` names the rc version.
   Publish it (this keeps it a pre-release and runs only the skipped-job notices).
5. Install the rc: `curl -fsSL https://mulchie.com/install.sh | MULCHIE_VERSION=X.Y.Z-rc.N sh`, or download the DMG.
   Do not use `releases/latest` to find it. Run the signature, notarization and attestation checks below with `V=X.Y.Z-rc.N`.
   The in-app updater and Homebrew are not exercised by an rc.
6. If the rc is bad, fix on `main` and tag `X.Y.Z-rc.N+1`. Delete a failed tag and draft as in "Each release".

From rc to final:

1. Set the version to `X.Y.Z` in the same files as step 1 and `cargo update -w`.
2. In `CHANGELOG.md` replace `Unreleased` in the `## [X.Y.Z]` heading with the date.
3. Merge, tag `vX.Y.Z` and continue from "Each release", step 4. The final build is a full release: tap, crates.io and
   `latest` all move, and an installed rc sees `X.Y.Z` as an update (a stable install never sees an rc).

The MCP registry entry in `server.json` points at the crates.io package, which an rc does not publish. Only publish
`server.json` to the registry for a final release.

Known limit: macOS shows the bundle version as `X.Y.Z-rc.N` (Tauri writes the version into
`CFBundleShortVersionString` unchanged). Developer ID signing and notarization do not check it; only the App Store would.

## Checks after publishing

On a Mac, in a scratch directory:

```sh
V=X.Y.Z
curl -fsSLO "https://github.com/williamrogersdev/mulchie/releases/download/v$V/SHA256SUMS"
curl -fsSLO "https://github.com/williamrogersdev/mulchie/releases/download/v$V/Mulchie_${V}_universal.dmg"
shasum -a 256 -c SHA256SUMS --ignore-missing

# The DMG is signed, notarized and stapled
codesign --verify --verbose=2 "Mulchie_${V}_universal.dmg"
xcrun stapler validate "Mulchie_${V}_universal.dmg"
spctl -a -vvv -t open --context context:primary-signature "Mulchie_${V}_universal.dmg"

# The app inside is universal and so is its CLI
hdiutil attach "Mulchie_${V}_universal.dmg" -nobrowse -mountpoint /tmp/mulchie-dmg
lipo -archs /tmp/mulchie-dmg/Mulchie.app/Contents/MacOS/mulchie
codesign -dv --verbose=2 /tmp/mulchie-dmg/Mulchie.app 2>&1 | grep -E 'Authority|TeamIdentifier|flags'
hdiutil detach /tmp/mulchie-dmg

# Provenance
gh attestation verify "Mulchie_${V}_universal.dmg" --repo williamrogersdev/mulchie

# Updater manifest (and, from an installed app, "Check for Updates…" in its menu)
curl -fsSL https://github.com/williamrogersdev/mulchie/releases/latest/download/latest.json | jq '.version, (.platforms | keys)'

# Installer, in a clean shell
curl -fsSL https://mulchie.com/install.sh | MULCHIE_INSTALL_DIR="$(mktemp -d)" sh

# Homebrew
brew update
brew install --cask williamrogersdev/tap/mulchie && mulchie --version && brew uninstall --cask mulchie
brew install williamrogersdev/tap/mulchie-cli && mulchie --version && brew uninstall mulchie-cli

# crates.io
cargo install mulchie --version "$V" --root "$(mktemp -d)"
```

On Linux, run the installer and `mulchie --version` on an x86_64 and an aarch64 machine.
To check the standalone macOS binary, download `mulchie-aarch64-apple-darwin.tar.gz` in a browser (so it
is quarantined), unpack it, and run `spctl -a -vvv -t exec mulchie-aarch64-apple-darwin/mulchie`.

## Notes

- Linux tarballs are built on Ubuntu 22.04 runners, so they need glibc 2.35 or newer.
  Linux aarch64 builds natively on GitHub's `ubuntu-22.04-arm` runner. There is no cross compiler.
- The macOS CLI binaries are notarized but cannot be stapled, so Gatekeeper checks Apple's servers on first run.
- `scripts/bundle.sh --universal` builds the CLI for both Apple targets, joins them with `lipo` into
  `crates/mulchie-app/binaries/mulchie-universal-apple-darwin`, then runs `tauri build --target universal-apple-darwin`.
  Tauri joins only the app binary itself, not sidecars, so the script has to.
- The Tauri CLI is pinned in `crates/mulchie-app/package.json`. Bump it there and commit `bun.lock`.
