#!/usr/bin/env bash
# May be run from a checkout or downloaded and piped to bash.
set -euo pipefail

if [ -n "${PGARBOR_REPO:-}" ]; then
  repo="$PGARBOR_REPO"
elif [ -n "${CLAUDE_PROJECT_DIR:-}" ]; then
  repo="$CLAUDE_PROJECT_DIR"
elif repo="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  :
else
  repo="$HOME/pgarbor"
fi

if [ ! -e "$repo" ]; then
  git clone --branch main --single-branch https://github.com/alliecatowo/pgarbor.git "$repo"
fi
if [ ! -f "$repo/scripts/cloud-setup.sh" ] || [ ! -f "$repo/rust-toolchain.toml" ]; then
  printf 'PGArbor setup script missing in %s. Select an up-to-date main checkout or set PGARBOR_REPO to its absolute path.\n' "$repo" >&2
  exit 1
fi
cd "$repo"
printf 'Running PGArbor setup from %s (commit %s)\n' "$PWD" "$(git rev-parse --short HEAD)"
exec bash scripts/cloud-setup.sh
