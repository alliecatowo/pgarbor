//! DROP, TRUNCATE, DROP OWNED, REASSIGN OWNED, DROP DATABASE (lease EXE-37). Catalog calls:
//! `pgarbor_catalog::ddl::drop_stmt` (`drop_by_names`), `ddl::comment` (`drop_owned`, `reassign_owned`).
//!
//! * [`exec_drop`]: ONE `run_ddl` with `ddl.drop_by_names(kind, &targets, if_exists, behavior)` where
//!   `behavior = DropBehavior::Cascade` if `cascade` else `Restrict`. The outcome's `DropRelationData`
//!   follow-ups remove the heap/index data (`rowstore::delete_relation_data`), `DropSequenceState` the
//!   sequence state. 2BP01 errors (`cannot drop table t because other objects depend on it`) and
//!   `drop cascades to ...` notices come from the catalog. Tag `DROP <KIND>` (`BoundStatement::tag()`).
//! * [`exec_truncate`] (`BoundTruncate{tables, restart_identity, cascade}`):
//!   1. require the `TRUNCATE` privilege on each table (`perm::require_relation_privilege`), lock every table `AccessExclusive`;
//!   2. refuse a table referenced by a foreign key from a table NOT in the list unless `cascade`:
//!      0A000 `cannot truncate a table referenced in a foreign key constraint` DETAIL `Table "c"
//!      references "p".` HINT `Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.`;
//!      with `cascade` also truncate the referencing tables and emit NOTICE `truncate cascades to table
//!      "c"`;
//!   3. for each table delete all heap rows and all index entries (`delete_relation_data` of the table
//!      and of each of its indexes - transactional via the overlay, so ROLLBACK restores them);
//!   4. `restart_identity`: for each identity/serial sequence OWNED by the table's columns restart it at
//!      its start value (`seq_restart`) - via a `run_ddl` + `alter_sequence(RESTART)`, or directly
//!      through `ExecTxn::seq_restart` for owned sequences (ALTER SEQUENCE ... RESTART semantics).
//!   Tag `TRUNCATE TABLE`.
//! * [`exec_drop_owned`] / [`exec_reassign_owned`]: `drop_owned(roles, behavior)`, `reassign_owned(old,
//!   new)` (tags `DROP OWNED`, `REASSIGN OWNED`) followed by follow-ups.
//! * [`exec_drop_database`]: 0A000 `DROP DATABASE` (single database per branch) except for `IF EXISTS`
//!   on a database that does not exist -> NOTICE `database "x" does not exist, skipping`.
use pgarbor_catalog::DropBehavior;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundDrop, BoundTruncate};

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_drop(cx: &mut ExecContext, d: &BoundDrop) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-37", "DROP statements (ddl_drop::exec_drop)"))
}

#[allow(unused_variables)]
pub fn exec_truncate(cx: &mut ExecContext, t: &BoundTruncate) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-37", "TRUNCATE (ddl_drop::exec_truncate)"))
}

#[allow(unused_variables)]
pub fn exec_drop_owned(cx: &mut ExecContext, roles: &[Oid], behavior: DropBehavior) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-37", "DROP OWNED (ddl_drop::exec_drop_owned)"))
}

#[allow(unused_variables)]
pub fn exec_reassign_owned(cx: &mut ExecContext, old_roles: &[Oid], new_owner: Oid) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-37", "REASSIGN OWNED (ddl_drop::exec_reassign_owned)"))
}

#[allow(unused_variables)]
pub fn exec_drop_database(cx: &mut ExecContext, name: &str, if_exists: bool, force: bool) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-37", "DROP DATABASE (ddl_drop::exec_drop_database)"))
}
