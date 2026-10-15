# drizzle fixture

An ordinary Drizzle ORM project (TypeScript, `pg` driver) used as an unmodified-application
compatibility fixture. Configured only via `DATABASE_URL`.

    DATABASE_URL=postgres://postgres@127.0.0.1:54318/fx_drizzle ./tests/apps/drizzle/run.sh

Use a **fresh, empty database** per run (the CRUD suite leaves seed rows behind, so a second run
against the same database fails on unique constraints). Exit 0 on success; non-zero with a message
on failure. The last line is `FIXTURE_RESULT {"name","passed","failed","steps","first_failure"}`.

## Stages
1. `pnpm install --frozen-lockfile` (inside this directory only).
2. `drizzle-kit generate` must report "No schema changes": `src/schema.ts` matches the committed migrations.
3. `drizzle-kit migrate` (twice; the second run is a no-op). Migrations in `drizzle/`:
   - `0000_baseline`: enum, 5 tables (serial, uuid, identity bigint, bigserial, composite PKs), text/varchar/int/bigint/numeric/bool/timestamptz/jsonb/array columns, unique, FKs (CASCADE and SET NULL), btree indexes.
   - `0001_add_login_and_views`: `ALTER TABLE ... ADD COLUMN` (generated).
   - `0002_post_search_and_checks`: hand-written custom migration: CHECK constraints, `ALTER COLUMN TYPE`, GIN index, partial index.
4. `src/test.ts`: 18 counted steps: insert/returning, joins/filter/order/limit/offset, array and jsonb operators,
   update/increment, aggregates + group by/having, relational query API (`db.query.*.findMany with`), transactions
   (commit, rollback, savepoint, `tx.rollback()`), SQLSTATE checks (23505/23503/23514/22P02), upsert, cascade deletes.
5. `drizzle-kit pull` introspection, content checks, then a round trip: `generate` from the pulled schema against the
   pulled snapshot must produce no changes. One known drizzle-kit quirk is tolerated: the pulled identity
   `MAXVALUE 9223372036854775807` loses precision as a JS number and yields a single spurious
   `SET MAXVALUE 9223372036854776000`; any other diff fails.

`.work/` holds logs and pull output (git-ignored).

## Pinned versions
drizzle-orm 0.45.4, drizzle-kit 0.31.11, pg 8.23.1, tsx 4.23.15, typescript 5.9.3, pnpm 12.8.2 (`packageManager`),
Node 26 used for validation. `pnpm-workspace.yaml` allows the esbuild postinstall (pnpm blocks build scripts by default).

Validated against PostgreSQL 18.6: 26 steps pass.
