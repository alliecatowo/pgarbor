# sqlx fixture

Ordinary Rust + sqlx application used as a PGArbor compatibility fixture
(RFC 001 section 10, RFC 003 section 6.6). It knows nothing about PGArbor and is
configured only through `DATABASE_URL`.

Run against a **fresh, empty database** (migrations run from scratch):

    DATABASE_URL=postgres://postgres@127.0.0.1:54318/fx_sqlx ./run.sh

`run.sh` builds with `cargo build --release --locked` (target dir
`$ROOT/target/apps-sqlx`, overridable via `CARGO_TARGET_DIR`) and runs the binary.
Exit 0 on success, non-zero on any failure; last stdout line is
`FIXTURE_RESULT {"name":"sqlx","passed":N,"failed":M,"steps":K}`.

## Covers

- `sqlx::migrate!` with 4 migrations in `migrations/`: enum type, serial / identity /
  uuid (`gen_random_uuid()`) PKs, varchar/text/int/bigint/numeric/bool/timestamptz/jsonb/
  enum/array columns, unique + composite unique + partial unique index, FKs with
  ON DELETE CASCADE / SET NULL, CHECK, secondary indexes, and a later ALTER TABLE
  migration (add columns, change type, add constraint/index). Re-running is a no-op.
- CRUD via runtime `sqlx::query` / `query_as` / `query_scalar` (no compile-time checked
  macros): insert RETURNING, joins/filter/order/limit, left join + group by/having,
  update RETURNING, delete with cascade and set-null.
- jsonb (`@>`, `->`, `->>`), array `ANY`, numeric, aggregates (count/sum/max/avg).
- Transactions: commit, explicit rollback, implicit rollback after error.
- Error handling by SQLSTATE and constraint name: 23505, 23503, 23514, 23502, 22P02.
- Introspection (sqlx has no `db pull`): catalog queries (pg_attribute, pg_constraint,
  pg_indexes, pg_enum, format_type, pg_get_constraintdef) are checked against expected
  shapes, then the introspected tables are rebuilt as DDL in a scratch schema and
  re-introspected; the two signatures must be identical.

## Pinned versions

sqlx 0.8.6 (=pinned; features runtime-tokio, postgres, migrate, macros, chrono, uuid,
json, rust_decimal; no TLS), tokio 1, chrono 0.4, uuid 1, rust_decimal 1, serde_json 1;
all transitive versions fixed by the committed `Cargo.lock`. Rust toolchain used for
validation: cargo 1.97.0. Validated against PostgreSQL 18.6.
