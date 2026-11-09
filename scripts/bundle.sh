#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
triple="$(rustc -vV | sed -n 's/^host: //p')"
cargo build --release -p mulchie
mkdir -p crates/mulchie-app/binaries
cp target/release/mulchie "crates/mulchie-app/binaries/mulchie-$triple"
cd crates/mulchie-app
bunx @tauri-apps/cli@2 build --bundles "${1:-app}"
