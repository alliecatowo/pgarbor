# ORM fixture apps vs. pgarbor wire server (orm-fixtures-01)

Measurement only; no engine code changed. Server: `target/debug/pgarbor serve <fresh .pga> --port 55440 --auth trust`,
one fresh `.pga` per app, apps run through their own `run.sh` with
`DATABASE_URL=postgres://postgres@127.0.0.1:55440/postgres?sslmode=disable`. Date: 2026-10-15.

## Results

| app | pass/fail | tests passed / total | first blocking error | likely component |
|---|---|---|---|---|
| sqlx | PASS | 16 / 16 | none | n/a |
| node-pg | FAIL | 2 / 3 stages (install, typecheck ok; `migrate up` failed; test suite never ran) | `CREATE EXTENSION IF NOT EXISTS "pgcrypto";` -> `0A000` "CREATE EXTENSION is not supported by PGArbor yet" (detail `pgarbor-unimplemented: SQL-14`) | sql (unimplemented feature, by design) |
| drizzle | FAIL | 2 / 3 stages (install, generate-check ok; `drizzle-kit migrate` failed; CRUD suite never ran) | `CREATE TABLE "posts" (... "id" bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (sequence name "posts_id_seq" ...))` -> `42601` syntax error at or near "sequence" (migration `0000_baseline.sql`) | sql parser |
| prisma | FAIL | 22 / 26 (16/16 client CRUD steps pass; 4 schema-engine steps fail) | `SELECT ... FROM information_schema.columns info ...` (prisma describe-columns query, run by `migrate diff` and `db pull`) -> `42601` syntax error | catalog (information_schema.columns view) |

Notes:
- sqlx: all migrations, CRUD, joins/aggregates, jsonb/array, transactions, SQLSTATE checks, and introspection pass.
- node-pg and drizzle stop at migrations, so their test suites were not exercised. Their numbers are stage counts from `run.sh`, not test counts.
- prisma's four failing steps (`migrate diff` no-drift, `db pull`, introspected-equals-original, introspected-matches-DB) all trace to one root cause (bug 1). The "no datasource" error in the second `migrate diff` is a knock-on effect of the empty `db pull` output, not a separate bug.

## Distinct root-cause bugs (deduplicated across apps)

### 1. `information_schema.columns` fails with 42601 (affects prisma)
Repro (any client; verified with `pg` against the server):
```sql
SELECT table_name FROM information_schema.columns;        -- 42601 syntax error
SELECT * FROM information_schema.columns LIMIT 1;         -- 42601 syntax error
SELECT table_name FROM information_schema.tables;         -- OK (220 rows)
SELECT table_name FROM information_schema.views;          -- OK
```
`information_schema.columns` is a builtin relkind 'v' view (`crates/pgarbor-catalog/src/builtin/system_rels.rs`). Only that
view fails, which suggests its stored definition does not parse or bind. Component: catalog (builtin view definition), possibly sql binder.
Impact: prisma `migrate diff --from-url` and `db pull` fail. Any tool that introspects columns through this view will hit it too.

### 2. Identity column options `(sequence name ...)` rejected by the parser (affects drizzle)
Repro:
```sql
CREATE TABLE repro_ident (id bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY
  (sequence name "repro_ident_id_seq" INCREMENT BY 1 MINVALUE 1 START WITH 1 CACHE 1));  -- 42601 at "sequence"
CREATE TABLE repro_ident2 (id bigint PRIMARY KEY GENERATED ALWAYS AS IDENTITY (START WITH 1 CACHE 1)); -- OK
```
The plain identity form is accepted, so the gap is in the `SEQUENCE NAME` sequence-option element inside `GENERATED ... AS IDENTITY (...)`.
Drizzle's pinned output (README says validated on PG 18.6) uses this form. Component: sql parser (`pgarbor-sql`).
Impact: blocks drizzle `migrate` at `0000_baseline`, so the rest of the drizzle suite never runs.

### 3. `CREATE EXTENSION` unsupported (SQL-14) (affects node-pg)
Repro: `CREATE EXTENSION IF NOT EXISTS "pgcrypto";` -> `0A000`, detail `pgarbor-unimplemented: SQL-14`.
This is the intended unsupported-behavior path, not a crash, so it is a feature gap rather than a defect. Component: sql (unimplemented inventory).
Impact: blocks node-pg `migrate up` at migration 1, so the rest of the node-pg suite never runs.
Decision for the team: implement it, or document pgcrypto-style extensions as out of scope for fixtures.

## Wire component
No wire-level failures were observed. All four apps connected, authenticated, and ran their statements. Errors arrived with the
expected SQLSTATEs.

## Reproduction artifacts (not committed)
- Fixture logs: `runs/review/orm-logs/{sqlx,node-pg,drizzle,prisma}.out` and `*.server.log`.
- Prisma query capture: `runs/review/orm-logs/prisma-proxy.log` (measurement-only TCP proxy `pgproxy.mjs`).
- Per-fixture `.pga` files: `runs/smoke/orm-<app>.pga`.
