# Releasing Mulchie

Merging a pull request that changes the version releases that version. Nobody pushes a tag
or publishes a draft: the pipeline builds everything, creates the tag `vX.Y.Z`, publishes the
GitHub Release, then updates the Homebrew tap and crates.io.

```sh
scripts/bump-version.sh X.Y.Z          # or X.Y.Z-rc.N for a release candidate
git switch -c release-X.Y.Z && git commit -am "Version X.Y.Z" && git push -u origin HEAD
gh pr create --fill && gh pr merge --auto --squash
```

What happens after the merge (`.github/workflows/main.yml`):

1. CI runs on the merge commit.
2. If the push changed the workspace version and the tag `vX.Y.Z` does not exist, `release.yml` runs.
   Any other push to `main` releases nothing.
3. `release.yml` builds and signs the assets, then creates the tag and the published GitHub Release in
   one step, as the last thing it does.
4. `publish.yml` updates the Homebrew tap and crates.io (not for a release candidate).

What a release produces:

| Asset | Used by |
| --- | --- |
| `Mulchie_X.Y.Z_universal.dmg` (signed, notarized, stapled) | Download page, Homebrew cask |
| `mulchie-<target>.tar.gz` and `.sha256` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | `install.sh`, Homebrew formula |
| `Mulchie_X.Y.Z_universal.app.tar.gz` and `.sig`, `latest.json` | In-app updates |
| `SHA256SUMS` | Manual verification |

The workflows are `.github/workflows/main.yml` (CI on `main`, then the release decision),
`.github/workflows/release.yml` (build, tag, GitHub Release) and `.github/workflows/publish.yml`
(tap and crates.io, called by `release.yml`). How pull requests merge is in
[CONTRIBUTING.md](../CONTRIBUTING.md).

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
`release.yml` makes a CLI-only release (see "CLI-only release" below). It checks
`bundle.createUpdaterArtifacts` and the public key, so a release never ships an app that cannot update.

### 3. Create the `macos-signing` environment

Repository Settings, Environments, New environment, name `macos-signing`.

- Deployment branches and tags: select "Selected branches and tags" and add the branch
  `main`. Releases are built from `main`. Manual dry runs from another branch will be refused
  unless you also allow that branch.
- Required reviewers: leave empty. A reviewer makes each signing job wait for a person to
  approve it (a release has three: the two macOS CLI builds and the desktop build), and then
  a release is no longer automatic.

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
If a signing secret is missing, a full release (one with the updater configured) fails early and names it.
A CLI-only release uses none of them.

The first crates.io publish of `mulchie-core` and `mulchie` claims those names. Check they are free.

### 5. Protect `main` and turn on auto-merge

Already done for this repository; the commands are in [CONTRIBUTING.md](../CONTRIBUTING.md) under
"Repository settings" in case they have to be applied again.

### 6. Try a dry run

`gh workflow run release.yml --ref <branch>` (or Actions, Release, Run workflow, with "dry_run" left on). It builds all four CLI targets and the universal
app with a throwaway updater key, skips signing, notarization and upload, and keeps the result as
the `release-assets` artifact. Run it before the first real release and after any change to the workflows.

## CLI-only release (before signing is set up)

A version can be released as the CLI alone before any signing secret exists. The switch is
the committed updater config: if `crates/mulchie-app/tauri.conf.json` does not have both
`bundle.createUpdaterArtifacts: true` and a non-empty `plugins.updater.pubkey`, the `prepare` job sets
`app=false` and prints a notice. You do not pass a flag. Dry runs ignore this and always build the app.

What a CLI-only release builds:

- the four CLI tarballs with their `.sha256` files (macOS arm64 and x86_64, Linux x86_64 and aarch64);
- `SHA256SUMS` over the tarballs;
- build provenance attestations for the tarballs;
- a GitHub Release whose notes start with a line saying it is CLI only. Assembly fails if any of the eight
  CLI files is missing.

What it skips: the desktop app job, Developer ID signing, notarization, the DMG, the updater archive and
`latest.json`. The CLI jobs do not use the `macos-signing` environment and read no Apple secret. The macOS
binaries carry an ad-hoc signature, which the job applies and verifies with `codesign --verify` (arm64 macOS
will not run an unsigned binary). They are not Developer ID signed or notarized: `install.sh` and `curl` do
not quarantine a file, so installing that way works, but a binary downloaded in a browser is quarantined and
Gatekeeper blocks it.

After the release is published, `publish.yml` looks at its assets. With no `.dmg` it updates only
`Formula/mulchie-cli.rb` in the tap (not the cask) and says so in a notice. crates.io is unchanged.
A pre-release is still skipped. `releases/latest` points at this release, so `install.sh` works at once;
the in-app updater has nothing to read because there is no `latest.json` and no app yet.

To move to a full release:

1. Run `scripts/release-setup.sh` and commit the `tauri.conf.json` change.
2. Create the `macos-signing` environment and set its secrets (steps 3 and 4 above).
3. Release the next version. The first app release cannot update anyone from a CLI-only install: there is no app
   to update. People install the DMG or the cask once, and later versions update from there.

With the updater configured, a missing Apple secret fails the release. The app is never shipped unsigned.

## First release (0.3.0): steps only the maintainer can take

These change things outside the repository. Do them in this order. The sections above have the commands.

1. Add the repository secrets: the tap token, the crates.io token and the Cloudflare token and account ID. Until they exist, the tap update, the crates.io publish and the site deploy are skipped with a warning in the run. For the desktop app, also run `scripts/release-setup.sh` and add the secrets it lists: the Apple certificate, the Apple ID, app-specific password and team ID used for notarization, and the updater key.
2. Create `williamrogersdev/homebrew-tap` (done).
3. Merge a pull request made with `scripts/bump-version.sh 0.3.0`.
4. Confirm the first site deploy and the `mulchie.com` route in Cloudflare. `site/wrangler.jsonc` has no route: attach the custom domain in the Cloudflare dashboard (or add a `routes` entry with `custom_domain: true`) once DNS is ready.

## Each release

1. On a branch, make sure `CHANGELOG.md` has the notes under `## [X.Y.Z] - Unreleased`, then run
   `scripts/bump-version.sh X.Y.Z`. It sets the version in `Cargo.toml` (`[workspace.package] version` and the
   `mulchie-core` dependency), `Cargo.lock`, `server.json` (two places), `docs/openapi.yaml` (`info.version`) and the
   app's `package.json`, and replaces `Unreleased` in the changelog heading with today's date. The app's bundle version
   comes from `Cargo.toml`; `tauri.conf.json` has no version.
2. Open a pull request and turn on auto-merge (`gh pr merge --auto --squash`). CI checks that every file carries the
   same version and that the changelog section is dated and not empty, so a pull request that could not be released
   does not merge.
3. That is all. When the pull request merges, the release runs: about 10 minutes for a CLI-only release, 30 to 60
   with the app, most of it notarization. Follow it with `gh run watch "$(gh run list --workflow main.yml --limit 1 --json databaseId --jq '.[0].databaseId')"`.
4. Run the checks below.

If the release fails, nothing was published and no tag exists: the tag is created with the release, at the end.

- A failure that will pass on a second try (a runner or Apple's notary service): `gh run rerun <run id> --failed`.
- A failure that needs a fix: merge the fix, then `gh workflow run release.yml -f dry_run=false`. That releases the
  version on `main`. The automatic release starts only on the push that changes the version, so a fix alone does not start it.
- If the GitHub Release exists but the tap or crates.io step failed: `gh workflow run publish.yml -f tag=vX.Y.Z`.
  It skips what is already published.

Never reuse a version that reached crates.io or has a tag; bump instead. `bump-version.sh` and `release.yml` both
refuse a version whose tag exists.

## Release candidates

Cut `vX.Y.Z-rc.N` first when you want signing, notarization, the updater and the artifacts to fail on a version
nobody installs. Anything with a `-` suffix after `X.Y.Z` is a pre-release. The pipeline builds, signs and
notarizes it exactly like a release, and then differs in four ways:

- The GitHub Release is published with the pre-release flag, so `releases/latest` does not move. `install.sh` with no
  version, the in-app updater (`releases/latest/download/latest.json`) and Mulchie's own update check keep seeing the last
  stable release.
- `publish.yml` skips Homebrew and crates.io (a crates.io publish cannot be undone). The crates job prints a notice.
- The release notes are the `## [X.Y.Z]` section of `CHANGELOG.md`, with a first line saying they are for a release
  candidate. The section may still say Unreleased. A `## [X.Y.Z-rc.N]` section, if you write one, is used instead.
- Nothing is published to the MCP registry by the workflows, for any release. Publishing `server.json` there is
  the one step still done by hand.

Steps:

1. On a branch, run `scripts/bump-version.sh X.Y.Z-rc.N`. It leaves `CHANGELOG.md` as it is.
2. Optional: run the Release workflow from the branch as a dry run (`gh workflow run release.yml --ref <branch>`).
3. Open a pull request and turn on auto-merge. The merge releases the rc.
4. Check the release: it is marked Pre-release, has the same assets as a release, and `latest.json` (when the app
   ships) names the rc version.
5. Install the rc: `curl -fsSL https://mulchie.com/install.sh | MULCHIE_VERSION=X.Y.Z-rc.N sh`, or download the DMG.
   Do not use `releases/latest` to find it. Run the signature, notarization and attestation checks below with `V=X.Y.Z-rc.N`.
   The in-app updater and Homebrew are not exercised by an rc.
6. If the rc is bad, fix on `main` and release `X.Y.Z-rc.N+1`.

From rc to final: run `scripts/bump-version.sh X.Y.Z` (it dates the changelog heading) and merge it as in "Each release".
The final build is a full release: tap, crates.io and `latest` all move, and an installed rc sees `X.Y.Z` as an update
(a stable install never sees an rc).

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
