//! `ModifyTable` operator: INSERT / UPDATE / DELETE driver (lease EXE-26, sonnet, north-star).
//! PG: `nodeModifyTable.c`. Layout contract: plan.rs `ModifyTableNode` (read its docs first).
//!
//! ## Execution (all work happens inside the FIRST `next` call; later calls only drain RETURNING)
//! 1. `perm::require_relation_privilege` (`INSERT` / `UPDATE` / `DELETE`; columns = supplied / assigned
//!    attnums), `check_writable` with the command name (`INSERT`/`UPDATE`/`DELETE`; 25006 in read-only
//!    transactions) and `lock_relation(table.oid, RowExclusive, Block)`.
//! 2. `td = table_desc(..)`; DRAIN the whole input child into a `Vec<Row>` before writing anything
//!    (Halloween protection: the statement must not see its own writes; also lets row ids of
//!    `INSERT ... SELECT FROM same_table` terminate).
//! 3. Per input row (call `ex.check_interrupts()` every 1024 rows):
//!    * `Insert`: `dml_insert::insert_input_row(.., &mut conflict_state)`; `Some(r)` => count + keep
//!      `r.row` for RETURNING.
//!    * `Update`: `rowid = input[rowid_slot]`; skip duplicates (the same rowid seen before: first match
//!      wins, PG semantics); `dml_update::update_one` (policy from [`epq_policy`]); `Some(new)` => count + keep
//!      `new` (+ the input's extra FROM columns for RETURNING).
//!    * `Delete`: same with `dml_delete::delete_one`; the kept image is the deleted row.
//! 4. After the last row: `fk::run_pending(ex)` (end-of-statement foreign-key checks); then, if
//!    `returning` is non-empty, evaluate the RETURNING expressions for every kept row over the
//!    RETURNING INPUT ROW = `kept image (ncols) ++ input[returning_extra_from..]` (see plan.rs) and
//!    queue the result rows; `rows_affected` = number of rows inserted/updated/deleted (rows skipped by
//!    ON CONFLICT DO NOTHING, concurrently deleted, or `WHERE`-rechecked-away are NOT counted).
//! 5. `next` then yields the queued RETURNING rows (none without RETURNING: the operator returns
//!    `None` right away after the work) and `affected_rows()` returns the count.
//! Errors abort the statement; the transaction layer discards all writes (implicit transaction) or
//! marks the block failed (explicit).
//!
//! ## EvalPlanQual policy ([`epq_policy`])
//! Walk down the UPDATE/DELETE input: a chain of `Filter` nodes ending in `SeqScan` / `IndexScan` on the
//! target table (`with_rowid`) with nothing else (no joins, no `ProjectSet`, no sub-queries as the
//! scan source) is "simple": the quals to re-check are the scan `filter`, the `index_cond`s (when the scan
//! is an `IndexScan`) and every `Filter::predicate`, all expressed over the target table row. Anything
//! else yields `EpqPolicy { quals: None }`.
//! `InputRef` slots in those quals index the scan's output = the table row, so they can be evaluated
//! directly over a `latest` image.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{ModifyTableNode, PlanNode};

use crate::build::BuildCtx;
use crate::dml_update::EpqPolicy;
use crate::runtime::RowSource;

#[allow(unused_variables)]
pub fn build_modify_table(node: &PlanNode, n: &ModifyTableNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    Err(PgError::unimplemented("EXE-26", "ModifyTable node (modify::build_modify_table)"))
}

/// Derive the EvalPlanQual policy from the ModifyTable input subtree (module docs).
#[allow(unused_variables)]
pub fn epq_policy(input: &PlanNode) -> Result<EpqPolicy> {
    Err(PgError::unimplemented("EXE-26", "EvalPlanQual policy (modify::epq_policy)"))
}
