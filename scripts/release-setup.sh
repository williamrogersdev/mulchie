#!/usr/bin/env bash
# One-time release setup. Run by the maintainer, on their own machine.
#
#   scripts/release-setup.sh
#
# What it does:
#   1. Generates the Tauri updater signing key at ~/.tauri/mulchie-updater.key
#      (refuses to overwrite an existing key).
#   2. Writes the public key and the update endpoint into
#      crates/mulchie-app/tauri.conf.json and turns on updater artifacts.
#   3. Prints the `gh secret set` commands for every secret the workflows use.
#      It does not run them. Nothing secret is written to the repository.
#
# Back up the private key somewhere safe. If it is lost, installed copies of the app
# can never update, because they only trust this key's public half.
set -euo pipefail

cd "$(dirname "$0")/.."

REPO="williamrogersdev/mulchie"
KEY_PATH="${MULCHIE_UPDATER_KEY:-$HOME/.tauri/mulchie-updater.key}"
CONF="crates/mulchie-app/tauri.conf.json"
ENDPOINT="https://github.com/${REPO}/releases/latest/download/latest.json"

command -v jq >/dev/null 2>&1 || { echo "jq is required (brew install jq)." >&2; exit 1; }
command -v bun >/dev/null 2>&1 || { echo "bun is required (brew install bun)." >&2; exit 1; }
[ -f "$CONF" ] || { echo "$CONF not found. Run this from a checkout of the repository." >&2; exit 1; }

if [ -e "$KEY_PATH" ] || [ -e "$KEY_PATH.pub" ]; then
  echo "A key already exists at $KEY_PATH (or $KEY_PATH.pub)." >&2
  echo "Not overwriting it. A new key would orphan every installed copy of the app." >&2
  echo "Move it away yourself if you are sure, then run this script again." >&2
  exit 1
fi

echo "Generating the updater key at $KEY_PATH."
echo "You will be asked for a password. Use a strong one and store it in your password manager."
mkdir -p "$(dirname "$KEY_PATH")"
chmod 700 "$(dirname "$KEY_PATH")"
(cd crates/mulchie-app && bun install --frozen-lockfile && bun run tauri signer generate --write-keys "$KEY_PATH")
chmod 600 "$KEY_PATH"

[ -s "$KEY_PATH.pub" ] || { echo "No public key was written to $KEY_PATH.pub." >&2; exit 1; }
pubkey="$(cat "$KEY_PATH.pub")"

tmp="$(mktemp)"
jq --arg pubkey "$pubkey" --arg endpoint "$ENDPOINT" '
  .bundle.createUpdaterArtifacts = true
  | .plugins.updater = {pubkey: $pubkey, endpoints: [$endpoint]}
' "$CONF" >"$tmp"
mv "$tmp" "$CONF"
echo "Updated $CONF (public key, endpoint, createUpdaterArtifacts)."
echo "Review and commit it: git diff $CONF"

cat <<TEXT

================================================================================
Next: set the secrets. Nothing below has been run.
Run these yourself, one by one, from this checkout. Replace the placeholders.
Full explanations: docs/RELEASING.md

Create the environment first (Settings > Environments > New environment: macos-signing).
Limit its deployment branches and tags to the branch main. Add no required reviewer:
a reviewer makes every release wait for a person.

# Updater signing key (environment: macos-signing)
gh secret set TAURI_SIGNING_PRIVATE_KEY --env macos-signing --repo ${REPO} < "${KEY_PATH}"
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --env macos-signing --repo ${REPO}

# Developer ID certificate (environment: macos-signing)
# Export "Developer ID Application" with its private key from Keychain Access as devid.p12.
base64 -i devid.p12 | gh secret set APPLE_CERTIFICATE --env macos-signing --repo ${REPO}
gh secret set APPLE_CERTIFICATE_PASSWORD --env macos-signing --repo ${REPO}
openssl rand -base64 32 | gh secret set KEYCHAIN_PASSWORD --env macos-signing --repo ${REPO}
rm devid.p12

# Notarization (environment: macos-signing)
gh secret set APPLE_ID --env macos-signing --repo ${REPO}          # Apple ID email
gh secret set APPLE_PASSWORD --env macos-signing --repo ${REPO}    # app-specific password
gh secret set APPLE_TEAM_ID --env macos-signing --repo ${REPO}     # 10 characters, e.g. UL34VFAKLJ

# Tap, crates.io and the site (repository secrets)
gh secret set HOMEBREW_TAP_TOKEN --repo ${REPO}     # fine-grained token, contents:write on williamrogersdev/homebrew-tap
gh secret set CARGO_REGISTRY_TOKEN --repo ${REPO}   # crates.io token with publish-new and publish-update scopes
gh secret set CLOUDFLARE_API_TOKEN --repo ${REPO}   # token that can deploy Workers
gh secret set CLOUDFLARE_ACCOUNT_ID --repo ${REPO}  # the account that owns the mulchie-site Worker
================================================================================
TEXT
