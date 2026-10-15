//! COPY (SQL-42). PG references: `copy.c` (`DoCopy`, `ProcessCopyOptions`, `CopyGetAttnums`),
//! `copyfrom.c`, `copyto.c`, `gram.y` (`CopyStmt`, `copy_opt_list`, `copy_generic_opt_elem`).
//!
//! `bind_copy(b, stmt)` (`Statement::Copy { source: CopySource, to, target: CopyTarget, options:
//! Vec<CopyOption>, legacy_options: Vec<CopyLegacyOption>, values }`) -> `BoundStatement::Copy(Box<BoundCopy>)`.
//! * `CopySource::Table { table_name, columns }` -> `CopyTarget::Table { table: TargetTable (via
//!   `target_table::open_target_table(.., DmlKind::Copy, true)` for COPY FROM; a relation lookup with
//!   defaults not needed for COPY TO), columns: attnums }` (column list order; none = all live non-generated
//!   columns for FROM, all live columns for TO; unknown column 42703 `column "x" of relation "t" does not
//!   exist`; duplicate 42701 `column "x" specified more than once`; generated column in FROM list: 428C9?
//!   PG allows listing? no -> `column "g" is a generated column` DETAIL `Generated columns cannot be used in
//!   COPY.`). `CopySource::Query(q)` (COPY TO only: `(SELECT ...) TO ...`; FROM + query: 0A000/42601) ->
//!   `CopyTarget::Query(Box<BoundQuery>)` bound with `select::bind_query`; DML with RETURNING as source
//!   0A000.
//! * Endpoint: `CopyTarget::{Stdin, Stdout, File{name}, Program{command}}` -> `CopyEndpoint`; direction
//!   from `to`; `COPY FROM STDOUT`/`TO STDIN` are grammar errors (42601). Server-side `FILE`/`PROGRAM`
//!   privilege checks are the executor's (42501 `permission denied to COPY to or from a file` ...).
//! * Options (new `WITH (...)` list `CopyOption` and legacy `WITH CSV HEADER ...` `CopyLegacyOption`):
//!   `FORMAT text|csv|binary`, `FREEZE`, `DELIMITER 'c'` (single one-byte char: 22023 `COPY delimiter must
//!   be a single one-byte character`; not in binary), `NULL 'str'`, `DEFAULT 'str'` (FROM only),
//!   `HEADER [bool|MATCH]` (CSV/text; `MATCH` -> 0A000), `QUOTE`, `ESCAPE` (CSV only), `FORCE_QUOTE (cols)|*`
//!   (TO CSV), `FORCE_NOT_NULL (cols)`, `FORCE_NULL (cols)` (FROM CSV), `ON_ERROR`, `LOG_VERBOSITY`,
//!   `REJECT_LIMIT` (0A000 unless `stop`/default), `ENCODING 'x'` (only UTF8 accepted). Defaults resolved:
//!   text delimiter TAB, null `\N`, CSV delimiter `,`, null empty string, quote `"`, escape = quote. Option
//!   conflicts with PG messages (22023 `COPY QUOTE requires CSV mode` - same pattern for ESCAPE,
//!   FORCE_QUOTE, FORCE_NOT_NULL, FORCE_NULL; `COPY delimiter character must not appear in the NULL
//!   specification`; HEADER is accepted in text mode in PG 18 (oracle: `copy t to stdout (header)` prints a
//!   header row); `COPY FREEZE` cannot be used with COPY TO). Redshift/Snowflake legacy options (`IAM_ROLE`, `GZIP`, ...): 42601.
//! * `WHERE cond` (sqlparser: `where_clause`? if present) -> boolean over the target's columns in a
//!   `TableExpr` scope; COPY TO with WHERE: 0A000 `WHERE clause not allowed with COPY TO`.
//! * COPY of a view/matview/sequence/foreign table as FROM target: 42809 `cannot copy to view "v"` HINT
//!   `To enable copying to a view, provide an INSTEAD OF INSERT trigger.`; TO a view is allowed in PG 18
//!   only via query: `cannot copy from view "v"` HINT `Try the COPY (SELECT ...) TO variant.`
use pgarbor_common::{PgError, Result};
use sqlparser::ast::Statement;

use super::context::Binder;
use crate::ir::BoundStatement;

/// Bind COPY.
#[allow(unused_variables)]
pub fn bind_copy(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    Err(PgError::unimplemented("SQL-42", "COPY binding (copy::bind_copy)"))
}
