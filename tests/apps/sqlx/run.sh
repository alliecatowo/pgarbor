#!/usr/bin/env bash
# Installs deps (cargo, inside this dir) and runs the sqlx fixture against $DATABASE_URL.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
: "${DATABASE_URL:?DATABASE_URL must be set}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target/apps-sqlx}"
cd "$HERE"
cargo build --release --locked --quiet
exec "$CARGO_TARGET_DIR/release/sqlx-fixture"
