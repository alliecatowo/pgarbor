//! Type-system / information functions that need no catalog beyond `TypeEnv` (TYPE-12). Lease TYP-43.
//!
//! PG18 reference: `utils/adt/misc.c` (`pg_typeof`, `pg_num_nulls`, `pg_column_size`?), `format_type.c`,
//! `version.c` (`pgsql_version`), `mbutils.c` (encodings), `dbsize.c` (`pg_size_pretty`, `pg_size_bytes`),
//! `regproc.c` (`to_regtype`, `to_regtypemod`), `src/backend/catalog/information_schema.sql`
//! (`_pg_char_max_length` etc.).
//! Owned procs (ImplModule::System):
//! - `pg_typeof(any)` → regtype `Datum::Oid(ctx.arg_types[0].oid)`; `format_type(oid, int4)` →
//!   `names::format_type` (NULL oid → NULL; unknown oid → `???`); `oidvectortypes`; `pg_basetype`.
//! - `pgsql_version` (`version()`): exactly `PostgreSQL 18.6 (PGArbor <CARGO_PKG_VERSION>) on
//!   <std::env::consts::ARCH>-<std::env::consts::OS>, compiled by rustc, 64-bit`. Tools only parse the
//!   leading `PostgreSQL 18.6`; this is a documented, intentional difference from the oracle
//!   (`PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on x86_64-pc-linux-gnu, compiled by gcc ...`).
//!   The `server_version` GUC is owned by the pgarbor crate.
//! - `pg_num_nulls`, `pg_num_nonnulls` (VARIADIC "any"); `pg_column_size(any)` (size of the
//!   `tuple::encode_value` encoding — documented divergence from PG's varlena sizes);
//!   `pg_column_compression` → NULL.
//! - Encodings: `getdatabaseencoding`, `pg_client_encoding` → `UTF8`; `PG_encoding_to_char(6)` → `UTF8`
//!   (encoding ids per PG `pg_enc`: 0 SQL_ASCII, 6 UTF8, 8 LATIN1 ...); `PG_char_to_encoding`;
//!   `pg_encoding_max_length_sql`.
//! - `pg_size_pretty(int8|numeric)` (`10 kB`, `1024 bytes`, rounding half-up per PG18 `pg_size_pretty`),
//!   `pg_size_bytes(text)` (errors 22023 `invalid size: "x"`).
//! - `to_regtype(text)`, `to_regtypemod(text)` (NULL when not found; uses `names::parse_type_string`).
//! - `timeofday()` → text like `Thu Feb 29 13:01:02.500000 2024 UTC` from `ctx.session.clock_timestamp()`.
//! - `parse_ident(text, strict bool)`, `unicode_version` (`16.0`), `icu_unicode_version` (NULL),
//!   `pg_collation_for(any)` (`"default"` for collatable types, NULL otherwise), `pg_settings_get_flags` (NULL).
//! - SQL-language information_schema helpers, matched by OID (pure functions of (typid, typmod)):
//!   `_pg_char_max_length` (13298), `_pg_char_octet_length` (13299), `_pg_numeric_precision` (13301),
//!   `_pg_numeric_precision_radix` (13302), `_pg_numeric_scale` (13303), `_pg_datetime_precision` (13304),
//!   `_pg_interval_type` (13305) — port their SQL bodies from `information_schema.sql` (they follow
//!   domains via `_pg_truetypid` — callers pass the true type already).
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a System-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-43", format!("function {}", proc.name))
}
