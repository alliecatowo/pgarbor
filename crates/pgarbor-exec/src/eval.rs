//! Expression evaluator, part 1: dispatcher and the "control-flow" expression kinds (lease EXE-05,
//! north-star). Related files: `eval_call` (functions, operators, ANY/ALL, row comparison),
//! `eval_cast` (casts, domains, constructors, subscripts), `subplan` (SubPlan), `eval_srf`
//! (set-returning functions).
//!
//! The evaluated tree is an EXECUTABLE `BoundExpr` (plan.rs module docs): no `Column`, `SubLink`,
//! `Aggref`, `WindowFunc`, `SetToDefault`; `Grouping` only with `mask_slot: Some`. Meeting one of
//! those is XX000 via `error::not_executable("<kind>")`. The row argument is the evaluating node's
//! INPUT ROW (`InputRef(i)` = `row[i]`); `OuterRef` reads `Executor::outer_value`.
//!
//! ## Semantics per kind (PG: `executor/execExprInterp.c`, `nodes/primnodes.h`)
//! * `Const(d)`: `d.clone()`. `Param(n)` (1-based): `ex.cx.se.params[n-1].clone()`; `n` beyond the
//!   supplied values -> 08P01 `bind message supplies {have} parameters, but prepared statement
//!   requires {n}`.
//! * `InputRef(i)`: `row[i]` (out of range -> XX000 `input reference {i} out of range`).
//! * `Bool`: three-valued with PG's evaluation order and SHORT-CIRCUITING: `AND` evaluates arguments
//!   left to right and stops at the first FALSE (a later erroring argument is never evaluated);
//!   NULL is remembered and yields NULL only if no FALSE is found; `OR` stops at the first TRUE;
//!   `NOT` of NULL is NULL. Arguments must be boolean (`logic::datum_to_bool3`).
//! * `NullTest`: scalar: `arg IS [NOT] NULL`. `row_semantics` (composite `Datum::Record`): `IS NULL`
//!   is true iff EVERY field is NULL; `IS NOT NULL` is true iff EVERY field is non-NULL (a record with
//!   one NULL field is neither). A NULL record itself counts as all-NULL. Never returns NULL.
//! * `BooleanTest`: `IS TRUE/NOT TRUE/FALSE/NOT FALSE/UNKNOWN/NOT UNKNOWN`; never NULL.
//! * `DistinctFrom`: `compare::not_distinct(env, left.ty, l, r)` (types are equal after binding);
//!   `negated` flips: IS NOT DISTINCT FROM. Never NULL.
//! * `Case`: if `arg` is present evaluate it once and push it on `rt.case_stack` while evaluating the
//!   arms' conditions (`CaseTestValue` reads the top). Arms evaluated in order, the first whose
//!   condition is TRUE returns its result (lazy: other results are not evaluated); no match -> the
//!   `default` or NULL.
//! * `Coalesce`: first non-NULL argument (lazy). `NullIf(a, b)`: evaluate `a`; if NULL -> NULL;
//!   evaluate `b`; `a = b` via the `eq` operator (`eval_call::call_operator`) TRUE -> NULL else `a`.
//! * `MinMax` (GREATEST/LEAST): NULL arguments are ignored; NULL only if all are NULL; comparison with
//!   `compare::cmp_datums(env, e.ty, ..)`; ties keep the first.
//! * `Collate(arg)`: evaluates `arg` (v1 sorts by code point).
//! * `SqlValue`: `CurrentDate` (date of `txn_ts` in the session TimeZone), `CurrentTime` (timetz),
//!   `CurrentTimestamp` (timestamptz = `txn_ts`; apply the precision in `e.ty.typmod` by casting to
//!   `e.ty`), `LocalTime` (time), `LocalTimestamp` (timestamp), use `cast::cast_datum` from
//!   timestamptz with the session `IoConfig` time zone; `CurrentUser`/`CurrentRole`/`User` -> name of
//!   `env.current_user()`, `SessionUser` -> `env.session_user()`, `CurrentCatalog` ->
//!   `se.services.database_name()`, `CurrentSchema` -> name of the first existing schema of the
//!   search path (NULL if none). Names are `Datum::Text`.
//! * `Grouping{ args, mask_slot: Some(s) }`: `mask = row[s]` (Int8); result Int4: for each arg `a`
//!   (position in `AggregateNode::group_keys`) in order, `result = (result << 1) | ((mask >> a) & 1)`.
//! * `FuncCall`/`Op`/`ScalarArrayOp`/`RowCompare` -> `eval_call`; `Cast`/`CoerceToDomain`/`Array`/`Row`/
//!   `FieldSelect`/`Subscript` -> `eval_cast`; `SubPlan` -> `subplan::eval_subplan`.
//! * `eval` checks `ex.check_interrupts()` only in loops (callers do it per row), not per node.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::BoundExpr;
use pgarbor_types::Datum;

use crate::context::ExecContext;
use crate::runtime::Executor;

/// Evaluate `e` over `row` (module docs).
#[allow(unused_variables)]
pub fn eval(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-05", "expression evaluation (eval::eval)"))
}

/// Evaluate a boolean expression to three-valued logic (`None` = NULL). A non-boolean result is
/// XX000 `expression of type X used as a qualification`.
#[allow(unused_variables)]
pub fn eval_bool3(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<Option<bool>> {
    Err(PgError::unimplemented("EXE-05", "boolean evaluation (eval::eval_bool3)"))
}

/// WHERE/ON/HAVING semantics: true only for TRUE (NULL and FALSE both reject).
#[allow(unused_variables)]
pub fn eval_qual(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<bool> {
    Err(PgError::unimplemented("EXE-05", "qualification evaluation (eval::eval_qual)"))
}

/// `eval_qual` for an optional qualification (`None` = TRUE).
#[allow(unused_variables)]
pub fn eval_opt_qual(ex: &mut Executor, e: &Option<BoundExpr>, row: &[Datum]) -> Result<bool> {
    Err(PgError::unimplemented("EXE-05", "optional qualification (eval::eval_opt_qual)"))
}

/// Evaluate a list of expressions (projection) over one row.
#[allow(unused_variables)]
pub fn eval_exprs(ex: &mut Executor, exprs: &[BoundExpr], row: &[Datum]) -> Result<Vec<Datum>> {
    Err(PgError::unimplemented("EXE-05", "expression list evaluation (eval::eval_exprs)"))
}

/// Evaluate an expression that contains no sub-plans outside any plan (column defaults in DDL
/// paths, CHECK validation, index builds): creates a throw-away `Runtime` and `Executor` over `cx`.
/// Sub-plans inside the expression are XX000.
#[allow(unused_variables)]
pub fn eval_standalone(cx: &mut ExecContext, e: &BoundExpr, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-05", "standalone evaluation (eval::eval_standalone)"))
}
