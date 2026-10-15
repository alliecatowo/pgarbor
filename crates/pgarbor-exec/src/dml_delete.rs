//! DELETE row processing (lease EXE-29). PG: `nodeModifyTable.c` (`ExecDelete`, EvalPlanQual).
//!
//! * [`delete_one`]: input row = `table row (ncols) ++ rowid ++ extra USING columns`;
//!   `rowstore::lock_row(.., ForUpdate, Block)` then exactly like `dml_update::update_one`'s locking
//!   step (`Gone` -> `None`; `Changed(latest)` -> EvalPlanQual per `EpqPolicy`: `quals == None` ->
//!   40001 `could not serialize access due to concurrent update`; else quals must still hold on `latest`).
//!   Then [`apply_delete`] and return the DELETED image (the RETURNING input).
//! * [`apply_delete`] (also used by FK cascades): `rowstore::delete_heap` + `index_maint::delete_entries`
//!   (entries are recomputed from `old_row`, which must be the exact stored image) + `fk::after_delete`
//!   (parent-side actions: NO ACTION/RESTRICT checks, CASCADE, SET NULL/DEFAULT). Returns whether the
//!   heap key existed.
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::dml_update::EpqPolicy;
use crate::error;
use crate::eval;
use crate::fk;
use crate::index_maint;
use crate::rowstore::{self, RowLock};
use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

/// Message of the 40001 raised when a joined or multi-table DELETE meets a concurrently changed row
/// (PG `heapam_tuple_update` / `ExecDelete` under READ COMMITTED with no EPQ support here).
const CONCURRENT_UPDATE_MESSAGE: &str = "could not serialize access due to concurrent update";

/// The EvalPlanQual quals of `policy`, or the 40001 error when EPQ is unsupported for this plan
/// shape (`quals == None`, e.g. joins). `Some(empty)` means "no quals to re-check".
fn epq_quals(policy: &EpqPolicy) -> Result<&[BoundExpr]> {
    match &policy.quals {
        Some(quals) => Ok(quals.as_slice()),
        None => Err(PgError::new(SqlState::SERIALIZATION_FAILURE, CONCURRENT_UPDATE_MESSAGE)),
    }
}

/// Delete one target row (module docs). Returns the deleted image (`ncols` values; the caller appends
/// the USING columns for RETURNING), or `None` when the row is gone or no longer matches the quals.
pub fn delete_one(
    ex: &mut Executor,
    td: &TableDesc,
    policy: &EpqPolicy,
    input: &[Datum],
    rowid: u64,
) -> Result<Option<Vec<Datum>>> {
    let ncols = td.ncols();
    if input.len() < ncols {
        return Err(PgError::internal(format!(
            "DELETE input row has {} slots but table \"{}\" has {} columns",
            input.len(),
            td.name,
            ncols
        )));
    }

    let old: Vec<Datum> = match rowstore::lock_row(ex, td, rowid, LockMode::ForUpdate, WaitPolicy::Block)? {
        // Deleted by a concurrent committed transaction (or by this one): nothing to delete.
        RowLock::Gone => return Ok(None),
        // Cannot happen with WaitPolicy::Block; report it as the PG lock error rather than guessing.
        RowLock::NotAvailable => return Err(error::row_lock_not_available(&td.name)),
        RowLock::Current(img) => img,
        RowLock::Changed(latest) => {
            // READ COMMITTED EvalPlanQual: every qual must still hold on the latest version.
            let quals = epq_quals(policy)?;
            for q in quals {
                if !eval::eval_qual(ex, q, &latest)? {
                    return Ok(None);
                }
            }
            latest
        }
    };

    if !apply_delete(ex, td, rowid, &old)? {
        return Ok(None);
    }
    Ok(Some(old))
}

/// Remove a heap row, its index entries and run the parent-side foreign-key actions (module docs).
/// `old_row` must be the exact stored image of row `rowid` (index entries are recomputed from it).
/// Returns `false` (and does nothing else) when the heap key does not exist in the transaction view.
pub fn apply_delete(ex: &mut Executor, td: &TableDesc, rowid: u64, old_row: &[Datum]) -> Result<bool> {
    let existed = rowstore::delete_heap(ex, td.oid, rowid)?;
    if !existed {
        return Ok(false);
    }
    index_maint::delete_entries(ex, td, rowid, old_row)?;
    fk::after_delete(ex, td, old_row)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epq_without_quals_is_serialization_failure() {
        let policy = EpqPolicy { quals: None };
        match epq_quals(&policy) {
            Ok(_) => panic!("EPQ without quals must fail"),
            Err(e) => {
                assert!(e.sqlstate == SqlState::SERIALIZATION_FAILURE);
                assert_eq!(e.message, CONCURRENT_UPDATE_MESSAGE);
            }
        }
    }

    #[test]
    fn epq_with_empty_quals_checks_nothing() {
        let policy = EpqPolicy { quals: Some(Vec::new()) };
        match epq_quals(&policy) {
            Ok(q) => assert!(q.is_empty()),
            Err(_) => panic!("empty quals must be accepted"),
        }
    }
    // End-to-end delete tests (heap and index removal, concurrent `Gone`, FK hook) need a live
    // Executor over EXE-02 / EXE-04 / EXE-32 and run in the central integration pass.
}
