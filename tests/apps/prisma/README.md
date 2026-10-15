# Prisma fixture

Ordinary TypeScript + Prisma app (no PGArbor-specific code), configured only via `DATABASE_URL`.
Run: `DATABASE_URL=postgres://user@host:port/emptydb ./run.sh` (target must be an empty database).
Exit 0 on success; non-zero with a message on failure. Last stdout line group ends with
`FIXTURE_RESULT {"name":"prisma","passed":N,"failed":N,"steps":N,"failed_steps":[...]}`.

## Pinned versions
node >= 22 (validated on v26.8.1), pnpm 12.8.2, prisma / @prisma/client 6.19.3 (classic `prisma-client-js`, Rust query engine,
no driver adapter), tsx 4.21.0, typescript 5.9.3, @types/node 24.10.1. Lockfile: `pnpm-lock.yaml` (installed `--frozen-lockfile`).
`pnpm-workspace.yaml` only allows dependency build scripts (prisma engines download).

## Coverage
Schema (`prisma/schema.prisma`): enums (Role, PostStatus), `@default(uuid())` UUID PK, autoincrement Int/BigInt PKs, composite PK,
`@updatedAt`, Json, `String[]`, Decimal(6,2), BigInt, Timestamptz, VarChar, unique + composite index, 1:1 / 1:N / M:N relations, `onDelete: Cascade`.

Migrations (`prisma/migrations`, hand-written Prisma-style SQL, applied by `prisma migrate deploy`):
1. `init` - enums, User/Post/Comment, indexes, FKs
2. `add_tags_and_profiles` - Profile, Tag, PostTag
3. `alter_posts` - `ALTER TABLE "Post" ADD COLUMN "subtitle"`

`run.sh` steps: install, validate, generate, migrate deploy (+ idempotent rerun, `migrate status`), drift check
(`migrate diff --from-url DB --to-schema-datamodel schema.prisma --exit-code`), `db pull --print` introspection + validate,
introspected schema vs original and vs DB (`migrate diff --exit-code`, must be empty), then `src/main.ts` (16 client steps):
create/returning, createManyAndReturn, where/orderBy/take/skip, include joins, relation filters, array and enum filters,
JSON path queries, count/aggregate/groupBy, update/updateMany/upsert/@updatedAt, nested writes, `$transaction` (array + interactive)
commit and rollback, unique (P2002) / FK (P2003) violations, invalid enum at DB level, cascade deletes, raw queries.

Validated against PostgreSQL 18.6 (26 steps pass).
