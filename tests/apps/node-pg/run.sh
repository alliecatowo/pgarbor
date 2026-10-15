#!/usr/bin/env bash
# Fixture: node-pg (pg + node-pg-migrate). Requires DATABASE_URL, node and pnpm.
set -uo pipefail
cd "$(dirname "$0")"

if [ -z "${DATABASE_URL:-}" ]; then
  echo "run.sh: DATABASE_URL is not set" >&2
  echo 'FIXTURE_RESULT {"name":"node-pg","passed":0,"failed":1,"steps":0}'
  exit 2
fi

STEPS=0
fail() {
  echo "run.sh: FAILED at step: $1" >&2
  echo "FIXTURE_RESULT {\"name\":\"node-pg\",\"passed\":$STEPS,\"failed\":1,\"steps\":$((STEPS + 1))}"
  exit 1
}
step() { local name=$1; shift; echo "== $name"; "$@" || fail "$name"; STEPS=$((STEPS + 1)); }

step "install"          pnpm install --frozen-lockfile
step "typecheck"        pnpm run typecheck
step "migrate up"       pnpm run migrate
step "migrate down all" pnpm exec node-pg-migrate down 4 --migrations-dir migrations --migration-file-language ts --tsconfig tsconfig.json
step "migrate up again" pnpm run migrate
# node-pg-migrate is idempotent when nothing is pending
step "migrate no-op"    pnpm run migrate

# Test suite prints its own FIXTURE_RESULT line; fold migration steps into it.
OUT=$(mktemp); trap 'rm -f "$OUT"' EXIT
echo "== tests"
pnpm run test 2>&1 | tee "$OUT" | grep -v '^FIXTURE_RESULT '
RC=${PIPESTATUS[0]}
LINE=$(grep '^FIXTURE_RESULT ' "$OUT" | tail -1)
if [ -z "$LINE" ]; then fail "tests (no result line)"; fi
node -e '
  const r = JSON.parse(process.argv[1].slice(15));
  r.steps += Number(process.argv[2]); r.passed += Number(process.argv[2]);
  console.log("FIXTURE_RESULT " + JSON.stringify(r));' "$LINE" "$STEPS"
exit "$RC"
