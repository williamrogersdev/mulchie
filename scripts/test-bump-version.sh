#!/usr/bin/env bash
# Tests for bump-version.sh. Copies the files it edits into a temp directory and runs
# the script there, so the checkout is never changed.
# Run: scripts/test-bump-version.sh
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/mulchie-test-bump.XXXXXX")"
trap 'rm -rf "$work"' EXIT INT TERM HUP

failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1"
  failures=$((failures + 1))
}

# A fresh copy of the repository files the script reads, with a known version and changelog.
fixture() {
  rm -rf "$work/repo"
  mkdir -p "$work/repo/scripts" "$work/repo/docs" "$work/repo/crates/mulchie-app"
  cp "$here/scripts/bump-version.sh" "$here/scripts/workspace-version.sh" "$here/scripts/release-notes.sh" "$work/repo/scripts/"
  cp "$here/Cargo.toml" "$here/Cargo.lock" "$here/server.json" "$work/repo/"
  cp "$here/docs/openapi.yaml" "$work/repo/docs/"
  cp "$here/crates/mulchie-app/package.json" "$work/repo/crates/mulchie-app/"
  cat >"$work/repo/CHANGELOG.md" <<'TEXT'
# Changelog

## [9.9.0] - Unreleased

- A new thing.

## [9.8.0] - 2026-01-01

- An old thing.
TEXT
  git -C "$work/repo" init -q
  # Start every test from 9.8.0, whatever the checkout is at.
  (cd "$work/repo" && scripts/bump-version.sh 9.8.0 >/dev/null)
}

run() {
  (cd "$work/repo" && MULCHIE_RELEASE_DATE=2026-02-03 scripts/bump-version.sh "$@")
}

fixture
if run --check >/dev/null; then pass "--check passes on a consistent tree"; else fail "--check passes on a consistent tree"; fi

fixture
if run 9.9.0-rc.1 >/dev/null \
  && [ "$(cd "$work/repo" && scripts/workspace-version.sh)" = "9.9.0-rc.1" ] \
  && run --check >/dev/null \
  && grep -qF '## [9.9.0] - Unreleased' "$work/repo/CHANGELOG.md"; then
  pass "a release candidate sets every file and leaves CHANGELOG.md alone"
else
  fail "a release candidate sets every file and leaves CHANGELOG.md alone"
fi

if run 9.9.0 >/dev/null \
  && run --check >/dev/null \
  && grep -qF '## [9.9.0] - 2026-02-03' "$work/repo/CHANGELOG.md" \
  && [ "$(grep -c '"version": "9.9.0"' "$work/repo/server.json")" = "2" ] \
  && grep -qF 'mulchie-core = { version = "9.9.0",' "$work/repo/Cargo.toml" \
  && grep -qF '  version: 9.9.0' "$work/repo/docs/openapi.yaml"; then
  pass "a release sets every file and dates the CHANGELOG.md heading"
else
  fail "a release sets every file and dates the CHANGELOG.md heading"
fi

# Only the three workspace crates may change in Cargo.lock.
changed="$(diff "$here/Cargo.lock" "$work/repo/Cargo.lock" | grep -c '^>' || true)"
if [ "$changed" = "3" ]; then pass "Cargo.lock changes only the three workspace crates"; else fail "Cargo.lock changes only the three workspace crates ($changed lines)"; fi

fixture
if run 9.7.0 >/dev/null 2>&1; then fail "a release with no CHANGELOG.md section is refused"; else pass "a release with no CHANGELOG.md section is refused"; fi
if [ "$(cd "$work/repo" && scripts/workspace-version.sh)" = "9.8.0" ]; then pass "a refused bump changes nothing"; else fail "a refused bump changes nothing"; fi

fixture
if run 1.2 >/dev/null 2>&1; then fail "a malformed version is refused"; else pass "a malformed version is refused"; fi

fixture
git -C "$work/repo" -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m init
git -C "$work/repo" tag v9.9.0
if run 9.9.0 >/dev/null 2>&1; then fail "a version whose tag exists is refused"; else pass "a version whose tag exists is refused"; fi

fixture
tmp="$(mktemp)"
sed 's/"version": "9.8.0"/"version": "0.0.1"/' "$work/repo/crates/mulchie-app/package.json" >"$tmp"
cat "$tmp" >"$work/repo/crates/mulchie-app/package.json"
rm -f "$tmp"
if run --check >/dev/null 2>&1; then fail "--check fails when a file is behind"; else pass "--check fails when a file is behind"; fi

if [ "$failures" -gt 0 ]; then
  echo "$failures test(s) failed"
  exit 1
fi
echo "all tests passed"
