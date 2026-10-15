# node-pg fixture

Ordinary TypeScript/Node app using the plain `pg` (node-postgres) driver and `node-pg-migrate`. Nothing PGArbor-specific; configured only through `DATABASE_URL`.

Run: `DATABASE_URL=postgres://user@host:port/emptydb ./tests/apps/node-pg/run.sh` (needs node and pnpm; deps install into this directory only). The database must exist; it is safe to re-run (migrations are rolled down and up again).

## Pinned versions
node-pg-migrate 9.0.0, pg 8.23.1, @types/pg 8.23.1, @types/node 26.6.5, tsx 4.23.15, typescript 7.0.2, pnpm 12 (lockfile `pnpm-lock.yaml` committed). Validated on Node 26.8.1 against PostgreSQL 18.6.

## Steps (run.sh)
install (frozen lockfile), typecheck, `migrate up`, `migrate down 4`, `migrate up`, no-op `migrate up`, then the test suite. Final line: `FIXTURE_RESULT {"name","passed","failed","steps"}`; exit 0 on success, non-zero with a message on failure.

## Schema (4 migrations in `migrations/`)
Extension pgcrypto; enums `user_role`, `order_status`; tables users (serial, varchar unique, bigint, bool, timestamptz), posts (uuid pk, text[], jsonb, GIN index, composite unique, partial index), comments (bigint identity ALWAYS, FK cascade, composite index), products (identity BY DEFAULT, numeric(12,2)), orders, order_items (composite pk, check, FKs). Migration 4 ALTERs: add columns, alter type/default, rename column, add check, partial index, table comment.

## Test coverage (`src/run-tests.ts`)
Schema introspection via information_schema/pg_catalog (columns, types, identity, enum labels, FKs and delete rules, indexes, check defs, comments); insert/returning; joins/filter/order/limit/offset; json/jsonb operators and array operators; update returning; count/sum/avg/group by; transactions commit/rollback/savepoints; SQLSTATE error handling (unique, fk, check, not null, enum, uuid, undefined table, identity override); upsert; named prepared statements and concurrent pool use; multi-statement simple queries; int8 -> bigint, numeric -> string, timestamptz -> Date type parsing; ON DELETE CASCADE; LISTEN/NOTIFY.

There is no schema pull tool for this stack, so introspection round-trip is verified by catalog queries.
