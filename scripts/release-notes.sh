#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version, without its heading.
#
#   scripts/release-notes.sh 0.3.0            Fails if the section is missing or empty.
#   scripts/release-notes.sh 0.3.0 --release  Also fails if the heading still says "Unreleased".
#
# A release candidate (X.Y.Z-rc.N) uses its own section if CHANGELOG.md has one, and
# otherwise the X.Y.Z section, with a line at the top saying so. That section may still say
# "Unreleased" (the final X.Y.Z has not shipped), so --release does not refuse it then.
#
# HTML comments are dropped, so a placeholder comment does not count as content.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: release-notes.sh VERSION [--release]}"
mode="${2:-}"

section="$version"
intro=""
heading="$(grep -m1 -F "## [$section]" CHANGELOG.md || true)"
if [ -z "$heading" ] && [ "${version#*-}" != "$version" ]; then
  section="${version%%-*}"
  intro="Release candidate $version of $section. These are the notes for $section."
  heading="$(grep -m1 -F "## [$section]" CHANGELOG.md || true)"
fi
if [ -z "$heading" ]; then
  echo "CHANGELOG.md has no '## [$version]' section" >&2
  exit 1
fi
if [ "$mode" = "--release" ] && [ -z "$intro" ] && printf '%s' "$heading" | grep -qi unreleased; then
  echo "CHANGELOG.md still marks $section as Unreleased: $heading" >&2
  echo "Fill the section and replace 'Unreleased' with the release date before tagging." >&2
  exit 1
fi

notes="$(awk -v h="## [$section]" '
  index($0, h) == 1 { found = 1; next }
  found && /^## \[/ { exit }
  found { print }
' CHANGELOG.md | sed -e '/<!--.*-->/d')"

if ! printf '%s' "$notes" | grep -q '[^[:space:]]'; then
  echo "the '## [$section]' section of CHANGELOG.md is empty" >&2
  exit 1
fi
if [ -n "$intro" ]; then
  printf '%s\n\n' "$intro"
fi
printf '%s\n' "$notes"
