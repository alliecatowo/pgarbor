//! Definition text of catalog objects as PostgreSQL prints it: `pg_get_indexdef`,
//! `pg_get_constraintdef`, `pg_get_viewdef`, function signatures/results/definitions, sequence and
//! default helpers. Lease CAT-26. PG sources: `utils/adt/ruleutils.c` (`pg_get_indexdef_worker`,
//! `pg_get_constraintdef_worker`, `pg_get_functiondef`, `print_function_arguments`,
//! `print_function_rettype`, `pg_get_function_arguments`, `pg_get_function_identity_arguments`,
//! `pg_get_function_result`, `pg_get_serial_sequence` is `utils/adt/misc.c`).
//!
//! These are pure functions of the model (expression TEXT is stored; only structure is formatted
//! here), used by the `sysfuncs::info` SQL functions, `pg_indexes`, `pg_views`,
//! information_schema and `\d` output. All object/column names go through
//! `pgarbor_types::text::format::quote_identifier` (quotes when not `[a-z_][a-z0-9_$]*` or a
//! reserved keyword per `pg_keywords`). Table references in definitions are schema-qualified exactly
//! as PG's `generate_relation_name`/`generate_qualified_relation_name` decide (documented per
//! function).
//!
//! ## `indexdef` (oracle-verified)
//! `CREATE [UNIQUE ]INDEX <name> ON <schema>.<table> USING <am> (<keys>)[ INCLUDE (<cols>)]
//! [ NULLS NOT DISTINCT][ WITH (<opts>)][ WHERE <predicate>]` - the table is ALWAYS schema-qualified
//! (`ON zz_cat.t`), the index name never. Examples:
//! - `CREATE UNIQUE INDEX t_pk2 ON zz_cat.t USING btree (a) WITH (fillfactor='50')` (option VALUES are
//!   single-quoted in indexdef; reloptions array shows `fillfactor=50`)
//! - `CREATE INDEX i1 ON zz_cat.t USING btree (((a + 1)))` (expression keys that are not plain function
//!   calls get an extra pair of parentheses), `... (lower(b))`, `... (a DESC NULLS LAST, b)`,
//!   `... (a) INCLUDE (a)`.
//! Key rendering: plain column -> quoted CURRENT column name (looked up by attnum, so a renamed
//! column prints its new name even though `IndexKey::name` keeps the old one); expression -> the
//! stored text; then ` <opclass>` only when the opclass is not the default for the type, then
//! ` COLLATE "<name>"` only when the collation differs from the column/type default (`COLLATE "C"`),
//! then ` DESC`, then ` NULLS FIRST|LAST` only when different from the default (ASC -> NULLS LAST,
//! DESC -> NULLS FIRST). `pretty` changes nothing here but spacing after commas is `, `.
//! With `column = Some(n)` (1-based) only that key's text is returned (`pg_get_indexdef(oid, n, pretty)`),
//! `column = Some(0)` is the full definition.
//!
//! ## `constraintdef` (oracle-verified for 18.6)
//! - PK: `PRIMARY KEY (id)`; UNIQUE: `UNIQUE (b)` / `UNIQUE NULLS NOT DISTINCT (b)`; with INCLUDE:
//!   `PRIMARY KEY (id) INCLUDE (x)`; backing-index options: ` WITH (fillfactor='70')`.
//! - CHECK: `CHECK ((c > 0))` = `CHECK (` + stored text + `)`; stored text already has its own
//!   parentheses as `pg_get_expr` prints them.
//! - NOT NULL: `NOT NULL a`; domain: `NOT NULL`, `CHECK ((VALUE > 0))`.
//! - FK: `FOREIGN KEY (f) REFERENCES t(id)` + ` MATCH FULL` (simple prints nothing) + ` ON UPDATE
//!   <action>` only if not NO ACTION + ` ON DELETE <action>` (with `SET NULL (a, b)` /
//!   `SET DEFAULT (a)` column lists when `delete_set_columns` is non-empty); the referenced table
//!   name is schema-qualified only when not visible in the search path (`env`).
//! - Trailing modifiers in this order: ` DEFERRABLE`, ` INITIALLY DEFERRED` (only when
//!   deferrable), ` NOT VALID` (when `!validated`), ` NOT ENFORCED` (when `!enforced`).
//! - EXCLUDE is unsupported (0A000 is never reached: such constraints cannot exist).
//!
//! ## Functions
//! - `function_arguments`: `pg_get_function_arguments`: every parameter with mode words:
//!   `a integer, b text DEFAULT 'x'::text, OUT o1 integer, VARIADIC v integer[]`; IN mode is implicit
//!   (no word), `INOUT`, `OUT`, `VARIADIC`; TABLE columns are NOT listed (they appear in the result);
//!   unnamed parameters print only the type; defaults print ` DEFAULT <expr>` (stored text).
//! - `function_identity_arguments`: same but without defaults and without OUT/TABLE params
//!   (`integer, integer[]`; mode words kept for INOUT/VARIADIC).
//! - `function_result`: `integer`, `SETOF integer`, `TABLE(a integer, b text)` (for TABLE params),
//!   `record`; procedures -> None (NULL).
//! - `functiondef`: `pg_get_functiondef` full statement:
//!   `CREATE OR REPLACE FUNCTION zz_cat.f(a integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$select a$function$\n`
//!   with clauses in PG order (RETURNS, LANGUAGE, WINDOW (no), IMMUTABLE|STABLE (VOLATILE omitted),
//!   STRICT, SECURITY DEFINER, LEAKPROOF, PARALLEL SAFE|RESTRICTED (UNSAFE omitted), COST n (when
//!   not default 100/1), ROWS n (SRFs when not 1000), `SET name TO 'value'` lines), body dollar-quoted
//!   with the tag `$function$` (if the body itself contains that tag PG tries `$function$` then appends
//!   a digit suffix tag: `$function1$`...). Procedures (oracle): `CREATE OR REPLACE PROCEDURE
//!   zz_cat.pr(IN x integer)\n LANGUAGE sql\nAS $procedure$select 1$procedure$\n` - no RETURNS, the IN
//!   mode word IS printed for procedure parameters, body tag `$procedure$`. The name is always
//!   schema-qualified. Output ends with a newline; plpgsql/stable example: `CREATE OR REPLACE
//!   FUNCTION zz_cat.g(a integer)\n RETURNS TABLE(x integer, y text)\n LANGUAGE plpgsql\n STABLE\nAS
//!   $function$begin ... end$function$\n`.
//!
//! ## Misc
//! - `serial_sequence(table, column)`: `pg_get_serial_sequence`: the sequence OWNED BY (table, column)
//!   (serial or identity) as `schema.seq` text quoted per `quote_qualified_identifier`; NULL if none.
//! - `viewdef`: `ViewDef::query.sql` verbatim for user views/matviews; system views: the stored
//!   `builtin::system_view_def`. Non-views -> None (`pg_get_viewdef` returns NULL? PG raises
//!   `"t" is not a view` 42809 for tables: return `Err(errors::not_a(name, "view"))`).
use crate::env::CatalogEnv;
use crate::model::*;
use pgarbor_common::{Oid, PgError, Result};

/// `pg_get_indexdef` (module docs). `Ok(None)` for an unknown OID.
// CONTRACT
#[allow(unused_variables)]
pub fn indexdef(env: &CatalogEnv, index: Oid, column: Option<usize>, pretty: bool) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_indexdef"))
}

/// `pg_get_constraintdef`. `Ok(None)` for an unknown OID.
// CONTRACT
#[allow(unused_variables)]
pub fn constraintdef(env: &CatalogEnv, constraint: Oid, pretty: bool) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_constraintdef"))
}

/// `pg_get_viewdef(oid)`.
// CONTRACT
#[allow(unused_variables)]
pub fn viewdef(env: &CatalogEnv, rel: Oid, pretty: bool) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_viewdef"))
}

/// `pg_get_function_arguments` for a user or builtin function OID.
// CONTRACT
#[allow(unused_variables)]
pub fn function_arguments(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_function_arguments"))
}

/// `pg_get_function_identity_arguments`.
// CONTRACT
#[allow(unused_variables)]
pub fn function_identity_arguments(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_function_identity_arguments"))
}

/// `pg_get_function_result` (None for procedures and unknown OIDs).
// CONTRACT
#[allow(unused_variables)]
pub fn function_result(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_function_result"))
}

/// `pg_get_functiondef` (user functions; builtin -> 42809 `"x" is an aggregate function`-style
/// errors are `internal`: builtin functions have no source: `Err(errors::cache_lookup_failed)`).
// CONTRACT
#[allow(unused_variables)]
pub fn functiondef(env: &CatalogEnv, func: Oid) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_functiondef"))
}

/// `pg_get_serial_sequence`: the sequence owned by `table.column`.
// CONTRACT
#[allow(unused_variables)]
pub fn serial_sequence(env: &CatalogEnv, table: Oid, column: &str) -> Result<Option<String>> {
    Err(PgError::unimplemented("CAT-26", "pg_get_serial_sequence"))
}

/// Index access method name (`btree`) and key/option text helpers are shared with the virtual
/// `pg_indexes` view: the `indexdef` text for an `Index` value that is not (yet) in the snapshot
/// (used by `diff.rs` and tests).
// CONTRACT
#[allow(unused_variables)]
pub fn indexdef_for(env: &CatalogEnv, index: &Index) -> Result<String> {
    Err(PgError::unimplemented("CAT-26", "index definition text"))
}
