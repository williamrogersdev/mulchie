#!/usr/bin/env bash
# Render the Homebrew cask and formula templates for a release.
#
#   scripts/render-homebrew.sh VERSION ASSETS_DIR OUT_DIR
#
# ASSETS_DIR holds the release assets: the four mulchie-<target>.tar.gz.sha256 files and
# Mulchie_VERSION_universal.dmg (or a Mulchie_VERSION_universal.dmg.sha256 file).
# Writes OUT_DIR/Casks/mulchie.rb and OUT_DIR/Formula/mulchie-cli.rb.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: render-homebrew.sh VERSION ASSETS_DIR OUT_DIR}"
assets="${2:?missing ASSETS_DIR}"
out="${3:?missing OUT_DIR}"

sha_of_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# First field of a "<hash>  <name>" file, checked to look like a SHA-256.
sha_from_checksum_file() {
  local value
  value="$(awk 'NR == 1 {print $1}' "$1")"
  if ! printf '%s' "$value" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "$1 does not hold a SHA-256 hash" >&2
    exit 1
  fi
  printf '%s' "$value"
}

dmg="$assets/Mulchie_${version}_universal.dmg"
if [ -f "$dmg" ]; then
  sha_dmg="$(sha_of_file "$dmg")"
elif [ -f "$dmg.sha256" ]; then
  sha_dmg="$(sha_from_checksum_file "$dmg.sha256")"
else
  echo "no DMG or DMG checksum for $version in $assets" >&2
  exit 1
fi

render() {
  sed \
    -e "s/@VERSION@/$version/g" \
    -e "s/@SHA256_DMG@/$sha_dmg/g" \
    -e "s/@SHA256_AARCH64_APPLE_DARWIN@/$(sha_from_checksum_file "$assets/mulchie-aarch64-apple-darwin.tar.gz.sha256")/g" \
    -e "s/@SHA256_X86_64_APPLE_DARWIN@/$(sha_from_checksum_file "$assets/mulchie-x86_64-apple-darwin.tar.gz.sha256")/g" \
    -e "s/@SHA256_AARCH64_UNKNOWN_LINUX_GNU@/$(sha_from_checksum_file "$assets/mulchie-aarch64-unknown-linux-gnu.tar.gz.sha256")/g" \
    -e "s/@SHA256_X86_64_UNKNOWN_LINUX_GNU@/$(sha_from_checksum_file "$assets/mulchie-x86_64-unknown-linux-gnu.tar.gz.sha256")/g" \
    "$1"
}

mkdir -p "$out/Casks" "$out/Formula"
render packaging/homebrew/mulchie.rb.tmpl >"$out/Casks/mulchie.rb"
render packaging/homebrew/mulchie-cli.rb.tmpl >"$out/Formula/mulchie-cli.rb"

if grep -n '@[A-Z0-9_]*@' "$out/Casks/mulchie.rb" "$out/Formula/mulchie-cli.rb"; then
  echo "unfilled placeholders remain" >&2
  exit 1
fi
echo "rendered $out/Casks/mulchie.rb and $out/Formula/mulchie-cli.rb"
