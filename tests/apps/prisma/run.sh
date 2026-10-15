#!/usr/bin/env bash
# PGArbor compatibility fixture: Prisma. Requires DATABASE_URL (empty database).
set -uo pipefail
cd "$(dirname "$0")"
: "${DATABASE_URL:?DATABASE_URL must be set}"
export DATABASE_URL
export PRISMA_HIDE_UPDATE_MESSAGE=1 CHECKPOINT_DISABLE=1

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
P=0; F=0; S=0
NAMES=()
FAILS=()

run_step() { # name cmd...
  local name="$1"; shift
  S=$((S+1))
  echo "== step: $name"
  if "$@" >"$WORK/step.log" 2>&1; then
    P=$((P+1)); echo "ok   - $name"
  else
    F=$((F+1)); FAILS+=("$name"); echo "FAIL - $name"; sed 's/^/       /' "$WORK/step.log"
  fi
}

finish() {
  local crud_p=0 crud_f=0 crud_s=0
  if [ -s "$WORK/crud.json" ]; then
    read -r crud_p crud_f crud_s < <(node -e 'const r=require(process.argv[1]);console.log(r.passed,r.failed,r.steps)' "$WORK/crud.json")
  fi
  P=$((P+crud_p)); F=$((F+crud_f)); S=$((S+crud_s))
  local failed_json
  failed_json=$(printf '%s\n' "${FAILS[@]:-}" | node -e 'const l=require("fs").readFileSync(0,"utf8").split("\n").filter(Boolean);console.log(JSON.stringify(l))')
  echo "FIXTURE_RESULT {\"name\":\"prisma\",\"passed\":$P,\"failed\":$F,\"steps\":$S,\"failed_steps\":$failed_json}"
  if [ "$F" -ne 0 ]; then echo "prisma fixture FAILED: $F failing step(s)" >&2; exit 1; fi
  exit 0
}

abort() { echo "prisma fixture ABORTED: $1" >&2; finish; }

run_step "pnpm install (frozen lockfile)" pnpm install --frozen-lockfile
[ "$F" -eq 0 ] || abort "dependency install failed"
run_step "prisma validate" pnpm exec prisma validate
run_step "prisma generate" pnpm exec prisma generate
[ "$F" -eq 0 ] || abort "generate failed"
run_step "prisma migrate deploy (3 migrations from scratch)" pnpm exec prisma migrate deploy
[ "$F" -eq 0 ] || abort "migrations failed"
run_step "prisma migrate deploy is idempotent" pnpm exec prisma migrate deploy
run_step "prisma migrate status up to date" pnpm exec prisma migrate status
run_step "database matches schema.prisma (migrate diff, no drift)" \
  pnpm exec prisma migrate diff --from-url "$DATABASE_URL" --to-schema-datamodel prisma/schema.prisma --exit-code

# Introspection round trip
mkdir -p .introspected
introspect() {
  pnpm exec prisma db pull --print > .introspected/schema.prisma &&
  grep -q '^model User ' .introspected/schema.prisma &&
  grep -q '^enum PostStatus ' .introspected/schema.prisma &&
  pnpm exec prisma validate --schema .introspected/schema.prisma
}
run_step "prisma db pull (introspect) and validate" introspect
run_step "introspected schema equals original schema (migrate diff)" \
  pnpm exec prisma migrate diff --from-schema-datamodel prisma/schema.prisma --to-schema-datamodel .introspected/schema.prisma --exit-code
run_step "introspected schema matches database (migrate diff)" \
  pnpm exec prisma migrate diff --from-url "$DATABASE_URL" --to-schema-datamodel .introspected/schema.prisma --exit-code

echo "== step: client CRUD suite"
FIXTURE_OUT="$WORK/crud.json" pnpm exec tsx src/main.ts || true
[ -s "$WORK/crud.json" ] || { F=$((F+1)); S=$((S+1)); FAILS+=("crud suite crashed"); }
finish
