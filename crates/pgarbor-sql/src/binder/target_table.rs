//! DML target tables: resolution, privilege marks, defaults, generated columns, CHECK constraints
//! (SQL-25). PG references: `parse_clause.c` (`setTargetTable`), `parse_relation.c`
//! (`addRangeTableEntry`), `rewriteHandler.c` (`rewriteTargetListIU`, `build_column_default`,
//! `get_assignment_input`), `analyze.c` (`transformInsertStmt` target handling), `execMain.c` /
//! `execExpr.c` for the constraint expressions the executor needs.
//!
//! [`open_target_table`] resolves the table named in `INSERT INTO t`, `UPDATE t`, `DELETE FROM t`,
//! `COPY t`:
//! * `Binder::qual` + `env.resolve_relation(schema, name)`: 42P01 `relation "t" does not exist`.
//! * Kind checks (`CheckValidRelation`-style, all 42809 / 55000): plain tables and (later)
//!   partitions are writable. A VIEW: v1 supports only the write-error 0A000 `INSERT into a view is not
//!   supported by PGArbor yet` (auto-updatable views are later work; document in the inventory); a
//!   materialized view: 42809 `cannot change materialized view "v"`; a sequence: 42809 `cannot change
//!   sequence "s"`; an index/composite type: 42809 `"x" is not a table`; a system catalog / virtual
//!   relation (OID < 16384): 42501 `permission denied: "pg_class" is a system catalog` (PGArbor catalogs are read-only);
//!   foreign table: 0A000.
//! * Alias: `UPDATE t AS x` / `INSERT INTO t AS x` -> the range entry's alias (columns referenced
//!   as `x.c`; `t.c` is then invalid: 42P01 `invalid reference to FROM-clause entry for table "t"`
//!   HINT `Perhaps you meant to reference the table alias "x".`).
//! * Builds a [`TargetTable`] via [`build_target_table`]; adds `RangeEntry` as range-table entry 1 of
//!   the DML scope (`ScopeKind::{Insert,Update,Delete}`), with `access.insert|update|delete = true`.
//!
//! [`build_target_table`] (`Relation` -> `TargetTable`):
//! * `columns`: one `TargetColumn` per attnum slot. `default`: the column's `StoredExpr` default
//!   re-parsed (`parser::parse_expr_text`) and bound in an expression scope over the table
//!   (`stored::bind_stored_expr`) and assignment-coerced to the column type; IDENTITY columns get
//!   `nextval('<seq>'::regclass)` of the identity sequence (the catalog `Sequence` owned by the
//!   column); `generated: Some((GeneratedKind::Stored, bound expr))` for generated columns, bound
//!   over the target's own columns (`Column{rt_index: 1, attnum}`); `not_null` from
//!   `Column::not_null`; `collation` from the column.
//! * `checks`: every enforced CHECK constraint of the table (`env.snapshot().constraints_of(oid)`,
//!   `ConstraintKind::Check`, `enforced == true`) parsed + bound (boolean). Domain checks of column
//!   types are enforced via `CoerceToDomain` nodes, not listed here.
//! * Cost note: binding defaults for EVERY column on every INSERT is wasteful; bind lazily only the
//!   defaults that can be used (`bind_defaults: bool` parameter: UPDATE/DELETE pass false).
use pgarbor_catalog::Relation;
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{Ident, ObjectName};

use super::context::Binder;
use crate::ir::{RangeEntry, TargetTable};

/// Which statement opens the target (selects the privilege flag and error wording).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlKind {
    Insert,
    Update,
    Delete,
    Copy,
}

/// Resolve + register the DML target; returns the target description and its 1-based range-table
/// index (always 1 for INSERT/COPY; UPDATE/DELETE register it before FROM/USING items so also 1).
#[allow(unused_variables)]
pub fn open_target_table(
    b: &mut Binder,
    name: &ObjectName,
    alias: Option<&Ident>,
    kind: DmlKind,
    bind_defaults: bool,
) -> Result<(TargetTable, u32)> {
    Err(PgError::unimplemented("SQL-25", "DML target resolution (target_table::open_target_table)"))
}

/// `Relation` -> `TargetTable` (module docs).
#[allow(unused_variables)]
pub fn build_target_table(b: &mut Binder, rel: &Relation, bind_defaults: bool) -> Result<TargetTable> {
    Err(PgError::unimplemented("SQL-25", "target table description (target_table::build_target_table)"))
}

/// The `RangeEntry` of a writable target (delegates to `from::relation_range_entry` and sets the
/// access flags for `kind`).
#[allow(unused_variables)]
pub fn target_range_entry(b: &mut Binder, rel: &Relation, alias: Option<&str>, kind: DmlKind) -> Result<RangeEntry> {
    Err(PgError::unimplemented("SQL-25", "target range entry (target_table::target_range_entry)"))
}
