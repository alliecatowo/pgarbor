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
use pgarbor_common::{PgError, Result};
use pgarbor_types::Datum;

use crate::dml_update::EpqPolicy;
use crate::runtime::Executor;
use crate::tabledesc::TableDesc;

#[allow(unused_variables)]
pub fn delete_one(
    ex: &mut Executor,
    td: &TableDesc,
    policy: &EpqPolicy,
    input: &[Datum],
    rowid: u64,
) -> Result<Option<Vec<Datum>>> {
    Err(PgError::unimplemented("EXE-29", "DELETE row (dml_delete::delete_one)"))
}

#[allow(unused_variables)]
pub fn apply_delete(ex: &mut Executor, td: &TableDesc, rowid: u64, old_row: &[Datum]) -> Result<bool> {
    Err(PgError::unimplemented("EXE-29", "row deletion (dml_delete::apply_delete)"))
}
