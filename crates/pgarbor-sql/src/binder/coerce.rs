//! Type coercion at bind time (SQL-09, sonnet). PG reference: `src/backend/parser/parse_coerce.c`
//! (`coerce_to_target_type`, `coerce_type`, `can_coerce_type`, `coerce_to_boolean`,
//! `coerce_to_specific_type`, `select_common_type`, `coerce_to_common_type`,
//! `coerce_record_to_complex`, `coerce_to_domain`), `parse_func.c` for `coerce_type` with
//! unknown literals, `parse_param.c` (parameter hook).
//!
//! Rules (all observable PG behavior, oracle-checked):
//! 1. **Unknown literal -> concrete type**: a `Const(Datum::Text(s))` of type `unknown` is converted
//!    NOW with `pgarbor_types::io::input(&b.io_ctx(), target, s)` into a `Const` of the target type
//!    (so `select 1 + 'x'` -> `22P02 invalid input syntax for type integer: "x"` at the literal;
//!    `insert into t(int_col) values ('x')` the same). `Const(Null)` of type unknown becomes a typed
//!    NULL. Input errors get `BoundArg::pos` if they have no position. Array targets use array input
//!    (`'{1,2}'` to `int4[]`); enum targets validate labels (22P02 `invalid input value for enum`);
//!    domain targets: input of the base type, then wrap `CoerceToDomain` (constraints checked at run
//!    time); typmod targets apply the typmod (`'abc'` -> `varchar(2)` is 22001 for explicit-or-assignment
//!    casts, silently-truncating only for explicit; follow `enforce_typmod` rules).
//! 2. **Unknown Param -> concrete type**: `params::resolve_param` pins the parameter type.
//! 3. **Typed expression -> target**: if `source == target` (same oid; typmod difference needs a
//!    length-coercion step when `target.typmod != -1` and differs) return it unchanged. Else
//!    `pgarbor_types::cast::plan_cast(env, from, to, ctx)`; wrap `ExprKind::Cast(CastExpr{arg, plan,
//!    form: Implicit})` (`Explicit` form for user casts) with `ty = target`. Binary-coercible
//!    relabels are Cast nodes with a Relabel-only plan (cheap at run time). No cast path ->
//!    `Ok(None)` from `try_coerce`.
//! 4. Contexts (`CastContext`): `Implicit` for operator/function arguments, `Assignment` for
//!    INSERT/UPDATE targets and similar, `Explicit` for `CAST`/`::`.
//! 5. **Domains**: coercing to a domain type D = coerce to D's base type then `CoerceToDomain`.
//!    Coercing FROM a domain to its base is a relabel.
//! 6. **Polymorphic targets** are never passed here (function resolution substitutes actual types).
//! 7. **Row/record**: `ROW(1,'a')` to a composite type coerces field-wise (`coerce_record_to_complex`);
//!    wrong arity -> 42846 `cannot cast type record to <composite>` DETAIL `Input has too few
//!    columns.` / `Input has too many columns.`.
//! 8. Constant folding: when the operand is already a `Const` and the cast is IMMUTABLE and safe,
//!    fold at bind time with `cast::cast_datum` (only for literals; keeps `EXPLAIN` text like PG).
//!    Do NOT fold if it errors (`'x'::int` errors at bind: the error IS the PG behavior).
use pgarbor_common::{PgError, Result};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::TypeId;

use super::context::{Binder, BoundArg};
use crate::ir::{BoundExpr, CoercionForm};

/// Coerce `arg` to `target` in context `cctx`. `Ok(Some(expr))` on success; `Ok(None)` when no cast
/// path exists in that context (the caller builds the context-specific error); `Err` for conversion
/// errors of literals (22P02, 22003, 22007, ...), inconsistent parameter types (42P08).
/// `form` is the display form recorded on a created Cast node (`Implicit` for the binder's own
/// casts, `Explicit` for user `::`).
#[allow(unused_variables)]
pub fn try_coerce(
    b: &mut Binder,
    arg: &BoundArg,
    target: TypeId,
    cctx: CastContext,
    form: CoercionForm,
) -> Result<Option<BoundExpr>> {
    Err(PgError::unimplemented("SQL-09", "coercion (coerce::try_coerce)"))
}

/// `try_coerce`, failing with 42846 `cannot cast type <from> to <to>` (position = `arg.pos`,
/// `diag::cannot_cast`) when there is no path. Used for explicit casts and internal conversions.
#[allow(unused_variables)]
pub fn coerce_strict(b: &mut Binder, arg: &BoundArg, target: TypeId, cctx: CastContext, form: CoercionForm) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-09", "strict coercion (coerce::coerce_strict)"))
}

/// Assignment coercion for INSERT / UPDATE / COPY values into column `col_name` of type `col_type`:
/// `try_coerce(.., Assignment, Implicit)`, else 42804 `column "a" is of type integer but expression
/// is of type text` + HINT `You will need to rewrite or cast the expression.`
/// (`diag::assignment_type_mismatch`). A `SetToDefault` expression passes through unchanged.
#[allow(unused_variables)]
pub fn coerce_assignment(b: &mut Binder, arg: &BoundArg, col_name: &str, col_type: TypeId) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-09", "assignment coercion (coerce::coerce_assignment)"))
}

/// Coerce to boolean for WHERE/HAVING/ON/FILTER/CASE WHEN/AND/OR/NOT: unknown literal -> bool input
/// (`where 'x'` is 22P02 `invalid input syntax for type boolean: "x"`); non-boolean typed ->
/// implicit cast if one exists else 42804 `argument of WHERE must be type boolean, not type
/// integer` (`what` is the clause word; `diag::not_boolean`).
#[allow(unused_variables)]
pub fn coerce_to_boolean(b: &mut Binder, arg: &BoundArg, what: &str) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-09", "boolean coercion (coerce::coerce_to_boolean)"))
}

/// Coerce to a specific type with implicit casting, producing the standard
/// `argument of <what> must be type <target>, not type <actual>` 42804 error (used by LIMIT/OFFSET
/// `bigint`, window frame offsets, `FETCH`).
#[allow(unused_variables)]
pub fn coerce_to_specific(b: &mut Binder, arg: &BoundArg, target: TypeId, what: &str) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-09", "specific-type coercion (coerce::coerce_to_specific)"))
}

/// Pick the common type of `args` (UNION/INTERSECT/EXCEPT columns, CASE results, COALESCE/GREATEST/
/// LEAST/NULLIF-like lists, ARRAY[] elements, VALUES columns, IN lists) with
/// `pgarbor_types::resolve::coerce::select_common_type(env, &oids, context)`, coerce every
/// argument with Implicit context and return them with the common `TypeId` (typmod:
/// `select_common_typmod`). All-unknown inputs resolve to `text`. `context` is the PG context word
/// used in the error `<context> types integer and text cannot be matched` (42804): `UNION`,
/// `INTERSECT`, `EXCEPT`, `CASE`, `COALESCE`, `GREATEST`, `LEAST`, `VALUES`, `ARRAY`, `IN`.
#[allow(unused_variables)]
pub fn coerce_to_common(b: &mut Binder, args: Vec<BoundArg>, context: &str) -> Result<(Vec<BoundExpr>, TypeId)> {
    Err(PgError::unimplemented("SQL-09", "common type coercion (coerce::coerce_to_common)"))
}

/// `resolveTargetListUnknowns`: an output expression of type `unknown` (string literal, NULL, bare
/// parameter) is coerced to `text` (`select 'a'` has type text; `select null` text; `select $1`
/// pins `$1` to text). Other types unchanged.
#[allow(unused_variables)]
pub fn resolve_unknown_output(b: &mut Binder, arg: &BoundArg) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-09", "unknown output resolution (coerce::resolve_unknown_output)"))
}

/// Build a Cast node from a plan (no checks). Fully implemented helper used by the lease bodies.
pub fn make_cast_node(arg: BoundExpr, plan: pgarbor_types::cast::CastPlan, form: CoercionForm) -> BoundExpr {
    let ty = plan.to;
    BoundExpr::new(crate::ir::ExprKind::Cast(Box::new(crate::ir::CastExpr { arg, plan, form })), ty)
}
