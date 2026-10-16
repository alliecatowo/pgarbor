//! Row-level constraint enforcement (lease EXE-31). PG: `executor/execMain.c` (`ExecConstraints`,
//! `ExecBuildSlotValueDescription`). UNIQUE/PRIMARY KEY are enforced by `index_maint`, FOREIGN KEY by
//! `fk`, domain constraints inside expressions (`eval_cast::eval_coerce_to_domain`).
//!
//! * [`check_not_null`]: for every non-dropped column with `not_null` whose value in the NEW row is
//!   NULL (columns in attnum order, FIRST violation wins) -> `error::not_null_violation(schema, table,
//!   column, error::row_description(..))` (23502). Message and DETAIL fixtures are in `error`:
//!   `null value in column "name" of relation "t" violates not-null constraint` / `Failing row
//!   contains (2, null, 1.50, y, null).`
//! * [`check_constraints`]: evaluate each `td.checks` expression over the full new row
//!   (`eval::eval_bool3`); FALSE -> `error::check_violation(schema, table, constraint_name,
//!   row_description)` (23514); TRUE or NULL pass. Constraints are checked in NAME order (PG:
//!   alphabetical by constraint name) after NOT NULL.
//! * [`enforce`]: `check_not_null` then `check_constraints` - the order PG uses for INSERT and UPDATE.
//! Row descriptions use `td.desc_cols()` and the session `IoCtx` (`cx.se.io_ctx()`).
//!
//! PG reference: `src/backend/executor/execMain.c` (`ExecConstraints`, `ExecRelCheck`) and
//! `src/backend/executor/execMain.c` (`ExecBuildSlotValueDescription`). The check order is the
//! `CheckConstraintCmp` order (plain `strcmp` on the constraint name), which is byte order on `str`.
use pgarbor_common::Result;
use pgarbor_types::Datum;

use crate::error::{check_violation, not_null_violation, row_description};
use crate::eval::{eval, eval_bool3};
use pgarbor_catalog::model::GeneratedKind;
use crate::runtime::Executor;
use crate::tabledesc::{CheckDesc, ColDesc, TableDesc};

/// The first (attnum order) live NOT NULL column whose value in `row` is NULL. A row shorter than
/// the column list counts the missing slots as NULL.
fn first_null_violation<'t>(td: &'t TableDesc, row: &[Datum]) -> Option<&'t ColDesc> {
    for (i, c) in td.cols.iter().enumerate() {
        if c.dropped || !c.not_null || c.generated == Some(GeneratedKind::Virtual) {
            continue;
        }
        let is_null = match row.get(i) {
            Some(d) => d.is_null(),
            None => true,
        };
        if is_null {
            return Some(c);
        }
    }
    None
}

/// The table's CHECK constraints sorted by constraint name (PG `CheckConstraintCmp`, `strcmp`).
fn checks_in_name_order(td: &TableDesc) -> Vec<&CheckDesc> {
    let mut v: Vec<&CheckDesc> = td.checks.iter().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// NOT NULL enforcement for the NEW row (module docs). 23502 on the first NULL in a NOT NULL column.
pub fn check_not_null(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    let mut col = first_null_violation(td, row);
    // NOT NULL on a VIRTUAL generated column applies to its computed value; an earlier attnum wins.
    for (slot, expr) in &td.virtual_not_null {
        if col.map(|c| c.attnum as usize - 1 < *slot).unwrap_or(false) {
            break;
        }
        if eval(ex, expr, row)?.is_null() {
            col = td.cols.get(*slot);
            break;
        }
    }
    let col = match col {
        Some(c) => c,
        None => return Ok(()),
    };
    let desc = row_description(&ex.cx.se.io_ctx(), &td.desc_cols(), row)?;
    Err(not_null_violation(&td.schema, &td.name, &col.name, &desc))
}

/// CHECK enforcement for the NEW row (module docs). A constraint whose expression is FALSE raises
/// 23514; TRUE and NULL pass. Constraints run in name order.
pub fn check_constraints(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    if td.checks.is_empty() {
        return Ok(());
    }
    for c in checks_in_name_order(td) {
        let v = eval_bool3(ex, &c.expr, row)?;
        if v == Some(false) {
            let desc = row_description(&ex.cx.se.io_ctx(), &td.desc_cols(), row)?;
            return Err(check_violation(&td.schema, &td.name, &c.name, &desc));
        }
    }
    Ok(())
}

/// NOT NULL then CHECK, the order PG uses for INSERT and UPDATE (module docs). Row-security WITH
/// CHECKs of the statement's target run first and view check options last (PG `ExecInsert`/`ExecUpdate`
/// order: `WCO_RLS_*` before `ExecConstraints`, `WCO_VIEW_CHECK` after the row is stored).
pub fn enforce(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    let wco = statement_wco(ex, td);
    check_wco(ex, td, row, &wco, false)?;
    check_not_null(ex, td, row)?;
    check_constraints(ex, td, row)?;
    check_wco(ex, td, row, &wco, true)
}

fn statement_wco(ex: &Executor, td: &TableDesc) -> Vec<pgarbor_sql::ir::TargetWco> {
    ex.cx.stmt.wco.iter().rev().find(|(oid, _)| *oid == td.oid).map(|(_, w)| w.clone()).unwrap_or_default()
}

/// Row-security WITH CHECKs of the statement's target for a proposed row: PG checks them before the
/// speculative insertion of `INSERT ... ON CONFLICT`, so a conflicting row is still rejected.
pub fn check_rls_wco(ex: &mut Executor, td: &TableDesc, row: &[Datum]) -> Result<()> {
    let wco = statement_wco(ex, td);
    check_wco(ex, td, row, &wco, false)
}

/// Evaluate the WITH CHECK OPTIONs of one phase (`views`: view check options, else row security).
fn check_wco(ex: &mut Executor, td: &TableDesc, row: &[Datum], wco: &[pgarbor_sql::ir::TargetWco], views: bool) -> Result<()> {
    use pgarbor_sql::ir::WcoKind;
    for w in wco {
        let is_view = w.kind == WcoKind::ViewCheck;
        if is_view != views {
            continue;
        }
        // PG ExecWithCheckOptions: the qual must be TRUE (NULL fails).
        if eval_bool3(ex, &w.expr, row)? == Some(true) {
            continue;
        }
        if is_view {
            let desc = row_description(&ex.cx.se.io_ctx(), &td.desc_cols(), row)?;
            return Err(pgarbor_common::PgError::new(
                pgarbor_common::SqlState(*b"44000"),
                format!("new row violates check option for view \"{}\"", w.relname),
            )
            .with_detail(format!("Failing row contains {desc}.")));
        }
        let msg = match &w.polname {
            Some(p) => format!("new row violates row-level security policy \"{p}\" for table \"{}\"", w.relname),
            None => format!("new row violates row-level security policy for table \"{}\"", w.relname),
        };
        return Err(pgarbor_common::PgError::new(pgarbor_common::SqlState::INSUFFICIENT_PRIVILEGE, msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_catalog::model::RelKind;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;

    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;

    fn col(attnum: i16, name: &str, not_null: bool, dropped: bool) -> ColDesc {
        ColDesc {
            attnum,
            name: name.to_string(),
            ty: if name == "name" { TypeId::TEXT } else { TypeId::INT4 },
            not_null,
            dropped,
            generated: None,
            identity: None,
            missing: None,
        }
    }

    fn table(cols: Vec<ColDesc>, checks: Vec<CheckDesc>) -> TableDesc {
        TableDesc {
            oid: 16400,
            schema: "public".to_string(),
            name: "t".to_string(),
            kind: RelKind::Table,
            cols,
            indexes: Vec::new(),
            checks,
            virtual_not_null: Vec::new(),
            fks_out: Vec::new(),
            fks_in: Vec::new(),
        }
    }

    #[test]
    fn first_null_violation_skips_dropped_and_nullable_columns() {
        let td = table(
            vec![
                col(1, "id", true, false),
                col(2, "gone", true, true),
                col(3, "name", true, false),
                col(4, "note", false, false),
            ],
            Vec::new(),
        );
        // dropped column 2 is NULL but ignored; column 4 is nullable
        let row = vec![Datum::Int4(1), Datum::Null, Datum::Text("x".into()), Datum::Null];
        assert!(first_null_violation(&td, &row).is_none());
        // NULL in the first NOT NULL column wins over later ones
        let row = vec![Datum::Null, Datum::Null, Datum::Null, Datum::Null];
        let c = first_null_violation(&td, &row).map(|c| c.name.clone());
        assert_eq!(c.as_deref(), Some("id"));
        // a short row treats the missing slot as NULL
        let row = vec![Datum::Int4(1)];
        let c = first_null_violation(&td, &row).map(|c| c.name.clone());
        assert_eq!(c.as_deref(), Some("name"));
    }

    #[test]
    fn checks_are_sorted_by_name() {
        let mk = |name: &str| CheckDesc {
            oid: 1,
            name: name.to_string(),
            expr: dummy_expr(),
            validated: true,
        };
        let td = table(vec![col(1, "id", false, false)], vec![mk("t_v_check"), mk("t_a_check"), mk("t_b_check")]);
        let names: Vec<&str> = checks_in_name_order(&td).iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["t_a_check", "t_b_check", "t_v_check"]);
    }

    #[test]
    fn not_null_error_text_and_detail() {
        let cell = Cell::new(1);
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped().unwrap();
        let g = NoGuc;
        let s = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &g, &s, &cell, 0, 0);
        let mut txn = MemTxn::new();
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        let td = table(vec![col(1, "id", true, false), col(2, "name", true, false)], Vec::new());

        assert!(check_not_null(&mut ex, &td, &[Datum::Int4(1), Datum::Text("a".into())]).is_ok());
        let Err(err) = check_not_null(&mut ex, &td, &[Datum::Int4(2), Datum::Null]) else {
            panic!("expected a NOT NULL violation");
        };
        assert_eq!(err.sqlstate.code(), "23502");
        assert_eq!(err.message, "null value in column \"name\" of relation \"t\" violates not-null constraint");
        assert_eq!(err.detail.as_deref(), Some("Failing row contains (2, null)."));
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn check_constraint_null_passes_and_false_fails() {
        // Requires evaluating a bound CHECK expression (eval::eval_bool3, EXE-05).
    }

    /// A placeholder expression for the name-order test, which never evaluates it.
    fn dummy_expr() -> pgarbor_sql::ir::BoundExpr {
        use pgarbor_sql::ir::BoundExpr;
        BoundExpr::null(TypeId::BOOL)
    }
}
