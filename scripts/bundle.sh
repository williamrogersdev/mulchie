#!/usr/bin/env bash
# Build the Mulchie desktop app bundle (macOS).
#
#   scripts/bundle.sh                       Host architecture, .app only.
#   scripts/bundle.sh --bundles app,dmg     Also build the DMG.
#   scripts/bundle.sh --universal           arm64 + x86_64 in one app (the release path).
#
# The Tauri CLI version is pinned in crates/mulchie-app/package.json.
# Signing and notarization are driven by Tauri's environment variables
# (APPLE_SIGNING_IDENTITY, APPLE_ID, ...). Without them the app is not Developer ID signed.
set -euo pipefail

usage() {
  sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
}

universal=0
bundles="app"
while [ $# -gt 0 ]; do
  case "$1" in
    --universal) universal=1 ;;
    --bundles)
      [ $# -ge 2 ] || { echo "--bundles needs a value" >&2; exit 2; }
      bundles="$2"
      shift
      ;;
    --bundles=*) bundles="${1#--bundles=}" ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "scripts/bundle.sh builds the macOS app and must run on macOS." >&2
  exit 1
fi

cd "$(dirname "$0")/.."
bin_dir="crates/mulchie-app/binaries"
mkdir -p "$bin_dir"

if [ "$universal" -eq 1 ]; then
  targets="aarch64-apple-darwin x86_64-apple-darwin"
  if command -v rustup >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    rustup target add $targets
  fi
  for t in $targets; do
    cargo build --locked --release -p mulchie --target "$t"
    # tauri-build wants a sidecar for each per-arch build it runs ...
    install -m 755 "target/$t/release/mulchie" "$bin_dir/mulchie-$t"
  done
  # ... and the bundler wants one for the universal target itself. Tauri only
  # lipos the app binary, not sidecars, so do it here.
  rm -f "$bin_dir/mulchie-universal-apple-darwin"
  lipo -create \
    "target/aarch64-apple-darwin/release/mulchie" \
    "target/x86_64-apple-darwin/release/mulchie" \
    -output "$bin_dir/mulchie-universal-apple-darwin"
  chmod 755 "$bin_dir/mulchie-universal-apple-darwin"
  lipo -info "$bin_dir/mulchie-universal-apple-darwin"
  tauri_args=(build --target universal-apple-darwin --bundles "$bundles")
  out="target/universal-apple-darwin/release/bundle"
else
  triple="$(rustc -vV | sed -n 's/^host: //p')"
  cargo build --locked --release -p mulchie
  install -m 755 target/release/mulchie "$bin_dir/mulchie-$triple"
  tauri_args=(build --bundles "$bundles")
  out="target/release/bundle"
fi

cd crates/mulchie-app
bun install --frozen-lockfile
bun run tauri "${tauri_args[@]}"
cd ../..

echo "Bundle output: $out"
ls "$out"
