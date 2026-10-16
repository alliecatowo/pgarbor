//! UPDATE / DELETE of rows that belong to inheritance children or partitions of the statement's
//! target (amendment pgarbor-exec-11). PG references: `nodeModifyTable.c` (`ExecUpdate`,
//! `ExecCrossPartitionUpdate`, `ExecDelete`), `execMain.c` (`ExecPartitionCheck`).
//!
//! The planner (`pgarbor_plan::inherit`) feeds the ModifyTable the PARENT's row layout; the rowid
//! slot of a row read from member `m` (not the target itself) holds `ROW(m, rowid)`. Here:
//! * the old member row is locked and read, assignments are evaluated over the parent-layout input
//!   row exactly as for the target, and the new values are mapped onto the member's columns by name;
//! * a partition whose new row violates its partition constraint moves: when the target is a
//!   partitioned table the row is deleted from the member and routed again from the target
//!   (`ExecCrossPartitionUpdate`); otherwise 23514 `new row for relation "p" violates partition
//!   constraint`;
//! * RETURNING images are converted back to the target's layout.
use pgarbor_catalog::model::RelKind;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_plan::plan::{ModifyTarget, UpdateSpec};
use pgarbor_types::Datum;

use crate::dml_delete;
use crate::dml_insert;
use crate::dml_update;
use crate::error;
use crate::partition;
use crate::rowstore::{self, RowLock};
use crate::runtime::Executor;
use crate::tabledesc::{table_desc, TableDesc};

/// `(relation, rowid)` of an UPDATE/DELETE input row: a plain int8 rowid belongs to `target`,
/// `ROW(oid, rowid)` to an inheritance member. `Ok(None)` for a NULL slot (no target row).
pub fn row_identity(input: &[Datum], slot: Option<u32>, target: Oid) -> Result<Option<(Oid, u64)>> {
    let slot = slot.ok_or_else(|| PgError::internal("ModifyTable: missing rowid slot"))? as usize;
    match input.get(slot) {
        Some(Datum::Int8(v)) => Ok(Some((target, *v as u64))),
        Some(Datum::Record(fields)) => match fields.as_slice() {
            [Datum::Oid(m), Datum::Int8(r)] => Ok(Some((*m, *r as u64))),
            _ => Err(PgError::internal("ModifyTable: malformed member row identity")),
        },
        Some(Datum::Null) => Ok(None),
        Some(_) => Err(PgError::internal("ModifyTable: rowid slot is not int8")),
        None => Err(PgError::internal("ModifyTable: rowid slot out of range")),
    }
}

fn concurrent_update() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update")
}

/// Lock and read a member row; `None` when it is gone.
fn lock_member(ex: &mut Executor, mtd: &TableDesc, rowid: u64) -> Result<Option<Vec<Datum>>> {
    match rowstore::lock_row(ex, mtd, rowid, LockMode::ForUpdate, WaitPolicy::Block)? {
        RowLock::Gone => Ok(None),
        RowLock::Current(img) => Ok(Some(img)),
        RowLock::NotAvailable => Err(error::row_lock_not_available(&mtd.name)),
        RowLock::Changed(_) => Err(concurrent_update()),
    }
}

/// UPDATE one row of member `member` of the target `td` (module docs). Returns the new image in the
/// target's layout.
#[allow(clippy::too_many_arguments)]
pub fn update_member(
    ex: &mut Executor,
    td: &TableDesc,
    target: &ModifyTarget,
    spec: &UpdateSpec,
    input: &[Datum],
    member: Oid,
    rowid: u64,
) -> Result<Option<(Oid, Vec<Datum>)>> {
    let mtd = table_desc(&*ex.cx, member)?;
    let Some(old_member) = lock_member(ex, &mtd, rowid)? else { return Ok(None) };
    let old_parent = partition::convert_row(&mtd, td, &old_member);
    let new_parent = dml_update::build_new_row(ex, td, target, spec, &old_parent, input)?;
    let mut new_member = old_member.clone();
    let mut modified: Vec<i16> = Vec::new();
    for (i, c) in td.cols.iter().enumerate() {
        if c.dropped {
            continue;
        }
        if let Some(mc) = mtd.col_by_name(&c.name) {
            let slot = (mc.attnum - 1) as usize;
            if let (Some(dst), Some(v)) = (new_member.get_mut(slot), new_parent.get(i)) {
                *dst = v.clone();
            }
            if spec.assignments.iter().any(|(a, _)| *a == c.attnum) {
                modified.push(mc.attnum);
            }
        }
    }
    if partition::is_partition(&*ex.cx, member)? && !partition::check_partition_constraint(ex, &mtd, &new_member)? {
        if td.kind != RelKind::PartitionedTable {
            return Err(partition::partition_constraint_error(ex, &mtd, &new_member));
        }
        // Cross-partition UPDATE: delete here, insert through the target's routing.
        if !dml_delete::apply_delete(ex, &mtd, rowid, &old_member)? {
            return Ok(None);
        }
        let root = table_desc(&*ex.cx, td.oid)?;
        let routed = partition::convert_row(&mtd, &root, &new_member);
        let (leaf, leaf_row) = partition::route(ex, root, routed)?;
        dml_insert::insert_row(ex, &leaf, leaf_row.clone())?;
        return Ok(Some((leaf.oid, partition::convert_row(&leaf, td, &leaf_row))));
    }
    let stored = dml_update::apply_update(ex, &mtd, rowid, &old_member, new_member, Some(modified.as_slice()))?;
    Ok(Some((member, partition::convert_row(&mtd, td, &stored))))
}

/// DELETE one row of member `member`; returns the deleted image in the target's layout.
pub fn delete_member(ex: &mut Executor, td: &TableDesc, member: Oid, rowid: u64) -> Result<Option<(Oid, Vec<Datum>)>> {
    let mtd = table_desc(&*ex.cx, member)?;
    let Some(old_member) = lock_member(ex, &mtd, rowid)? else { return Ok(None) };
    if !dml_delete::apply_delete(ex, &mtd, rowid, &old_member)? {
        return Ok(None);
    }
    Ok(Some((member, partition::convert_row(&mtd, td, &old_member))))
}
