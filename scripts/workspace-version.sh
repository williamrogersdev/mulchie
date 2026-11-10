#!/usr/bin/env bash
# Print the workspace version from the root Cargo.toml ([workspace.package] version).
# With an argument (a tag such as v0.3.0 or v0.3.0-rc.1), exit non-zero unless it matches.
# The version must be X.Y.Z or X.Y.Z-PRERELEASE (a release candidate is X.Y.Z-rc.N);
# build metadata (+...) is refused because it cannot go in a tag or a download URL.
set -euo pipefail
cd "$(dirname "$0")/.."

version="$(awk '
  /^\[/ { in_pkg = ($0 == "[workspace.package]"); next }
  in_pkg && /^version[[:space:]]*=/ {
    gsub(/^[^"]*"|"[^"]*$/, "")
    print
    exit
  }
' Cargo.toml)"

if [ -z "$version" ]; then
  echo "could not read [workspace.package] version from Cargo.toml" >&2
  exit 1
fi

if ! printf '%s' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'; then
  echo "workspace version '$version' is not X.Y.Z or X.Y.Z-PRERELEASE" >&2
  exit 1
fi

if [ $# -ge 1 ]; then
  tag="$1"
  if [ "$tag" != "v$version" ]; then
    echo "tag $tag does not match workspace version $version (expected v$version)" >&2
    exit 1
  fi
fi
echo "$version"
