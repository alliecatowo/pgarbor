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
use std::collections::{HashSet, VecDeque};

use pgarbor_catalog::model::acl_mode;
use pgarbor_common::{PgError, Result};
use pgarbor_core::locks::LockMode;
use pgarbor_core::WaitPolicy;
use pgarbor_plan::plan::{
    ConflictPlanAction, InsertSpec, ModifyOp, ModifyTableNode, ModifyTarget, NodeKind, PlanNode, UpdateSpec,
};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::build::BuildCtx;
use crate::dml_conflict::ConflictState;
use crate::dml_delete;
use crate::dml_insert;
use crate::dml_update::{self, EpqPolicy};
use crate::error;
use crate::eval;
use crate::fk;
use crate::perm;
use crate::runtime::{Executor, Row, RowSource};
use crate::tabledesc::table_desc;

/// Runtime state of one `ModifyTable` operator (module docs). Owns copies of everything it needs.
struct ModifyTableOp {
    op: ModifyOp,
    target: ModifyTarget,
    input: Box<dyn RowSource>,
    rowid_slot: Option<u32>,
    insert: Option<InsertSpec>,
    update: Option<UpdateSpec>,
    returning: Vec<BoundExpr>,
    returning_extra_from: Option<u32>,
    policy: EpqPolicy,
    /// The first `next` call has run the whole statement.
    started: bool,
    /// Rows inserted / updated / deleted.
    affected: u64,
    /// RETURNING rows not yet handed out.
    queue: VecDeque<Row>,
}

/// Build the ModifyTable operator (module docs). Privilege, read-only and ON CONFLICT UPDATE privilege
/// checks happen here; the table lock and all writes happen in the first `next`.
pub fn build_modify_table(node: &PlanNode, n: &ModifyTableNode, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let _ = node;
    let oid = n.target.table.oid;
    let (command, mode, columns): (&str, u32, Vec<i16>) = match n.op {
        ModifyOp::Insert => {
            let spec = n
                .insert
                .as_ref()
                .ok_or_else(|| PgError::internal("INSERT ModifyTable without an insert spec"))?;
            ("INSERT", acl_mode::INSERT, spec.columns.clone())
        }
        ModifyOp::Update => {
            let spec = n
                .update
                .as_ref()
                .ok_or_else(|| PgError::internal("UPDATE ModifyTable without an update spec"))?;
            ("UPDATE", acl_mode::UPDATE, spec.assignments.iter().map(|(attnum, _)| *attnum).collect())
        }
        ModifyOp::Delete => ("DELETE", acl_mode::DELETE, Vec::new()),
    };
    if n.op != ModifyOp::Insert && n.rowid_slot.is_none() {
        return Err(PgError::internal("UPDATE/DELETE ModifyTable without a rowid slot"));
    }
    perm::require_relation_privilege(&b.cx.se, oid, mode, &columns)?;
    if let Some(spec) = &n.insert {
        if let Some(conflict) = &spec.on_conflict {
            if let ConflictPlanAction::DoUpdate { assignments, .. } = &conflict.action {
                let assigned: Vec<i16> = assignments.iter().map(|(attnum, _)| *attnum).collect();
                perm::require_relation_privilege(&b.cx.se, oid, acl_mode::UPDATE, &assigned)?;
            }
        }
    }
    b.cx.txn.check_writable(command)?;
    let input = b.child(&n.input)?;
    let policy = match n.op {
        ModifyOp::Insert => EpqPolicy::default(),
        ModifyOp::Update | ModifyOp::Delete => epq_policy(&n.input)?,
    };
    Ok(Box::new(ModifyTableOp {
        op: n.op,
        target: n.target.clone(),
        input,
        rowid_slot: n.rowid_slot,
        insert: n.insert.clone(),
        update: n.update.clone(),
        returning: n.returning.clone(),
        returning_extra_from: n.returning_extra_from,
        policy,
        started: false,
        affected: 0,
        queue: VecDeque::new(),
    }))
}

impl ModifyTableOp {
    /// Lock the target, drain the input, apply every row, run the end-of-statement FK checks and
    /// evaluate RETURNING into `queue` (module docs, steps 1-4).
    fn run(&mut self, ex: &mut Executor) -> Result<()> {
        let oid = self.target.table.oid;
        let locked = ex.txn().lock_relation(oid, LockMode::RowExclusive, WaitPolicy::Block)?;
        if !locked {
            return Err(error::relation_lock_not_available(&self.target.table.name));
        }
        let td = table_desc(&*ex.cx, oid)?;

        // Halloween protection: nothing is written before the whole input has been read.
        let mut input_rows: Vec<Row> = Vec::new();
        let mut ticks: u32 = 0;
        while let Some(row) = self.input.next(ex)? {
            ex.tick(&mut ticks)?;
            input_rows.push(row);
        }

        let want_returning = !self.returning.is_empty();
        let extra_from = self.returning_extra_from.map(|v| v as usize);
        let mut kept: Vec<Row> = Vec::new();
        let mut count: u64 = 0;
        let mut seen: HashSet<u64> = HashSet::new();
        let mut conflict = ConflictState::default();

        match self.op {
            ModifyOp::Insert => {
                let spec = self
                    .insert
                    .as_ref()
                    .ok_or_else(|| PgError::internal("INSERT ModifyTable without an insert spec"))?;
                for input_row in &input_rows {
                    ex.tick(&mut ticks)?;
                    if let Some(inserted) =
                        dml_insert::insert_input_row(ex, &td, spec, input_row, &mut conflict)?
                    {
                        count += 1;
                        if want_returning {
                            kept.push(returning_input(inserted.row, input_row, extra_from));
                        }
                    }
                }
            }
            ModifyOp::Update => {
                let spec = self
                    .update
                    .as_ref()
                    .ok_or_else(|| PgError::internal("UPDATE ModifyTable without an update spec"))?;
                for input_row in &input_rows {
                    ex.tick(&mut ticks)?;
                    let rowid = match rowid_of(input_row, self.rowid_slot)? {
                        Some(rowid) => rowid,
                        None => continue,
                    };
                    // First match wins: a target row matched by several input rows is updated once.
                    if !seen.insert(rowid) {
                        continue;
                    }
                    if let Some(new_row) =
                        dml_update::update_one(ex, &td, &self.target, spec, &self.policy, input_row, rowid)?
                    {
                        count += 1;
                        if want_returning {
                            kept.push(returning_input(new_row, input_row, extra_from));
                        }
                    }
                }
            }
            ModifyOp::Delete => {
                for input_row in &input_rows {
                    ex.tick(&mut ticks)?;
                    let rowid = match rowid_of(input_row, self.rowid_slot)? {
                        Some(rowid) => rowid,
                        None => continue,
                    };
                    if !seen.insert(rowid) {
                        continue;
                    }
                    if let Some(deleted) = dml_delete::delete_one(ex, &td, &self.policy, input_row, rowid)? {
                        count += 1;
                        if want_returning {
                            kept.push(returning_input(deleted, input_row, extra_from));
                        }
                    }
                }
            }
        }

        // End-of-statement foreign-key checks run before any RETURNING row is released.
        fk::run_pending(ex)?;
        for row in &kept {
            ex.tick(&mut ticks)?;
            let out = eval::eval_exprs(ex, &self.returning, row)?;
            self.queue.push_back(out);
        }
        self.affected = count;
        Ok(())
    }
}

impl RowSource for ModifyTableOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if !self.started {
            self.started = true;
            // A failing statement must not leave its deferred FK checks behind.
            let mark = ex.cx.stmt.pending_fk.len();
            if let Err(e) = self.run(ex) {
                ex.cx.stmt.pending_fk.truncate(mark);
                return Err(e);
            }
        }
        Ok(self.queue.pop_front())
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        if self.started {
            return Err(PgError::internal("ModifyTable cannot be rescanned after it has run"));
        }
        Ok(())
    }

    fn affected_rows(&self) -> Option<u64> {
        Some(self.affected)
    }
}

/// Target rowid of an input row: `Ok(None)` for a NULL rowid slot (no target row).
fn rowid_of(input: &[Datum], slot: Option<u32>) -> Result<Option<u64>> {
    let slot = match slot {
        Some(s) => s as usize,
        None => return Err(PgError::internal("ModifyTable: missing rowid slot")),
    };
    match input.get(slot) {
        Some(Datum::Int8(v)) => Ok(Some(*v as u64)),
        Some(Datum::Null) => Ok(None),
        Some(_) => Err(PgError::internal("ModifyTable: rowid slot is not int8")),
        None => Err(PgError::internal("ModifyTable: rowid slot out of range")),
    }
}

/// RETURNING input row: the stored image followed by the input's extra FROM/USING columns.
fn returning_input(mut image: Row, input: &[Datum], extra_from: Option<usize>) -> Row {
    if let Some(from) = extra_from {
        if let Some(extra) = input.get(from..) {
            image.extend_from_slice(extra);
        }
    }
    image
}

/// Derive the EvalPlanQual policy from the ModifyTable input subtree (module docs).
pub fn epq_policy(input: &PlanNode) -> Result<EpqPolicy> {
    // Filters encountered walking down from the top; applied bottom-up below.
    let mut filters: Vec<&BoundExpr> = Vec::new();
    let mut cur = input;
    loop {
        match &cur.kind {
            NodeKind::Filter(f) => {
                filters.push(&f.predicate);
                cur = &f.input;
            }
            NodeKind::SeqScan(s) => {
                if !s.with_rowid {
                    return Ok(EpqPolicy { quals: None });
                }
                let mut quals: Vec<BoundExpr> = Vec::new();
                if let Some(q) = &s.filter {
                    quals.push(q.clone());
                }
                return Ok(EpqPolicy { quals: Some(finish_quals(quals, &filters)) });
            }
            NodeKind::IndexScan(s) => {
                if !s.with_rowid {
                    return Ok(EpqPolicy { quals: None });
                }
                let mut quals: Vec<BoundExpr> = s.index_cond.clone();
                if let Some(q) = &s.filter {
                    quals.push(q.clone());
                }
                return Ok(EpqPolicy { quals: Some(finish_quals(quals, &filters)) });
            }
            _ => return Ok(EpqPolicy { quals: None }),
        }
    }
}

/// Append the filter predicates (collected top-down) to the scan quals, innermost first.
fn finish_quals(mut quals: Vec<BoundExpr>, filters_top_down: &[&BoundExpr]) -> Vec<BoundExpr> {
    for pred in filters_top_down.iter().rev() {
        quals.push((*pred).clone());
    }
    quals
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_plan::plan::{FilterNode, OutCol, ResultNode, SeqScan, TableRef};
    use pgarbor_sql::ir::ExprKind;
    use pgarbor_types::TypeId;

    fn bool_expr(v: bool) -> BoundExpr {
        BoundExpr { kind: ExprKind::Const(Datum::Bool(v)), ty: TypeId::of(16) }
    }

    fn seq_scan(filter: Option<BoundExpr>, with_rowid: bool) -> PlanNode {
        let table = TableRef {
            oid: 16400,
            schema: "public".to_string(),
            name: "t".to_string(),
            alias: "t".to_string(),
            ncols: 1,
            col_types: vec![TypeId::of(23)],
        };
        let out = vec![OutCol { name: "a".to_string(), ty: TypeId::of(23) }];
        PlanNode::new(NodeKind::SeqScan(SeqScan { table, filter, with_rowid, needed: vec![true] }), out)
    }

    fn filter_over(input: PlanNode, pred: BoundExpr) -> PlanNode {
        let out = input.out.clone();
        PlanNode::new(NodeKind::Filter(Box::new(FilterNode { input, predicate: pred })), out)
    }

    #[test]
    fn epq_plain_seq_scan_has_no_quals() {
        let p = epq_policy(&seq_scan(None, true)).unwrap();
        assert_eq!(p.quals.as_ref().map(|q| q.len()), Some(0));
    }

    #[test]
    fn epq_filter_over_seq_scan_collects_scan_then_filter_quals() {
        let plan = filter_over(seq_scan(Some(bool_expr(true)), true), bool_expr(false));
        let p = epq_policy(&plan).unwrap();
        let quals = p.quals.expect("simple shape must yield quals");
        assert_eq!(quals.len(), 2);
        assert!(matches!(quals[0].kind, ExprKind::Const(Datum::Bool(true))));
        assert!(matches!(quals[1].kind, ExprKind::Const(Datum::Bool(false))));
    }

    #[test]
    fn epq_join_like_input_has_no_quals() {
        let res = PlanNode::new(NodeKind::Result(ResultNode { exprs: vec![], one_time_filter: None }), vec![]);
        assert!(epq_policy(&res).unwrap().quals.is_none());
    }

    #[test]
    fn epq_scan_without_rowid_has_no_quals() {
        assert!(epq_policy(&seq_scan(None, false)).unwrap().quals.is_none());
    }

    #[test]
    fn rowid_of_reads_int8_and_null() {
        let row = vec![Datum::Int4(1), Datum::Int8(-1), Datum::Null];
        assert_eq!(rowid_of(&row, Some(1)).unwrap(), Some(u64::MAX));
        assert_eq!(rowid_of(&row, Some(2)).unwrap(), None);
        assert!(rowid_of(&row, Some(0)).is_err());
        assert!(rowid_of(&row, Some(9)).is_err());
        assert!(rowid_of(&row, None).is_err());
    }

    #[test]
    fn returning_input_appends_extra_columns() {
        let input = vec![Datum::Int4(1), Datum::Int4(2), Datum::Int4(3)];
        let image = vec![Datum::Int4(9)];
        let out = returning_input(image.clone(), &input, Some(1));
        assert_eq!(out, vec![Datum::Int4(9), Datum::Int4(2), Datum::Int4(3)]);
        assert_eq!(returning_input(image.clone(), &input, None), image);
        assert_eq!(returning_input(image.clone(), &input, Some(7)), image);
    }
}
