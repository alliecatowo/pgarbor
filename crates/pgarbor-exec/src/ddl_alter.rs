//! `AlterAction` execution: every ALTER other than `ALTER TABLE` commands (lease EXE-36).
//! `exec_alter(cx, action)` = ONE `run_ddl` calling the `CatalogTxn` method named in each
//! `pgarbor_sql::ir::AlterAction` variant's doc, then follow-ups, result tag `action.tag()`.
//!
//! Notes:
//! * Renames and schema moves that change stored expression text pass `&SqlExprRewriter`
//!   (`pgarbor_sql::rewrite::SqlExprRewriter`): `rename_relation`, `set_relation_schema`, `rename_type`,
//!   `set_type_schema`, `rename_function`, `set_function_schema`.
//! * `RenameRelation` with `keyword == Index` uses `rename_index(index, new_name)`; other keywords
//!   `rename_relation(rel, name, rewriter)`.
//! * `Sequence{sequence, options, owned_by}`: `alter_sequence(seq, &options, owned_by)`; its outcome carries
//!   `RestartSequence` when RESTART was given (executed by `run_followups`).
//! * `DomainNotNull{domain, set}` returns the `(table, attnum)` columns using the domain: when
//!   `set == true` scan each table and fail with 23502 `column "a" of table "t" contains null values`
//!   (oracle-verified wording: OF TABLE, not "of relation") if a NULL exists. `AddDomainConstraint` returns `(oid, columns)`; unless `not_valid` validate the new CHECK
//!   against existing values of those columns (23514 `column "a" of table "t" contains values that
//!   violate the new constraint`, oracle-verified). `ValidateDomainConstraint` likewise.
//! * `AddEnumValue`, `RenameEnumValue`, `AlterTypeOwner`, `FunctionAttrs`, `Role`, `RenameRole`, `RoleConfig`,
//!   `RenameDatabase`, `DatabaseOwner`, `DatabaseConfig`, `DatabaseOptions`, `Owner`, `SchemaOwner`,
//!   `RenameSchema`, `IndexOptions`, `ViewOptions`, `DomainDefault`, `RelationOwner`: direct calls.
//! `ALTER TYPE ... ADD VALUE` inside a transaction block is allowed in PG >= 12 (the new label cannot be
//! used until commit; not enforced in v1).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::AlterAction;

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_alter(cx: &mut ExecContext, action: &AlterAction) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-36", "ALTER statements (ddl_alter::exec_alter)"))
}
