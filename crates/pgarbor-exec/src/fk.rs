//! Foreign-key enforcement (lease EXE-32, sonnet): PostgreSQL's RI triggers
//! (`utils/adt/ri_triggers.c`) re-implemented as direct index probes.
//!
//! ## Hooks (called by `dml_insert`, `dml_update`, `dml_delete` AFTER the row and its index entries are written)
//! * [`after_insert`]: new row in the referencing table -> queue a [`PendingFkCheck::Child`] per FK in
//!   `td.fks_out` whose columns are all non-NULL (MATCH SIMPLE: any NULL skips the check; MATCH FULL:
//!   all-NULL skips, a mix of NULL and non-NULL fails immediately with 23503 `insert or update on
//!   table "c" violates foreign key constraint "fk"` DETAIL `MATCH FULL does not allow mixing of null
//!   and nonnull key values.`).
//! * [`after_update`]: referencing side: queue Child checks only for FKs whose referencing columns
//!   CHANGED (identical key values need no check). Referenced side: for every FK in `td.fks_in` whose
//!   referenced columns changed (compare with `compare::not_distinct` on the key columns), run the
//!   ON UPDATE action for the old key.
//! * [`after_delete`]: referenced side: for every FK in `td.fks_in`, run the ON DELETE action for the
//!   old row's key (skip when any key column is NULL).
//!
//! ## Actions on the referenced side (`apply_parent_action`)
//! Find the referencing rows: probe an index of the FK table whose leading key columns are exactly
//! `fk.cols` (any btree index of `fks_in`'s table; encode the key with the FK-column types), else
//! sequentially scan the FK table comparing `fk.cols` with the OLD key (`sql_eq`, cast to a common type
//! when the FK column type differs from the referenced column type: use
//! `pgarbor_types::cast::cast_datum` assignment/implicit casts from the referenced column type to the
//! FK column type; if no cast exists the FK could not have been created).
//! * `NO ACTION`: queue [`PendingFkCheck::ParentGone`] (checked at statement end: if a row with the old
//!   key exists in the referenced table again, e.g. after `UPDATE p SET id = id + 1`, nothing is
//!   wrong; else if referencing rows exist raise [`crate::error::fk_still_referenced`]).
//! * `RESTRICT`: same check but IMMEDIATELY (not queued) and ignoring re-insertion.
//! * `CASCADE` delete: delete each referencing row through `dml_delete::apply_delete` (which recurses
//!   into its own `fks_in`); `CASCADE` update: update the FK columns of each referencing row to the new
//!   referenced values through `dml_update::apply_update`.
//! * `SET NULL` / `SET DEFAULT`: update the FK columns (`delete_set_columns` if non-empty for ON
//!   DELETE) of each referencing row to NULL / the column default (`TableDesc` default exprs via
//!   the `tabledesc::bind_stored_text` of the column default, evaluated with `eval`); a NOT NULL FK
//!   column then fails with 23502 as usual.
//! * Recursion depth is bounded by 1000 nested cascade levels (54001 `stack depth limit exceeded`).
//! Row locks: every referencing row is changed through `rowstore::lock_row` first (EvalPlanQual not
//! needed: a concurrently deleted row is simply skipped).
//!
//! ## End-of-statement checks (`run_pending`)
//! Called by `modify` after all rows were applied, before RETURNING rows are released.
//! * `Child { fk, key }` (key values in the FK table's column types): look up the referenced key in
//!   the unique index `fk.ref_index` of the referenced table (cast the values to the referenced column
//!   types first; encode with the index `key_cols`; entry key `[ref_index][enc]` exists?), then take
//!   `ExecTxn::lock_and_read_latest(heap row key, ForKeyShare)` on the parent row and require the
//!   latest value to exist (a concurrent committed DELETE of the parent => violation). Missing =>
//!   [`crate::error::fk_missing_parent`] with `key_description` of the FK column names/values.
//! * `ParentGone { fk, old_key }`: see NO ACTION above.
//! Deferrable constraints are treated as immediate-at-end-of-statement in v1 (known gap: INITIALLY
//! DEFERRED is not honored until COMMIT; `SET CONSTRAINTS` is a no-op).
//!
//! ## Validation (`validate_foreign_key`, FollowUp::ValidateForeignKey)
//! Scan the whole FK table; for every row with a fully non-NULL key run the Child check; first failure
//! is returned (PG reports the same message/detail as an INSERT would).
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::Datum;

use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// A check postponed to the end of the statement (module docs).
// CONTRACT
#[derive(Clone, Debug)]
pub enum PendingFkCheck {
    /// The referencing row with these FK-column values must have a parent.
    Child { fk: Oid, key: Vec<Datum> },
    /// The referenced key `old_key` disappeared (or changed): no referencing row may remain.
    ParentGone { fk: Oid, old_key: Vec<Datum> },
}

/// Hook after a heap row was inserted (module docs).
#[allow(unused_variables)]
pub fn after_insert(ex: &mut Executor, td: &TableDesc, new_row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-32", "FK checks after insert (fk::after_insert)"))
}

/// Hook after a heap row was updated in place (module docs).
#[allow(unused_variables)]
pub fn after_update(ex: &mut Executor, td: &TableDesc, old_row: &[Datum], new_row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-32", "FK checks after update (fk::after_update)"))
}

/// Hook after a heap row was deleted (module docs).
#[allow(unused_variables)]
pub fn after_delete(ex: &mut Executor, td: &TableDesc, old_row: &[Datum]) -> Result<()> {
    Err(PgError::unimplemented("EXE-32", "FK actions after delete (fk::after_delete)"))
}

/// Execute and clear `ex.cx.stmt.pending_fk` (module docs).
#[allow(unused_variables)]
pub fn run_pending(ex: &mut Executor) -> Result<()> {
    Err(PgError::unimplemented("EXE-32", "pending FK checks (fk::run_pending)"))
}

/// Validate an existing foreign key against all rows of `table` (module docs).
#[allow(unused_variables)]
pub fn validate_foreign_key(ex: &mut Executor, table: Oid, constraint: Oid) -> Result<()> {
    Err(PgError::unimplemented("EXE-32", "FK validation (fk::validate_foreign_key)"))
}
