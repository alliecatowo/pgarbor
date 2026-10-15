//! Expression evaluator, part 3: casts, domains, constructors, field and element access
//! (lease EXE-07, north-star).
//!
//! * `Cast(c)`: evaluate `c.arg`; NULL -> NULL; `pgarbor_types::cast::apply_cast(&c.plan, value,
//!   &FuncCtx)` with `arg_types = [c.arg.ty]`, `result_type = e.ty`. Errors are the cast's own
//!   (22P02 `invalid input syntax for type integer: "abc"`, 22003 `integer out of range`, 22001 `value
//!   too long for type character varying(3)`, ...).
//! * `CoerceToDomain{arg, domain}` - PG `ExecEvalConstraintNotNull/Check`: evaluate `arg`; then apply the
//!   constraints of the domain and, base-first, of the domains it is built on (`env.snapshot().
//!   type_obj(domain)` for the NOT NULL flag and base type, `snapshot.domain_constraints(domain)` for the
//!   CHECKs). The bound form of a domain is cached in `ex.rt.domains` (`runtime::DomainRt`): each stored
//!   CHECK text (`Constraint::check.sql`, which refers to the value as `VALUE`) is bound ONCE per
//!   execution by pushing a synthetic scope with `pgarbor_sql::binder::stored::table_expr_scope(b,
//!   "domain", &[("value".into(), base_type)])` (so `VALUE` resolves to `Column{rt_index: 1, attnum: 1}`),
//!   parsing with `pgarbor_sql::parse_expr_text` and binding with the binder's expression entry point
//!   (read `crates/pgarbor-sql/src/binder/expr.rs`), then rewriting the column to `InputRef(0)`
//!   (`tabledesc::columns_to_input_refs`). Evaluate each check over the row `[value]`.
//!   NOT NULL violated -> 23502 `domain {d} does not allow null values` (DATATYPE field = domain name);
//!   a check evaluating to FALSE -> 23514 `value for domain {d} violates check constraint "{c}"`
//!   (oracle: `value for domain zz_d violates check constraint "zz_d_check"`; DATATYPE = domain, CONSTRAINT =
//!   constraint name); NULL passes. NULL input passes unless the domain is NOT NULL. Return the value.
//! * `Array(a)`: evaluate elements in order. `multidims == false`: 1-D array
//!   `Array{elem: a.elem_type, dims: [(n, 1)], items}` (n = 0 -> empty array: no dims); NULL elements
//!   allowed. `multidims == true`: every element is itself an array value of identical dims; the
//!   result has one more leading dimension; a mismatch is 2202E `multidimensional arrays must have
//!   array expressions with matching dimensions`; NULL sub-array elements are an error as in PG.
//! * `Row(r)`: `Datum::Record(values)` in order.
//! * `FieldSelect{arg, field}`: evaluate `arg` (a `Datum::Record`); NULL record -> NULL; value
//!   `fields[field - 1]`; out of range -> XX000. Composite columns of user composite types are
//!   `Record` too (same layout).
//! * `Subscript{array, subscripts, is_slice}` - PG `array_get_element/array_get_slice`: evaluate the
//!   array and every index (int4, NULL index -> NULL result); element access with indexes outside the
//!   dims or on a NULL/empty array -> NULL (never an error); each subscript position uses the
//!   dimension's lower bound; slices clamp to the bounds and return an array of the same type
//!   (`{}` when entirely out of range); if any position is a slice all are treated as slices.
//!   Build `Vec<Option<subscript::Subscript>>` (`None` = NULL subscript, `Index(i)` for `[i]`,
//!   `Slice(lo, hi)` for `[lo:hi]` with `None` = omitted bound) and call
//!   `pgarbor_types::array::subscript::fetch(&array, &subs)` instead of re-implementing. A NULL
//!   array operand gives NULL. `ARRAY[..]` construction uses `subscript::construct(elem_oid, items,
//!   elements_are_arrays)`.
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{ArrayExpr, BoundExpr, CastExpr, CoerceToDomain, FieldSelect, RowExpr, SubscriptExpr};
use pgarbor_types::{Datum, TypeId};

use crate::runtime::Executor;

#[allow(unused_variables)]
pub fn eval_cast(ex: &mut Executor, c: &CastExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "cast evaluation (eval_cast::eval_cast)"))
}

#[allow(unused_variables)]
pub fn eval_coerce_to_domain(ex: &mut Executor, c: &CoerceToDomain, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "domain coercion (eval_cast::eval_coerce_to_domain)"))
}

#[allow(unused_variables)]
pub fn eval_array(ex: &mut Executor, a: &ArrayExpr, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "ARRAY[] construction (eval_cast::eval_array)"))
}

#[allow(unused_variables)]
pub fn eval_row(ex: &mut Executor, r: &RowExpr, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "ROW() construction (eval_cast::eval_row)"))
}

#[allow(unused_variables)]
pub fn eval_field_select(ex: &mut Executor, f: &FieldSelect, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "field selection (eval_cast::eval_field_select)"))
}

#[allow(unused_variables)]
pub fn eval_subscript(ex: &mut Executor, s: &SubscriptExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "array subscripting (eval_cast::eval_subscript)"))
}

/// Helper shared with `eval_srf`/`dml_*`: apply an assignment-context cast of `value` from `from` to
/// `to` using the same machinery as `Cast` (`cast::cast_datum` with `CastContext::Assignment`).
#[allow(unused_variables)]
pub fn coerce_assignment(ex: &mut Executor, value: Datum, from: TypeId, to: TypeId) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "assignment coercion (eval_cast::coerce_assignment)"))
}

/// Evaluate an expression and coerce the result with `coerce_assignment` if its static type differs
/// from `to`.
#[allow(unused_variables)]
pub fn eval_coerced(ex: &mut Executor, e: &BoundExpr, row: &[Datum], to: TypeId) -> Result<Datum> {
    Err(PgError::unimplemented("EXE-07", "coerced evaluation (eval_cast::eval_coerced)"))
}
