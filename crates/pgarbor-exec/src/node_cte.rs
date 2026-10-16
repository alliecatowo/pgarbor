//! CTE execution (lease EXE-15, sonnet): `CteScan`, `WorkTableScan`, the CTE runtime and eager
//! data-modifying CTEs. PG: `nodeCtescan.c`, `nodeRecursiveunion.c`, `nodeWorktablescan.c`,
//! `ExecPostprocessPlan` (data-modifying CTEs run to completion).
//!
//! ## Runtime state (`runtime::CteRt`)
//! One `CteRt` per `Plan::ctes[id]`, built by [`build_cte_rt`]: the body operators are built with
//! `b.child(..)` (pre-order numbering continues after sub-plans), `state = Pending`, `rows` empty,
//! `worktable` empty.
//!
//! ## `ensure_cte(ex, id)` (lazy evaluation)
//! * `Done`: nothing. `Running` (a non-recursive CTE referencing itself cannot happen): XX000.
//! * `CteBodyRt::Query(op)` (WITH RECURSIVE; PG semantics):
//!   take the operator out (see runtime module docs), set `Running`, drain it
//!   into a `Vec<Row>`, store `rows = Rc::new(..)`, `Done`.
//! * `CteBodyRt::Recursive{non_recursive, recursive, union_all}` (WITH RECURSIVE; PG semantics):
//!   `result = []`; run the non-recursive term fully: its rows (de-duplicated against everything so far
//!   when `!union_all`, using `keycodec` keys of ALL columns with NULLs equal) become both `result`
//!   and the first working table. Loop: while the working table is non-empty: set
//!   `worktable = previous iteration's rows`, `recursive.rescan(ex, &[])`, drain it into `new_rows`
//!   (de-duplicate against `result` and itself when `!union_all`), append to `result`,
//!   `worktable = new_rows`. Check `ex.check_interrupts()` per iteration and per 1024 rows. Finally
//!   `rows = result`.
//! * `CteBodyRt::Modify(op)`: data-modifying CTE; normally executed by [`run_modify_ctes`]; if reached
//!   here `Pending` it is executed the same way.
//!
//! ## [`run_modify_ctes`]
//! Called once by `build::open_plan`: for every CTE whose body is `Modify`, run `ensure_cte`
//! (executes the INSERT/UPDATE/DELETE ... RETURNING to completion and buffers its RETURNING rows),
//! in `id` order, BEFORE the main query starts.
//!
//! ## CteScan / WorkTableScan operators
//! * `CteScan{cte_id, filter}`: on first `next` after (re)start call `ensure_cte`, then iterate a
//!   shared `Rc<Vec<Row>>` clone of `rows` (`rescan` restarts the position only). Output row = CTE
//!   columns; `filter` over it.
//! * `WorkTableScan{cte_id, filter}`: yields the rows currently in `ctes[cte_id].worktable` (a snapshot
//!   cloned at (re)scan time); `rescan` re-reads the work table. Output row = CTE columns.
use std::cell::RefCell;
use std::rc::Rc;

use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::{CteBodyPlan, CteDef, CteScan, OutCol, Plan, PlanNode, WorkTableScan};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::{Datum, TypeId};

use crate::build::BuildCtx;
use crate::eval::eval_opt_qual;
use crate::runtime::{CteBodyRt, CteRt, CteState, Executor, RecStream, Row, RowSource};
use crate::sortutil::row_key_bytes;

/// `CteScan` operator (module docs).
struct CteScanOp {
    cte_id: u32,
    filter: Option<BoundExpr>,
    pos: usize,
    skipped: u32,
}

impl RowSource for CteScanOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        let idx = cte_index(ex, self.cte_id)?;
        loop {
            let row = loop {
                if let Some(r) = ex.rt.ctes[idx].rows.get(self.pos) {
                    break r.clone();
                }
                match ex.rt.ctes[idx].state {
                    CteState::Done => return Ok(None),
                    CteState::Streaming => advance_recursive(ex, idx)?,
                    _ if matches!(ex.rt.ctes[idx].body, CteBodyRt::Recursive { .. }) => advance_recursive(ex, idx)?,
                    _ => ensure_cte(ex, self.cte_id)?,
                }
            };
            self.pos += 1;
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
            ex.tick(&mut self.skipped)?;
        }
    }

    fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.pos = 0;
        Ok(())
    }
}

/// `WorkTableScan` operator (module docs).
struct WorkTableScanOp {
    cte_id: u32,
    filter: Option<BoundExpr>,
    /// Snapshot of the work table, taken at (re)scan time.
    snap: Option<Vec<Row>>,
    pos: usize,
    skipped: u32,
}

impl RowSource for WorkTableScanOp {
    fn next(&mut self, ex: &mut Executor) -> Result<Option<Row>> {
        if self.snap.is_none() {
            self.snap = Some(snapshot_worktable(ex, self.cte_id)?);
        }
        loop {
            let row = match self.snap.as_ref().and_then(|r| r.get(self.pos)) {
                Some(r) => r.clone(),
                None => return Ok(None),
            };
            self.pos += 1;
            if eval_opt_qual(ex, &self.filter, &row)? {
                return Ok(Some(row));
            }
            ex.tick(&mut self.skipped)?;
        }
    }

    fn rescan(&mut self, ex: &mut Executor, _params: &[Datum]) -> Result<()> {
        self.snap = Some(snapshot_worktable(ex, self.cte_id)?);
        self.pos = 0;
        Ok(())
    }
}

/// Index of CTE `id` in `Runtime::ctes` (normally `id` itself).
fn cte_index(ex: &Executor, id: u32) -> Result<usize> {
    if let Some(c) = ex.rt.ctes.get(id as usize) {
        if c.id == id {
            return Ok(id as usize);
        }
    }
    ex.rt
        .ctes
        .iter()
        .position(|c| c.id == id)
        .ok_or_else(|| PgError::internal(format!("unknown CTE id {id}")))
}

/// Copy of the current work table of CTE `id`.
fn snapshot_worktable(ex: &Executor, id: u32) -> Result<Vec<Row>> {
    let idx = cte_index(ex, id)?;
    let guard = ex.rt.ctes[idx].worktable.borrow();
    Ok((*guard).clone())
}

/// Definition of CTE `id` in the plan.
fn def_for(plan: &Plan, id: u32) -> Result<&CteDef> {
    plan.ctes
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| PgError::internal(format!("unknown CTE id {id} in plan")))
}

/// The scan node must produce exactly the CTE's columns.
fn check_width(node: &PlanNode, def: &CteDef) -> Result<()> {
    if node.width() != def.columns.len() {
        return Err(PgError::internal(format!(
            "scan of CTE \"{}\" produces {} columns but the CTE has {}",
            def.name,
            node.width(),
            def.columns.len()
        )));
    }
    Ok(())
}

pub fn build_cte_scan(node: &PlanNode, n: &CteScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let def = def_for(b.plan, n.cte_id)?;
    check_width(node, def)?;
    Ok(Box::new(CteScanOp { cte_id: n.cte_id, filter: n.filter.clone(), pos: 0, skipped: 0 }))
}

pub fn build_worktable_scan(node: &PlanNode, n: &WorkTableScan, b: &mut BuildCtx) -> Result<Box<dyn RowSource>> {
    let def = def_for(b.plan, n.cte_id)?;
    if !matches!(def.body, CteBodyPlan::Recursive { .. }) {
        return Err(PgError::internal(format!(
            "work table scan of CTE \"{}\" outside a recursive CTE",
            def.name
        )));
    }
    check_width(node, def)?;
    Ok(Box::new(WorkTableScanOp { cte_id: n.cte_id, filter: n.filter.clone(), snap: None, pos: 0, skipped: 0 }))
}

/// Runtime form of one CTE definition (module docs).
pub fn build_cte_rt(def: &CteDef, b: &mut BuildCtx) -> Result<CteRt> {
    let body = match &def.body {
        CteBodyPlan::Query(node) => CteBodyRt::Query(b.child(node)?),
        CteBodyPlan::Recursive { non_recursive, recursive, union_all } => {
            let nr = b.child(non_recursive)?;
            let rec = b.child(recursive)?;
            CteBodyRt::Recursive { non_recursive: nr, recursive: rec, union_all: *union_all }
        }
        CteBodyPlan::Modify(node) => CteBodyRt::Modify(b.child(node)?),
    };
    Ok(CteRt {
        id: def.id,
        name: def.name.clone(),
        columns: def.columns.clone(),
        materialize: def.materialize,
        body,
        state: CteState::Pending,
        rows: Rc::new(Vec::new()),
        worktable: Rc::new(RefCell::new(Vec::new())),
        stream: RecStream::default(),
    })
}

/// Drain an operator to exhaustion, polling interrupts every 1024 rows.
fn drain_rows(ex: &mut Executor, op: &mut dyn RowSource) -> Result<Vec<Row>> {
    let mut out: Vec<Row> = Vec::new();
    let mut ticks: u32 = 0;
    while let Some(row) = op.next(ex)? {
        out.push(row);
        ex.tick(&mut ticks)?;
    }
    Ok(out)
}

/// `true` when `row` is new for the CTE (and remembers it). Always `true` for `UNION ALL`.
fn admit(ex: &Executor, stream: &mut RecStream, columns: &[OutCol], union_all: bool, row: &[Datum]) -> Result<bool> {
    if union_all {
        return Ok(true);
    }
    let types: Vec<TypeId> = columns.iter().map(|c| c.ty).collect();
    let slots: Vec<usize> = (0..columns.len()).collect();
    let key = row_key_bytes(ex, &types, &slots, row)?;
    Ok(stream.seen.insert(key))
}

/// Run one step of a recursive CTE: the non-recursive term first, then one iteration of the
/// recursive term per call, appending the new rows to `rows`. When the work table runs dry the CTE
/// is `Done`. Consumers call this only when they have read every buffered row, so a `LIMIT` above
/// an infinite recursion terminates (PG produces the rows lazily as well).
fn advance_recursive(ex: &mut Executor, idx: usize) -> Result<()> {
    let mut body = std::mem::replace(&mut ex.rt.ctes[idx].body, CteBodyRt::Taken);
    let prev = ex.rt.ctes[idx].state;
    ex.rt.ctes[idx].state = CteState::Running;
    let result = match &mut body {
        CteBodyRt::Recursive { non_recursive, recursive, union_all } => {
            step_recursive(ex, idx, &mut **non_recursive, &mut **recursive, *union_all)
        }
        _ => Err(PgError::internal("advance of a CTE that is not recursive")),
    };
    ex.rt.ctes[idx].body = body;
    match result {
        Ok(true) => {
            ex.rt.ctes[idx].state = CteState::Done;
            ex.rt.ctes[idx].worktable.borrow_mut().clear();
            ex.rt.ctes[idx].stream = RecStream::default();
            Ok(())
        }
        Ok(false) => {
            ex.rt.ctes[idx].state = CteState::Streaming;
            Ok(())
        }
        Err(e) => {
            ex.rt.ctes[idx].state = prev;
            Err(e)
        }
    }
}

/// One step; returns `true` when the recursion is finished.
fn step_recursive(
    ex: &mut Executor,
    idx: usize,
    non_recursive: &mut dyn RowSource,
    recursive: &mut dyn RowSource,
    union_all: bool,
) -> Result<bool> {
    let columns = ex.rt.ctes[idx].columns.clone();
    let mut stream = std::mem::take(&mut ex.rt.ctes[idx].stream);
    let result = (|| -> Result<bool> {
        let mut ticks = 0;
        loop {
            ex.tick(&mut ticks)?;
            let row = if !stream.started {
                non_recursive.next(ex)?
            } else {
                recursive.next(ex)?
            };
            if let Some(row) = row {
                if admit(ex, &mut stream, &columns, union_all, &row)? {
                    stream.working.push(row.clone());
                    Rc::make_mut(&mut ex.rt.ctes[idx].rows).push(row);
                    return Ok(false);
                }
                continue;
            }
            if stream.working.is_empty() { return Ok(true); }
            *ex.rt.ctes[idx].worktable.borrow_mut() = std::mem::take(&mut stream.working);
            // A new working table invalidates cached hash/materialize/sort inputs, just like
            // changing a nested-loop parameter. Reusing the old hash table can recurse forever.
            ex.rt.epoch = ex.rt.epoch.wrapping_add(1);
            recursive.rescan(ex, &[])?;
            stream.started = true;
        }
    })();
    ex.rt.ctes[idx].stream = stream;
    result
}

/// Run one CTE body to completion (the body is owned by the caller while it runs).
fn run_body(ex: &mut Executor, _idx: usize, body: &mut CteBodyRt) -> Result<Vec<Row>> {
    match body {
        CteBodyRt::Query(op) | CteBodyRt::Modify(op) => drain_rows(ex, &mut **op),
        CteBodyRt::Recursive { .. } => Err(PgError::internal("recursive CTE bodies are advanced step by step")),
        CteBodyRt::Taken => Err(PgError::internal("CTE body is already being evaluated")),
    }
}

/// Execute CTE `id` if still pending (module docs).
pub fn ensure_cte(ex: &mut Executor, id: u32) -> Result<()> {
    let idx = cte_index(ex, id)?;
    if matches!(ex.rt.ctes[idx].body, CteBodyRt::Recursive { .. }) && ex.rt.ctes[idx].state != CteState::Running {
        while ex.rt.ctes[idx].state != CteState::Done {
            advance_recursive(ex, idx)?;
        }
        return Ok(());
    }
    match ex.rt.ctes[idx].state {
        CteState::Done => return Ok(()),
        CteState::Streaming => return Err(PgError::internal("CTE is streaming")),
        CteState::Running => {
            return Err(PgError::internal(format!(
                "CTE \"{}\" is referenced while it is being evaluated",
                ex.rt.ctes[idx].name
            )))
        }
        CteState::Pending => {}
    }
    // Take the body out so the operators can run while the CTE runtime is borrowed by `ex`.
    let mut body = std::mem::replace(&mut ex.rt.ctes[idx].body, CteBodyRt::Taken);
    ex.rt.ctes[idx].state = CteState::Running;
    let result = run_body(ex, idx, &mut body);
    ex.rt.ctes[idx].body = body;
    match result {
        Ok(rows) => {
            ex.rt.ctes[idx].rows = Rc::new(rows);
            ex.rt.ctes[idx].state = CteState::Done;
            Ok(())
        }
        Err(e) => {
            ex.rt.ctes[idx].state = CteState::Pending;
            Err(e)
        }
    }
}

/// Run all data-modifying CTEs to completion (module docs).
pub fn run_modify_ctes(ex: &mut Executor) -> Result<()> {
    let mut ids: Vec<u32> = ex
        .rt
        .ctes
        .iter()
        .filter(|c| matches!(c.body, CteBodyRt::Modify(_)))
        .map(|c| c.id)
        .collect();
    ids.sort_unstable();
    for id in ids {
        ensure_cte(ex, id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_types::context::IoConfig;

    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    /// Source yielding fixed rows; counts `next` calls.
    struct Fixed {
        rows: Vec<Row>,
        pos: usize,
        calls: Rc<Cell<u32>>,
    }

    impl RowSource for Fixed {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            self.calls.set(self.calls.get() + 1);
            let r = self.rows.get(self.pos).cloned();
            self.pos += 1;
            Ok(r)
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    /// Recursive term: for each working-table row `n < limit` yields `n + 1`.
    struct Succ {
        wt: Rc<RefCell<Vec<Row>>>,
        limit: i32,
        out: Vec<Row>,
        pos: usize,
    }

    impl RowSource for Succ {
        fn next(&mut self, _ex: &mut Executor) -> Result<Option<Row>> {
            let r = self.out.get(self.pos).cloned();
            self.pos += 1;
            Ok(r)
        }
        fn rescan(&mut self, _ex: &mut Executor, _params: &[Datum]) -> Result<()> {
            let limit = self.limit;
            let next: Vec<Row> = self
                .wt
                .borrow()
                .iter()
                .filter_map(|r| match r.first() {
                    Some(Datum::Int4(n)) if *n < limit => Some(vec![Datum::Int4(*n + 1)]),
                    _ => None,
                })
                .collect();
            self.out = next;
            self.pos = 0;
            Ok(())
        }
    }

    fn int_cte(id: u32, body: CteBodyRt, wt: Rc<RefCell<Vec<Row>>>) -> CteRt {
        CteRt {
            id,
            name: format!("c{id}"),
            columns: vec![OutCol { name: "n".into(), ty: TypeId::INT4 }],
            materialize: true,
            body,
            state: CteState::Pending,
            rows: Rc::new(Vec::new()),
            worktable: wt,
            stream: RecStream::default(),
        }
    }

    /// Build a session around `ctes`, run `steps`, and hand the CTE runtimes back.
    fn exec_ctes(ctes: Vec<CteRt>, steps: impl FnOnce(&mut Executor) -> Result<()>) -> Result<Vec<CteRt>> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let g = NoGuc;
        let s = NoServices;
        let cell = Cell::new(12345);
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        rt.ctes = ctes;
        let mut ex = Executor::new(&mut rt, &mut cx);
        steps(&mut ex)?;
        drop(ex);
        Ok(rt.ctes)
    }

    fn int_rows(rows: &Rc<Vec<Row>>) -> Vec<i32> {
        rows.iter()
            .map(|r| match r.first() {
                Some(Datum::Int4(n)) => *n,
                _ => -1,
            })
            .collect()
    }

    #[test]
    fn recursive_union_all_yields_1_to_5() -> Result<()> {
        let wt = Rc::new(RefCell::new(Vec::new()));
        let calls = Rc::new(Cell::new(0));
        let nonrec = Fixed { rows: vec![vec![Datum::Int4(1)]], pos: 0, calls };
        let rec = Succ { wt: Rc::clone(&wt), limit: 5, out: Vec::new(), pos: 0 };
        let body = CteBodyRt::Recursive { non_recursive: Box::new(nonrec), recursive: Box::new(rec), union_all: true };
        let ctes = exec_ctes(vec![int_cte(0, body, wt)], |ex| ensure_cte(ex, 0))?;
        assert_eq!(ctes[0].state, CteState::Done);
        assert_eq!(int_rows(&ctes[0].rows), vec![1, 2, 3, 4, 5]);
        Ok(())
    }

    #[test]
    fn shared_cte_is_executed_once() -> Result<()> {
        let calls = Rc::new(Cell::new(0));
        let q = Fixed { rows: vec![vec![Datum::Int4(7)]], pos: 0, calls: Rc::clone(&calls) };
        let wt = Rc::new(RefCell::new(Vec::new()));
        let ctes = exec_ctes(vec![int_cte(0, CteBodyRt::Query(Box::new(q)), wt)], |ex| {
            ensure_cte(ex, 0)?;
            ensure_cte(ex, 0)
        })?;
        // One row plus the terminating `None`, drained exactly once.
        assert_eq!(calls.get(), 2);
        assert_eq!(int_rows(&ctes[0].rows), vec![7]);
        Ok(())
    }

    #[test]
    fn unreferenced_modify_cte_runs_eagerly() -> Result<()> {
        let calls = Rc::new(Cell::new(0));
        let m = Fixed { rows: Vec::new(), pos: 0, calls: Rc::clone(&calls) };
        let wt = Rc::new(RefCell::new(Vec::new()));
        let ctes = exec_ctes(vec![int_cte(0, CteBodyRt::Modify(Box::new(m)), wt)], run_modify_ctes)?;
        assert!(calls.get() >= 1);
        assert_eq!(ctes[0].state, CteState::Done);
        Ok(())
    }

    #[test]
    fn cte_scan_replays_after_rescan() -> Result<()> {
        let calls = Rc::new(Cell::new(0));
        let q = Fixed { rows: vec![vec![Datum::Int4(1)], vec![Datum::Int4(2)]], pos: 0, calls };
        let wt = Rc::new(RefCell::new(Vec::new()));
        exec_ctes(vec![int_cte(0, CteBodyRt::Query(Box::new(q)), wt)], |ex| {
            let mut op = CteScanOp { cte_id: 0, filter: None, pos: 0, skipped: 0 };
            let first = drain_rows(ex, &mut op)?;
            assert_eq!(first.len(), 2);
            op.rescan(ex, &[])?;
            let again = drain_rows(ex, &mut op)?;
            assert_eq!(again.len(), 2);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    #[ignore = "needs EXE-22"]
    fn union_recursion_deduplicates() -> Result<()> {
        let calls = Rc::new(Cell::new(0));
        let nonrec = Fixed {
            rows: vec![vec![Datum::Int4(1)], vec![Datum::Int4(1)], vec![Datum::Int4(2)]],
            pos: 0,
            calls,
        };
        let rec = Fixed { rows: Vec::new(), pos: 0, calls: Rc::new(Cell::new(0)) };
        let wt = Rc::new(RefCell::new(Vec::new()));
        let body = CteBodyRt::Recursive { non_recursive: Box::new(nonrec), recursive: Box::new(rec), union_all: false };
        let ctes = exec_ctes(vec![int_cte(0, body, wt)], |ex| ensure_cte(ex, 0))?;
        assert_eq!(int_rows(&ctes[0].rows), vec![1, 2]);
        Ok(())
    }
}
