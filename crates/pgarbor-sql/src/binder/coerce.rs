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
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::cast::plan_cast;
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::io;
use pgarbor_types::oids;
use pgarbor_types::registry::{base_type_and_typmod, is_polymorphic};
use pgarbor_types::resolve::coerce::{select_common_type, select_common_typmod, verify_common_type};
use pgarbor_types::{Datum, TypeEnv, TypeId};

use super::context::{Binder, BoundArg};
use super::params;
use crate::diag;
use crate::ir::{BoundExpr, CoerceToDomain, CoercionForm, ExprKind};

/// Coerce `arg` to `target` in context `cctx`. `Ok(Some(expr))` on success; `Ok(None)` when no cast
/// path exists in that context (the caller builds the context-specific error); `Err` for conversion
/// errors of literals (22P02, 22003, 22007, ...), inconsistent parameter types (42P08).
/// `form` is the display form recorded on a created Cast node (`Implicit` for the binder's own
/// casts, `Explicit` for user `::`).
pub fn try_coerce(
    b: &mut Binder,
    arg: &BoundArg,
    target: TypeId,
    cctx: CastContext,
    form: CoercionForm,
) -> Result<Option<BoundExpr>> {
    // Coercion to `any` (and to `unknown` itself) never changes the node.
    if target.oid == oids::ANY || target.oid == oids::UNKNOWN {
        return Ok(Some(arg.expr.clone()));
    }
    // Polymorphic targets are substituted by function resolution before they reach here.
    if is_polymorphic(target.oid) {
        return Ok(None);
    }
    if arg.is_unknown() {
        return coerce_unknown(b, arg, target, cctx, form);
    }
    check_row_arity(b, arg, target)?;
    coerce_expr_to(b, &arg.expr, target, cctx, form)
}

/// `try_coerce`, failing with 42846 `cannot cast type <from> to <to>` (position = `arg.pos`,
/// `diag::cannot_cast`) when there is no path. Used for explicit casts and internal conversions.
pub fn coerce_strict(b: &mut Binder, arg: &BoundArg, target: TypeId, cctx: CastContext, form: CoercionForm) -> Result<BoundExpr> {
    match try_coerce(b, arg, target, cctx, form)? {
        Some(e) => Ok(e),
        None => Err(diag::cannot_cast(&b.type_name(arg.ty()), &b.type_name(target), arg.pos)),
    }
}

/// Assignment coercion for INSERT / UPDATE / COPY values into column `col_name` of type `col_type`:
/// `try_coerce(.., Assignment, Implicit)`, else 42804 `column "a" is of type integer but expression
/// is of type text` + HINT `You will need to rewrite or cast the expression.`
/// (`diag::assignment_type_mismatch`). A `SetToDefault` expression passes through unchanged.
pub fn coerce_assignment(b: &mut Binder, arg: &BoundArg, col_name: &str, col_type: TypeId) -> Result<BoundExpr> {
    if matches!(arg.expr.kind, ExprKind::SetToDefault { .. }) {
        return Ok(arg.expr.clone());
    }
    match try_coerce(b, arg, col_type, CastContext::Assignment, CoercionForm::Implicit)? {
        Some(e) => Ok(e),
        None => Err(diag::assignment_type_mismatch(
            col_name,
            &b.type_name(col_type),
            &b.type_name(arg.ty()),
            arg.pos,
        )),
    }
}

/// Coerce to boolean for WHERE/HAVING/ON/FILTER/CASE WHEN/AND/OR/NOT: unknown literal -> bool input
/// (`where 'x'` is 22P02 `invalid input syntax for type boolean: "x"`); non-boolean typed ->
/// implicit cast if one exists else 42804 `argument of WHERE must be type boolean, not type
/// integer` (`what` is the clause word; `diag::not_boolean`).
/// PG `coerce_to_boolean` coerces with `COERCION_ASSIGNMENT` (not implicit), so the cast context
/// used here is `Assignment`.
pub fn coerce_to_boolean(b: &mut Binder, arg: &BoundArg, what: &str) -> Result<BoundExpr> {
    if arg.ty().oid == oids::BOOL {
        return Ok(arg.expr.clone());
    }
    match try_coerce(b, arg, TypeId::BOOL, CastContext::Assignment, CoercionForm::Implicit)? {
        Some(e) => Ok(e),
        None => Err(diag::not_boolean(what, &b.type_name(arg.ty()), arg.pos)),
    }
}

/// Coerce to a specific type with implicit casting, producing the standard
/// `argument of <what> must be type <target>, not type <actual>` 42804 error (used by LIMIT/OFFSET
/// `bigint`, window frame offsets, `FETCH`).
/// PG `coerce_to_specific_type` uses `COERCION_ASSIGNMENT` (so `LIMIT 2.5` is accepted); the cast
/// context used here is `Assignment` to match PG.
pub fn coerce_to_specific(b: &mut Binder, arg: &BoundArg, target: TypeId, what: &str) -> Result<BoundExpr> {
    match try_coerce(b, arg, target, CastContext::Assignment, CoercionForm::Implicit)? {
        Some(e) => Ok(e),
        None => Err(diag::with_position(
            PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("argument of {what} must be type {}, not type {}", b.type_name(target), b.type_name(arg.ty())),
            ),
            arg.pos,
        )),
    }
}

/// Pick the common type of `args` (UNION/INTERSECT/EXCEPT columns, CASE results, COALESCE/GREATEST/
/// LEAST/NULLIF-like lists, ARRAY[] elements, VALUES columns, IN lists) with
/// `pgarbor_types::resolve::coerce::select_common_type(env, &oids, context)`, coerce every
/// argument with Implicit context and return them with the common `TypeId` (typmod:
/// `select_common_typmod`). All-unknown inputs resolve to `text`. `context` is the PG context word
/// used in the error `<context> types integer and text cannot be matched` (42804): `UNION`,
/// `INTERSECT`, `EXCEPT`, `CASE`, `COALESCE`, `GREATEST`, `LEAST`, `VALUES`, `ARRAY`, `IN`.
pub fn coerce_to_common(b: &mut Binder, args: Vec<BoundArg>, context: &str) -> Result<(Vec<BoundExpr>, TypeId)> {
    if args.is_empty() {
        return Err(PgError::internal("coerce_to_common called with no arguments"));
    }
    let env: &dyn TypeEnv = b.env;
    let oids_in: Vec<Oid> = args.iter().map(|a| a.ty().oid).collect();
    let types_in: Vec<TypeId> = args.iter().map(|a| a.ty()).collect();
    let common = select_common_type(env, &oids_in, context)?;
    let typmod = select_common_typmod(&types_in, common)?;
    verify_common_type(env, &oids_in, common, context)?;
    let target = TypeId::new(common, typmod);
    let mut out = Vec::with_capacity(args.len());
    for arg in &args {
        match try_coerce(b, arg, target, CastContext::Implicit, CoercionForm::Implicit)? {
            Some(e) => out.push(e),
            None => {
                return Err(PgError::new(
                    SqlState::CANNOT_COERCE,
                    format!("{context} could not convert type {} to {}", b.type_name(arg.ty()), b.type_name(target)),
                ))
            }
        }
    }
    Ok((out, target))
}

/// `resolveTargetListUnknowns`: an output expression of type `unknown` (string literal, NULL, bare
/// parameter) is coerced to `text` (`select 'a'` has type text; `select null` text; `select $1`
/// pins `$1` to text). Other types unchanged.
pub fn resolve_unknown_output(b: &mut Binder, arg: &BoundArg) -> Result<BoundExpr> {
    if !arg.is_unknown() {
        return Ok(arg.expr.clone());
    }
    match try_coerce(b, arg, TypeId::TEXT, CastContext::Implicit, CoercionForm::Implicit)? {
        Some(e) => Ok(e),
        None => Err(PgError::internal("unknown-typed output expression has no text conversion")),
    }
}

/// Build a Cast node from a plan (no checks). Fully implemented helper used by the lease bodies.
pub fn make_cast_node(arg: BoundExpr, plan: pgarbor_types::cast::CastPlan, form: CoercionForm) -> BoundExpr {
    let ty = plan.to;
    BoundExpr::new(crate::ir::ExprKind::Cast(Box::new(crate::ir::CastExpr { arg, plan, form })), ty)
}

// ---------------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------------

/// Is `oid` a domain type (`typtype = 'd'`)?
fn is_domain(env: &dyn TypeEnv, oid: Oid) -> bool {
    env.type_info(oid).map(|t| t.typtype == b'd').unwrap_or(false)
}

/// Base type of a domain target, carrying the domain's typmod unless the target supplies one
/// (same rule as `io::domain_base` and `plan_cast`).
fn domain_base(env: &dyn TypeEnv, target: TypeId) -> TypeId {
    let (base, tm) = base_type_and_typmod(env, target.oid, -1);
    TypeId::new(base, if target.typmod >= 0 { target.typmod } else { tm })
}

/// `CoerceToDomain` over `inner`; the node's type is the domain (rule 5).
fn wrap_domain(inner: BoundExpr, domain: TypeId, form: CoercionForm) -> BoundExpr {
    BoundExpr::new(
        ExprKind::CoerceToDomain(Box::new(CoerceToDomain { arg: inner, domain: domain.oid, form })),
        domain,
    )
}

/// Coerce an unknown-typed argument (literal string, NULL, or unresolved parameter) to `target`.
fn coerce_unknown(
    b: &mut Binder,
    arg: &BoundArg,
    target: TypeId,
    cctx: CastContext,
    form: CoercionForm,
) -> Result<Option<BoundExpr>> {
    let env: &dyn TypeEnv = b.env;
    let domain = is_domain(env, target.oid);
    let base = if domain { domain_base(env, target) } else { target };
    // PG passes typmod -1 to the input function, except INTERVAL which must see its typmod (parse_coerce.c
    // `coerce_type`); the length/precision rule is then applied as a separate coercion step.
    let input_ty = if base.oid == oids::INTERVAL { base } else { TypeId::of(base.oid) };
    let node = match &arg.expr.kind {
        ExprKind::Param(n) => {
            // The parameter is pinned to the type itself, unmodified; a typmod is applied afterwards.
            let pinned = params::resolve_param(b, *n, TypeId::of(target.oid), arg.pos)?;
            if target.typmod >= 0 && !domain {
                return coerce_plain(env, &pinned, target, cctx, form);
            }
            return Ok(Some(pinned));
        }
        ExprKind::Const(Datum::Null) => BoundExpr::null(base),
        ExprKind::Const(Datum::Text(s)) => {
            let ctx = b.io_ctx();
            let datum = io::input(&ctx, input_ty, s).map_err(|e| diag::with_position(e, arg.pos))?;
            BoundExpr::constant(datum, input_ty)
        }
        _ => return coerce_expr_to(b, &arg.expr, target, cctx, form),
    };
    let node = match coerce_plain(env, &node, base, cctx, form)? {
        Some(n) => n,
        None => return Ok(None),
    };
    Ok(Some(if domain { wrap_domain(node, target, form) } else { node }))
}

/// Coerce a typed expression to `target`, going through the base type first for domains (rule 5).
fn coerce_expr_to(
    b: &Binder,
    expr: &BoundExpr,
    target: TypeId,
    cctx: CastContext,
    form: CoercionForm,
) -> Result<Option<BoundExpr>> {
    let env: &dyn TypeEnv = b.env;
    if is_domain(env, target.oid) {
        let base = domain_base(env, target);
        return Ok(coerce_plain(env, expr, base, cctx, form)?.map(|inner| wrap_domain(inner, target, form)));
    }
    coerce_plain(env, expr, target, cctx, form)
}

/// Typed, non-domain coercion (rule 3). `Ok(None)` when `find_cast` finds no pathway in `cctx`.
fn coerce_plain(
    env: &dyn TypeEnv,
    expr: &BoundExpr,
    target: TypeId,
    cctx: CastContext,
    form: CoercionForm,
) -> Result<Option<BoundExpr>> {
    let from = expr.ty;
    if from.oid == target.oid && (target.typmod < 0 || target.typmod == from.typmod) {
        return Ok(Some(expr.clone()));
    }
    if pgarbor_types::cast::find_cast(env, from.oid, target.oid, cctx)?.is_none() {
        return Ok(None);
    }
    let plan = plan_cast(env, from, target, cctx)?;
    Ok(Some(make_cast_node(expr.clone(), plan, form)))
}

/// PG `coerce_record_to_complex` arity rule (rule 7): a `ROW(...)` coerced to a composite type must
/// supply exactly the composite's non-dropped attributes.
fn check_row_arity(b: &Binder, arg: &BoundArg, target: TypeId) -> Result<()> {
    let env: &dyn TypeEnv = b.env;
    let row = match &arg.expr.kind {
        ExprKind::Row(r) if arg.expr.ty.oid == oids::RECORD => r,
        _ => return Ok(()),
    };
    let composite = if is_domain(env, target.oid) { domain_base(env, target) } else { target };
    let is_composite = env.type_info(composite.oid).map(|t| t.typtype == b'c').unwrap_or(false);
    if !is_composite {
        return Ok(());
    }
    let fields = match env.composite_fields(composite.oid) {
        Some(f) => f,
        None => return Ok(()),
    };
    let wanted = fields.iter().filter(|f| !f.dropped).count();
    let got = row.args.len();
    if got == wanted {
        return Ok(());
    }
    let detail = if got < wanted { "Input has too few columns." } else { "Input has too many columns." };
    let mut err = diag::cannot_cast("record", &b.type_name(composite), arg.pos);
    err.detail = Some(detail.to_string());
    Err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::ddl::spec::CreateDomainSpec;
    use pgarbor_catalog::testing;
    use pgarbor_catalog::CatalogEnv;
    use pgarbor_types::context::IoConfig;

    /// Run `f` with a fresh binder over `env`.
    fn with_binder<R>(env: &CatalogEnv, f: impl FnOnce(&mut Binder<'_>) -> R) -> R {
        let src = SourceMap::new("select 1");
        let ctx = BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let mut b = Binder::new(&ctx, &src);
        f(&mut b)
    }

    fn unknown_text(s: &str) -> BoundExpr {
        BoundExpr::constant(Datum::text(s), TypeId::UNKNOWN)
    }

    #[test]
    fn implicit_int4_to_numeric_is_cast_node() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(BoundExpr::column(0, 1, 1, TypeId::INT4), Some(3));
            let got = try_coerce(b, &arg, TypeId::NUMERIC, CastContext::Implicit, CoercionForm::Implicit)?
                .expect("int4 -> numeric is implicit");
            assert!(matches!(got.kind, ExprKind::Cast(ref c) if c.form == CoercionForm::Implicit));
            assert_eq!(got.ty, TypeId::NUMERIC);
            Ok(())
        })
    }

    #[test]
    fn numeric_to_int4_implicit_none_assignment_some() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(BoundExpr::column(0, 1, 1, TypeId::NUMERIC), None);
            assert!(try_coerce(b, &arg, TypeId::INT4, CastContext::Implicit, CoercionForm::Implicit)?.is_none());
            assert!(try_coerce(b, &arg, TypeId::INT4, CastContext::Assignment, CoercionForm::Implicit)?.is_some());
            Ok(())
        })
    }

    #[test]
    fn unknown_literal_to_int4_reports_22p02_at_literal() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(unknown_text("x"), Some(12));
            let e = try_coerce(b, &arg, TypeId::INT4, CastContext::Implicit, CoercionForm::Implicit).unwrap_err();
            assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
            assert_eq!(e.message, "invalid input syntax for type integer: \"x\"");
            assert_eq!(e.position, Some(12));
            Ok(())
        })
    }

    #[test]
    fn unknown_null_becomes_typed_null() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(BoundExpr::null(TypeId::UNKNOWN), None);
            let got = try_coerce(b, &arg, TypeId::INT4, CastContext::Implicit, CoercionForm::Implicit)?
                .expect("NULL coerces");
            assert!(got.is_null_const());
            assert_eq!(got.ty, TypeId::INT4);
            Ok(())
        })
    }

    #[test]
    fn coerce_to_boolean_rejects_integer() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(BoundExpr::column(0, 1, 1, TypeId::INT4), None);
            let e = coerce_to_boolean(b, &arg, "WHERE").unwrap_err();
            assert_eq!(e.sqlstate.code(), "42804");
            assert_eq!(e.message, "argument of WHERE must be type boolean, not type integer");
            Ok(())
        })
    }

    #[test]
    fn common_type_numeric_and_text_mismatch() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let args = vec![
                BoundArg::new(BoundExpr::column(0, 1, 1, TypeId::INT4), None),
                BoundArg::new(BoundExpr::column(0, 1, 2, TypeId::NUMERIC), None),
            ];
            let (exprs, ty) = coerce_to_common(b, args, "UNION")?;
            assert_eq!(ty, TypeId::NUMERIC);
            assert_eq!(exprs.len(), 2);

            let args = vec![
                BoundArg::new(BoundExpr::column(0, 1, 1, TypeId::INT4), None),
                BoundArg::new(BoundExpr::column(0, 1, 2, TypeId::TEXT), None),
            ];
            let e = coerce_to_common(b, args, "UNION").unwrap_err();
            assert_eq!(e.sqlstate.code(), "42804");
            assert_eq!(e.message, "UNION types integer and text cannot be matched");
            Ok(())
        })
    }

    #[test]
    fn all_unknown_inputs_resolve_to_text() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let args = vec![
                BoundArg::new(BoundExpr::null(TypeId::UNKNOWN), None),
                BoundArg::new(unknown_text("a"), None),
            ];
            let (_exprs, ty) = coerce_to_common(b, args, "UNION")?;
            assert_eq!(ty, TypeId::TEXT);
            Ok(())
        })
    }

    #[test]
    fn unknown_output_resolves_to_text() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        with_binder(&env, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(unknown_text("a"), None);
            let got = resolve_unknown_output(b, &arg)?;
            assert_eq!(got.ty, TypeId::TEXT);
            assert!(matches!(got.kind, ExprKind::Const(Datum::Text(ref s)) if s == "a"));
            Ok(())
        })
    }

    #[test]
    fn unknown_literal_to_domain_wraps_coerce_to_domain() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_res, env2) = testing::run(&mut kv, &env, |t| {
            t.create_domain(&CreateDomainSpec {
                namespace: pgarbor_types::oids::PUBLIC_NAMESPACE,
                name: "posint".to_string(),
                owner: 10,
                base: TypeId::INT4,
                collation: None,
                default: None,
                not_null: None,
                checks: Vec::new(),
                ndims: 0,
            })
            .map(|_| ())
        })?;
        let dom = env2.type_by_name(None, "posint").expect("domain was created");
        with_binder(&env2, |b: &mut Binder<'_>| -> Result<()> {
            let arg = BoundArg::new(unknown_text("5"), None);
            let got = try_coerce(b, &arg, TypeId::of(dom), CastContext::Assignment, CoercionForm::Implicit)?
                .expect("literal coerces to the domain");
            assert_eq!(got.ty.oid, dom);
            assert!(matches!(got.kind, ExprKind::CoerceToDomain(_)));
            if let ExprKind::CoerceToDomain(d) = &got.kind {
                assert_eq!(d.domain, dom);
                assert!(matches!(d.arg.kind, ExprKind::Const(Datum::Int4(5))));
            }
            Ok(())
        })
    }
}
