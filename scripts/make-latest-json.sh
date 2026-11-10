#!/usr/bin/env bash
# Write the Tauri updater manifest (latest.json) to stdout.
#
#   scripts/make-latest-json.sh VERSION URL SIG_FILE [NOTES_FILE]
#
# URL is where the .app.tar.gz will be downloaded from. SIG_FILE is the .sig file
# Tauri wrote next to it. The one universal archive serves both Mac architectures.
set -euo pipefail

version="${1:?usage: make-latest-json.sh VERSION URL SIG_FILE [NOTES_FILE]}"
url="${2:?missing URL}"
sig_file="${3:?missing SIG_FILE}"
notes_file="${4:-}"

signature="$(cat "$sig_file")"
notes=""
if [ -n "$notes_file" ]; then
  notes="$(cat "$notes_file")"
fi

jq -n \
  --arg version "$version" \
  --arg url "$url" \
  --arg signature "$signature" \
  --arg notes "$notes" \
  --arg pub_date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  '{
    version: $version,
    notes: $notes,
    pub_date: $pub_date,
    platforms: {
      "darwin-aarch64": {signature: $signature, url: $url},
      "darwin-x86_64": {signature: $signature, url: $url}
    }
  }'
