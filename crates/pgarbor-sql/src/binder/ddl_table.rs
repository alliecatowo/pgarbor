//! CREATE TABLE and CREATE TABLE AS / SELECT INTO (SQL-30). PG references: `parse_utilcmd.c`
//! (`transformCreateStmt`, `transformColumnDefinition`, `transformTableConstraint`, serial handling
//! `generateSerialExtraStmts`, `transformIndexConstraints`), `tablecmds.c` (`DefineRelation`,
//! `MergeAttributes`), `createas.c`, `gram.y` (`columnDef`, `ColQualList`, `TableConstraint`).
//!
//! `bind_create_table(b, ct)` -> `BoundStatement::CreateTable(Box<CreateTableSpec>)` (or
//! `CreateTableAs` when `ct.query` is present):
//! * Name: `Binder::qual` + `env.creation_namespace_for(schema, name)` (first schema of search_path when
//!   unqualified; 3F000 `no schema has been selected to create in` / 3F000 `schema "x" does not exist`);
//!   `IF NOT EXISTS`; `OR REPLACE` not valid for tables (42601).
//! * Unsupported -> 0A000 naming the feature: `TEMP`/`TEMPORARY`/`GLOBAL`/`LOCAL` (temp tables),
//!   `INHERITS`, `PARTITION BY`, `PARTITION OF`, `OF type_name`, `LIKE` (is supported by PG; v1: 0A000
//!   `CREATE TABLE ... LIKE`), `ON COMMIT`, tablespaces, `WITHOUT OIDS`, foreign tables, non-PG options
//!   (`ENGINE=`, `COMMENT=`, `CLUSTER BY`, `ORDER BY`, `AS ... WITH NO DATA` is supported).
//!   `UNLOGGED` -> `Persistence::Unlogged`; storage `WITH (fillfactor=70)` -> `options` as `name=value`.
//! * **Columns** (`ColumnDef { name, data_type, options }`): fold + truncate names (`Binder::ident`);
//!   duplicates 42701 `column "a" specified more than once`; more than 1600 columns: 54011 `tables
//!   can have at most 1600 columns`. Type: serial pseudo-types (`serial`, `serial2`, `serial4`,
//!   `serial8`, `smallserial`, `bigserial` — arrive as `DataType::Custom`) -> `ColumnSpec.serial =
//!   Some(SerialKind)`, `type_id` = int2/int4/int8, NOT NULL implied; otherwise
//!   `types::resolve_data_type`. Pseudo-types: 42P16 `column "c" has pseudo-type anyelement`
//!   (`types::is_pseudo_type`); shell/undefined: 42704.
//! * **Column options** (`ColumnOption`): `NotNull` -> `not_null: Some(None)`; `Null` -> explicit
//!   nullable; `Default(expr)` -> `stored::bind_ddl_expr` with `ExprCtx::column_default()` coerced
//!   (Assignment) to the column type (`bind_ddl_expr` target) -> `default`; a second DEFAULT: 42601
//!   `multiple default values specified for column "c" of table "t"` (`diag::invalid_table_definition`,
//!   42601 per PG); `Collation(name)` -> `collation: Some(oid)` (42804 `collations are not supported by
//!   type integer`); `Generated { generated_as, sequence_options, generation_expr, generation_expr_mode }`:
//!   `ALWAYS`/`BY DEFAULT AS IDENTITY [(seq options)]` -> `identity: Some(IdentitySpec{kind, options})`
//!   (`ExprCtx` none; type must be smallint/integer/bigint: 22023 `identity column type must be
//!   smallint, integer, or bigint`; identity + DEFAULT: 42601 `both default and identity specified for
//!   column "c" of table "t"`), `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]` -> `generated:
//!   Some(GeneratedSpec{kind, expr})` bound with `ExprCtx::generated()` (generated columns may only use
//!   immutable functions: 42P17 `generation expression is not immutable`; may not reference other
//!   generated columns; can't have a DEFAULT: 42601 `both default and generation expression specified
//!   for column "c" of table "t"`); `PrimaryKey`/`Unique`/`ForeignKey`/`Check` column constraints ->
//!   `ddl_constraint::bind_column_constraint` (they become table `ConstraintSpec`s); `Comment`,
//!   `OnUpdate`, `CharacterSet`, `Materialized`, `Ephemeral`, `Alias`, `Options`, `Policy`, `Tags`,
//!   `Identity`(MSSQL), `OnConflict`, `DialectSpecific`: 42601.
//! * **Table constraints** (`TableConstraint`): `ddl_constraint::bind_table_constraint`. Constraint-name
//!   collisions are the catalog's job (42710 `constraint "x" for relation "t" already exists`).
//!   Multiple PRIMARY KEYs: 42P16 `multiple primary keys for table "t" are not allowed`.
//!   PRIMARY KEY columns become NOT NULL (catalog does it; binder only lists columns, 42703 `column
//!   "x" named in key does not exist`).
//! * Result: `CreateTableSpec { namespace, name, owner: env.current_user(), kind: RelKind::Table,
//!   persistence, columns, constraints, options, if_not_exists, inherits: [] }`.
//!
//! ## CREATE TABLE AS ([`bind_create_table_as`]) / SELECT INTO ([`bind_select_into`])
//! Bind the query (`select::bind_query`, `keep_unknown_outputs: false`: unknown -> text); columns = query
//! outputs renamed by the optional column list (more names than columns 42601 `too many column names
//! were specified`); no constraints; `with_data` from `WITH [NO] DATA`. Duplicate output names:
//! 42701 `column "x" specified more than once`. `IF NOT EXISTS`, `UNLOGGED` honored; TEMP 0A000;
//! `CREATE TABLE AS EXECUTE` 0A000. Result `BoundStatement::CreateTableAs`.
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{CreateTable, Query, SelectInto};

use super::context::Binder;
use crate::ir::BoundStatement;

/// Bind CREATE TABLE (module docs).
#[allow(unused_variables)]
pub fn bind_create_table(b: &mut Binder, ct: &CreateTable) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-30", "CREATE TABLE binding (ddl_table::bind_create_table)"))
}

/// `CREATE TABLE name [(cols)] AS query [WITH [NO] DATA]` (called by `bind_create_table` when
/// `ct.query` is set).
#[allow(unused_variables)]
pub fn bind_create_table_as(b: &mut Binder, ct: &CreateTable) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-30", "CREATE TABLE AS binding (ddl_table::bind_create_table_as)"))
}

/// `SELECT ... INTO [TEMP] t FROM ...` (statement-level; `into` taken from the top SELECT block).
#[allow(unused_variables)]
pub fn bind_select_into(b: &mut Binder, q: &Query, into: &SelectInto) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-30", "SELECT INTO binding (ddl_table::bind_select_into)"))
}
