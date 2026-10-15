//! `INSERT ... ON CONFLICT` (SQL-27). PG references: `parse_clause.c` (`transformOnConflictArbiter`,
//! `resolve_unique_index_expr`, `infer_arbiter_indexes` is planner/`plancat.c`), `analyze.c`
//! (`transformOnConflictClause`), `gram.y` (`opt_on_conflict`).
//!
//! `bind_on_conflict(b, oc, target, columns)` (sqlparser `OnConflict { conflict_target, action }`):
//! * **Arbiter** (`Arbiter`): `ConflictTarget::Columns(idents)` -> find the unique indexes of the target
//!   whose key columns EXACTLY match the set of inferred columns (order-insensitive), non-partial, non-
//!   deferrable, valid (use `env.snapshot().indexes_of(table)`); none -> 42P10 `there is no unique or
//!   exclusion constraint matching the ON CONFLICT specification`. Expression inference elements
//!   (`ON CONFLICT ((lower(a)))`) and `WHERE index_predicate` are not representable in sqlparser 0.63's
//!   `ConflictTarget::Columns(Vec<Ident>)`; document as a gap. `ConflictTarget::OnConstraint(name)` ->
//!   the named unique/PK constraint's backing index (42704 `constraint "x" for table "t" does not
//!   exist`, oracle; a CHECK constraint name: 42809 `constraint in ON CONFLICT clause has no associated
//!   index`). No target: only legal with `DO NOTHING`
//!   (`Arbiter { indexes: [], any_unique: true }`); with DO UPDATE: 42601 `ON CONFLICT DO UPDATE
//!   requires inference specification or constraint name` HINT `For example, ON CONFLICT (column_name).`
//! * **DO NOTHING** -> `ConflictAction::DoNothing`.
//! * **DO UPDATE SET ... [WHERE]**: register the pseudo relation `excluded` (columns = target
//!   columns, `PseudoRelation::Excluded`, same attnums, `access.select`) as range entry 2 of the INSERT
//!   scope and make both the target (by its alias/name) and `excluded` visible. Assignments are bound by
//!   `update_delete::bind_assignments` against the target (`excluded.c` and `t.c` references allowed in
//!   values and WHERE; the SET targets are unqualified column names of the target). `DoUpdate.selection`
//!   -> `coerce_to_boolean("WHERE")`. A column assigned twice: 42601 `multiple assignments to same
//!   column "x"`. Generated columns / identity ALWAYS as SET targets: 428C9 as for UPDATE.
//! * Result `BoundOnConflict { arbiter, action, excluded_rt }` where `excluded_rt` is the range-table
//!   index of `excluded` (0 for DO NOTHING).
//! * Runtime errors the executor raises (documented here for the bind side): 21000 `ON CONFLICT DO
//!   UPDATE command cannot affect row a second time` HINT `Ensure that no rows proposed for insertion
//!   within the same command have duplicate constrained values.`
use pgarbor_common::{PgError, Result};
use sqlparser::ast::OnConflict;

use super::context::Binder;
use crate::ir::{BoundOnConflict, TargetTable};

/// Bind the ON CONFLICT clause of an INSERT (module docs). `columns` = target attnums of the INSERT
/// column list (for the `excluded` row layout the executor uses all target columns).
#[allow(unused_variables)]
pub fn bind_on_conflict(b: &mut Binder, oc: &OnConflict, target: &TargetTable, columns: &[i16]) -> Result<BoundOnConflict> {
    Err(PgError::unimplemented("SQL-27", "ON CONFLICT binding (on_conflict::bind_on_conflict)"))
}
