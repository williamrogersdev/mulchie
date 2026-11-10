#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version, without its heading.
#
#   scripts/release-notes.sh 0.3.0            Fails if the section is missing or empty.
#   scripts/release-notes.sh 0.3.0 --release  Also fails if the heading still says "Unreleased".
#
# HTML comments are dropped, so a placeholder comment does not count as content.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: release-notes.sh VERSION [--release]}"
mode="${2:-}"

heading="$(grep -m1 -F "## [$version]" CHANGELOG.md || true)"
if [ -z "$heading" ]; then
  echo "CHANGELOG.md has no '## [$version]' section" >&2
  exit 1
fi
if [ "$mode" = "--release" ] && printf '%s' "$heading" | grep -qi unreleased; then
  echo "CHANGELOG.md still marks $version as Unreleased: $heading" >&2
  echo "Fill the section and replace 'Unreleased' with the release date before tagging." >&2
  exit 1
fi

notes="$(awk -v h="## [$version]" '
  index($0, h) == 1 { found = 1; next }
  found && /^## \[/ { exit }
  found { print }
' CHANGELOG.md | sed -e '/<!--.*-->/d')"

if ! printf '%s' "$notes" | grep -q '[^[:space:]]'; then
  echo "the '## [$version]' section of CHANGELOG.md is empty" >&2
  exit 1
fi
printf '%s\n' "$notes"
