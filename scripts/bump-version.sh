#!/usr/bin/env bash
# Set the Mulchie version in every file that carries it, ready for a release pull request.
#
#   scripts/bump-version.sh 0.4.0         A release.
#   scripts/bump-version.sh 0.4.0-rc.1    A release candidate (a pre-release).
#   scripts/bump-version.sh --check       Change nothing; fail unless every file carries
#                                         the workspace version. CI runs this.
#
# Files: Cargo.toml ([workspace.package] version and the mulchie-core dependency),
# Cargo.lock (the three workspace crates), server.json (two places), docs/openapi.yaml
# (info.version) and crates/mulchie-app/package.json.
#
# For a release (no suffix) it also dates the CHANGELOG.md heading: "## [0.4.0] - Unreleased"
# becomes "## [0.4.0] - YYYY-MM-DD" (UTC). The section must already exist and have notes.
# A release candidate leaves CHANGELOG.md alone and uses the notes of its X.Y.Z section.
#
# Merging the result to main releases it: .github/workflows/main.yml sees the version
# change and starts the release. Nothing here tags, pushes or publishes.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "usage: scripts/bump-version.sh X.Y.Z[-PRERELEASE] | --check" >&2
  exit 2
}
[ $# -eq 1 ] || usage

current="$(scripts/workspace-version.sh)"

# Lines of FILE that contain the literal TEXT.
count() {
  grep -cF -- "$2" "$1" || true
}

# check_count FILE TEXT WANT: fail unless FILE has exactly WANT lines containing TEXT.
check_count() {
  local got
  got="$(count "$1" "$2")"
  if [ "$got" != "$3" ]; then
    echo "$1: expected $3 line(s) containing '$2', found $got" >&2
    return 1
  fi
}

# The versions Cargo.lock records for the workspace crates, one per line.
lock_versions() {
  awk '
    /^name = "(mulchie|mulchie-app|mulchie-core)"$/ { ours = 1; next }
    ours && /^version = / { gsub(/^version = "|"$/, ""); print; ours = 0; next }
    { ours = 0 }
  ' Cargo.lock
}

check() {
  local version="$1" bad=0 locked
  check_count Cargo.toml "mulchie-core = { version = \"$version\"," 1 || bad=1
  locked="$(lock_versions | sort -u | tr '\n' ' ')"
  if [ "$locked" != "$version " ] || [ "$(lock_versions | wc -l | tr -d ' ')" != "3" ]; then
    echo "Cargo.lock: the workspace crates are at '${locked% }', expected three at $version" >&2
    bad=1
  fi
  check_count server.json "\"version\": \"$version\"" 2 || bad=1
  check_count docs/openapi.yaml "  version: $version" 1 || bad=1
  check_count crates/mulchie-app/package.json "\"version\": \"$version\"" 1 || bad=1
  return $bad
}

if [ "$1" = "--check" ]; then
  if check "$current"; then
    echo "Every file carries version $current."
    exit 0
  fi
  echo "Run scripts/bump-version.sh $current to bring every file to the workspace version." >&2
  exit 1
fi

new="$1"
case "$new" in
  -*) usage ;;
esac
new="${new#v}"
if ! printf '%s' "$new" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'; then
  echo "'$new' is not X.Y.Z or X.Y.Z-PRERELEASE" >&2
  exit 1
fi

# Before changing anything: the files must agree on the current version, the tag must
# be new, and a release needs its CHANGELOG.md section.
if ! check "$current"; then
  echo "The files do not agree on the current version $current. Fix that first." >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/v$new" >/dev/null 2>&1; then
  echo "Tag v$new already exists: $new was released. Pick the next version." >&2
  exit 1
fi
prerelease=false
case "$new" in
  *-*) prerelease=true ;;
esac
if [ "$prerelease" = false ] && [ "$(count CHANGELOG.md "## [$new]")" = "0" ]; then
  echo "CHANGELOG.md has no '## [$new]' section. Write the notes under '## [$new] - Unreleased' first." >&2
  exit 1
fi

# replace FILE OLD NEW: replace the literal OLD with NEW on every line that contains it.
replace() {
  local tmp
  tmp="$(mktemp)"
  OLD="$2" NEW="$3" awk '
    BEGIN { old = ENVIRON["OLD"]; new = ENVIRON["NEW"] }
    {
      i = index($0, old)
      if (i) { $0 = substr($0, 1, i - 1) new substr($0, i + length(old)) }
      print
    }
  ' "$1" >"$tmp"
  cat "$tmp" >"$1"
  rm -f "$tmp"
}

if [ "$new" != "$current" ]; then
  # Cargo.toml: the version line inside [workspace.package], then the dependency.
  tmp="$(mktemp)"
  NEW="$new" awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") }
    in_pkg && /^version[[:space:]]*=/ { print "version = \"" ENVIRON["NEW"] "\""; next }
    { print }
  ' Cargo.toml >"$tmp"
  cat "$tmp" >Cargo.toml
  replace Cargo.toml "mulchie-core = { version = \"$current\"," "mulchie-core = { version = \"$new\","

  # Cargo.lock: the version line that follows each workspace crate name.
  NEW="$new" awk '
    /^name = "(mulchie|mulchie-app|mulchie-core)"$/ { ours = 1; print; next }
    ours && /^version = / { print "version = \"" ENVIRON["NEW"] "\""; ours = 0; next }
    { ours = 0; print }
  ' Cargo.lock >"$tmp"
  cat "$tmp" >Cargo.lock
  rm -f "$tmp"

  replace server.json "\"version\": \"$current\"" "\"version\": \"$new\""
  replace docs/openapi.yaml "  version: $current" "  version: $new"
  replace crates/mulchie-app/package.json "\"version\": \"$current\"" "\"version\": \"$new\""
fi

if [ "$prerelease" = false ]; then
  today="${MULCHIE_RELEASE_DATE:-$(date -u +%Y-%m-%d)}"
  replace CHANGELOG.md "## [$new] - Unreleased" "## [$new] - $today"
fi

if [ "$(scripts/workspace-version.sh)" != "$new" ] || ! check "$new"; then
  echo "The version was not set everywhere. Look at git diff." >&2
  exit 1
fi
scripts/release-notes.sh "$new" --release >/dev/null

echo "Version is now $new (was $current)."
echo "Open a pull request with this change and turn on auto-merge. Merging it releases v$new."
