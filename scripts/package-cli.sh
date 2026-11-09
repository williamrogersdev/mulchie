#!/usr/bin/env bash
# Package a built CLI binary as mulchie-<target>.tar.gz plus a .sha256 file.
#
#   scripts/package-cli.sh TARGET OUT_DIR [BINARY]
#
# BINARY defaults to target/TARGET/release/mulchie. The archive holds one directory,
# mulchie-TARGET/, with the binary, LICENSE and README.md. The .sha256 file is
# "<hash>  <archive name>", which `shasum -a 256 -c` and `sha256sum -c` both accept.
set -euo pipefail
cd "$(dirname "$0")/.."

target="${1:?usage: package-cli.sh TARGET OUT_DIR [BINARY]}"
out_dir="${2:?missing OUT_DIR}"
binary="${3:-target/$target/release/mulchie}"

[ -x "$binary" ] || { echo "no executable at $binary" >&2; exit 1; }

name="mulchie-$target"
mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
cp "$binary" "$stage/$name/mulchie"
cp LICENSE README.md "$stage/$name/"
chmod 755 "$stage/$name/mulchie"
tar -czf "$out_dir/$name.tar.gz" -C "$stage" "$name"

if command -v sha256sum >/dev/null 2>&1; then
  hash="$(sha256sum "$out_dir/$name.tar.gz" | awk '{print $1}')"
else
  hash="$(shasum -a 256 "$out_dir/$name.tar.gz" | awk '{print $1}')"
fi
echo "$hash  $name.tar.gz" >"$out_dir/$name.tar.gz.sha256"
echo "wrote $out_dir/$name.tar.gz ($hash)"
