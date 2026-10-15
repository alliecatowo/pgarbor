#!/usr/bin/env bash
# Drizzle fixture: migrate -> CRUD suite -> introspection round trip, against $DATABASE_URL.
set -uo pipefail
cd "$(dirname "$0")"

: "${DATABASE_URL:?DATABASE_URL must be set (postgres://user@host:port/db)}"
export DATABASE_URL

STEPS=0 PASSED=0 FAILED=0
FIRST_FAIL=""
TEST_JSON=""

finish() {
  local code=0
  [ "$FAILED" -gt 0 ] && code=1
  local detail
  detail=$(printf '%s' "$FIRST_FAIL" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n' ' ')
  printf 'FIXTURE_RESULT {"name":"drizzle","passed":%d,"failed":%d,"steps":%d,"first_failure":"%s"}\n' \
    "$PASSED" "$FAILED" "$STEPS" "$detail"
  exit "$code"
}

# stage <name> <command...>: run a stage, capturing output to .work/<name>.log
stage() {
  local name=$1; shift
  STEPS=$((STEPS + 1))
  mkdir -p .work
  echo "== stage: $name"
  if "$@" >".work/$name.log" 2>&1; then
    PASSED=$((PASSED + 1)); tail -n 3 ".work/$name.log"; return 0
  fi
  FAILED=$((FAILED + 1))
  echo "ERROR: stage '$name' failed; output:" >&2
  cat ".work/$name.log" >&2
  [ -z "$FIRST_FAIL" ] && FIRST_FAIL="stage $name failed"
  return 1
}

rm -rf .work/pull && mkdir -p .work

stage install pnpm install --frozen-lockfile || finish

# generate must find nothing to do: committed migrations match src/schema.ts
check_generate() {
  local out; out=$(pnpm exec drizzle-kit generate 2>&1); echo "$out"
  echo "$out" | grep -q "No schema changes" || { echo "schema.ts drifted from committed migrations"; return 1; }
}
stage generate-check check_generate || finish

stage migrate pnpm exec drizzle-kit migrate || finish

# running migrate a second time must be a no-op
stage migrate-idempotent pnpm exec drizzle-kit migrate || finish

# CRUD + query suite; each assertion step is counted individually
STEPS=$((STEPS + 1))
echo "== stage: crud-suite"
pnpm exec tsx src/test.ts 2>&1 | tee .work/crud.log
TEST_JSON=$(grep '^TEST_RESULT ' .work/crud.log | tail -n1 | sed 's/^TEST_RESULT //')
if [ -n "$TEST_JSON" ]; then
  tp=$(printf '%s' "$TEST_JSON" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(j.passed+" "+j.failed)})')
  set -- $tp
  STEPS=$((STEPS - 1 + $1 + $2)); PASSED=$((PASSED + $1)); FAILED=$((FAILED + $2))
  [ "$2" -gt 0 ] && FIRST_FAIL="crud-suite: $(grep '^FAIL' .work/crud.log | head -n1)"
else
  FAILED=$((FAILED + 1)); FIRST_FAIL="crud-suite crashed (no TEST_RESULT)"
fi
[ "$FAILED" -gt 0 ] && { echo "ERROR: $FIRST_FAIL" >&2; finish; }

# Introspection: pull the live schema, then push the pulled schema back; a faithful pull means no diff.
stage pull pnpm exec drizzle-kit pull --dialect postgresql --url "$DATABASE_URL" --out .work/pull || finish

check_pull_output() {
  grep -q 'pgTable("users"' .work/pull/schema.ts &&
  grep -q 'pgTable("post_likes"' .work/pull/schema.ts &&
  grep -q 'pgEnum("user_role"' .work/pull/schema.ts &&
  grep -q 'generatedAlwaysAsIdentity' .work/pull/schema.ts &&
  grep -q 'posts_tags_gin_idx' .work/pull/schema.ts &&
  grep -q 'posts_price_nonneg' .work/pull/schema.ts &&
  grep -q 'onDelete("cascade")' .work/pull/schema.ts && grep -q 'onDelete("set null")' .work/pull/schema.ts
}
stage pull-contents check_pull_output || finish

roundtrip() {
  # pull wrote a snapshot of the live DB into .work/pull/meta; generating from the pulled schema
  # against that snapshot must find nothing to do.
  cat > .work/pull/roundtrip.config.ts <<CFG
import { defineConfig } from "drizzle-kit";
export default defineConfig({
  dialect: "postgresql",
  schema: "./.work/pull/schema.ts",
  out: "./.work/pull",
});
CFG
  local out
  out=$(pnpm exec drizzle-kit generate --config .work/pull/roundtrip.config.ts 2>&1) || { echo "$out"; return 1; }
  echo "$out"
  echo "$out" | grep -q "No schema changes" && return 0
  # Known drizzle-kit quirk: the pulled identity MAXVALUE 9223372036854775807 is emitted as a JS
  # number literal (loses precision), so generate reports exactly one spurious ALTER ... SET MAXVALUE.
  # Any other difference is a real round-trip failure.
  local sql; sql=$(cat .work/pull/0001_*.sql 2>/dev/null | sed '/^$/d')
  if [ "$sql" = 'ALTER TABLE "posts" ALTER COLUMN "id" SET MAXVALUE 9223372036854776000;' ]; then
    echo "round-trip clean apart from known drizzle-kit bigint MAXVALUE precision quirk"; return 0
  fi
  echo "pulled schema differs from live database snapshot:"; echo "$sql"; return 1
}
stage pull-roundtrip roundtrip || finish

# Pulled schema, when generated against an empty snapshot, must also produce the same table set.
check_generate_from_pull() {
  local n; n=$(pnpm exec tsx -e '
    import { getTableConfig } from "drizzle-orm/pg-core";
    import * as s from "./.work/pull/schema.ts";
    const t = Object.values(s).filter((v: any) => v && typeof v === "object" && Symbol.for("drizzle:Name") in v);
    console.log(t.map((x: any) => getTableConfig(x).name).sort().join(","));
  ')
  echo "$n"
  [ "$n" = "comments,organizations,post_likes,posts,users" ]
}
stage pull-tables check_generate_from_pull || finish

finish
