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
//!
//! ## Implementation notes
//! * Child expressions are evaluated with `crate::eval::eval` (EXE-05). `FuncCtx` values are built with
//!   `crate::eval_call::with_func_ctx` (EXE-06), the shared constructor.
//! * Domain constraints are evaluated for NULL input too, as PG does (`VALUE IS NOT NULL` checks reject
//!   NULL); a NULL check result passes.
//! * Subscript positions in slice mode: `lower`/`upper` are optional bounds; a scalar position written
//!   inside a slice expression arrives as a bounded pair (the binder turns `[2]` into `[1:2]`).
//!   Outside slice mode `lower` is the index.
//!   Source: PG `executor/execExprInterp.c` (`EEOP_SBSREF_SUBSCRIPTS` jumps to the end on the first NULL
//!   subscript; the container is evaluated before the subscripts).

use std::rc::Rc;

use pgarbor_catalog::model::TypeDef;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::binder::context::{Binder, BindContext, BindSession, ExprCtx};
use pgarbor_sql::binder::expr::bind_expr;
use pgarbor_sql::binder::stored::table_expr_scope;
use pgarbor_sql::ir::{ArrayExpr, BoundExpr, CastExpr, CoerceToDomain, FieldSelect, RowExpr, SubscriptExpr, SubscriptItem};
use pgarbor_sql::{parse_expr_text, SourceMap};
use pgarbor_types::array::subscript::{self, Subscript};
use pgarbor_types::cast::{apply_cast, cast_datum, CastContext, CastStep};
use pgarbor_types::datum::type_mismatch;
use pgarbor_types::{Datum, TypeEnv, TypeId};

use crate::context::{EngineServices, SessionEnv};
use crate::eval::eval;
use crate::eval_call::with_func_ctx;
use crate::runtime::{DomainCheckRt, DomainRt, Executor};
use crate::tabledesc::columns_to_input_refs;

/// Guard against corrupt catalogs whose domains are nested without end.
const MAX_DOMAIN_DEPTH: usize = 64;

/// `Cast(c)`: NULL input -> NULL, otherwise the planned cast applied with the cast's own errors.
pub fn eval_cast(ex: &mut Executor, c: &CastExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    let value = eval(ex, &c.arg, row)?;
    if value.is_null() {
        return Ok(Datum::Null);
    }
    let arg_types = [c.arg.ty];
    let out = with_func_ctx(&*ex.cx, &arg_types, result_type, |fc| apply_cast(&c.plan, value, fc))?;
    check_cast_domains(ex, &c.plan.steps, &out)?;
    Ok(out)
}

/// Run the domain checks recorded in a cast plan on the cast's result: `DomainCheck` steps check the
/// value itself, `ArrayCoerce` steps check every element (`posint[]` element checks). The type crate
/// leaves these steps as identity, so the constraints are enforced here.
fn check_cast_domains(ex: &mut Executor, steps: &[CastStep], value: &Datum) -> Result<()> {
    for step in steps {
        match step {
            CastStep::DomainCheck { domain } => check_domain_value(ex, *domain, value)?,
            CastStep::ArrayCoerce { elem_plan } if needs_domain_check(&elem_plan.steps) => {
                if let Datum::Array(arr) = value {
                    for item in &arr.items {
                        check_cast_domains(ex, &elem_plan.steps, item)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// True when a cast plan (or an element plan) contains a `DomainCheck` step at any depth.
fn needs_domain_check(steps: &[CastStep]) -> bool {
    steps.iter().any(|step| match step {
        CastStep::DomainCheck { .. } => true,
        CastStep::ArrayCoerce { elem_plan } => needs_domain_check(&elem_plan.steps),
        _ => false,
    })
}

/// `CoerceToDomain`: NOT NULL and CHECK enforcement of `c.domain` (module docs). The bound constraints
/// are cached per execution in `ex.rt.domains`.
pub fn eval_coerce_to_domain(ex: &mut Executor, c: &CoerceToDomain, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    let _ = result_type;
    let value = eval(ex, &c.arg, row)?;
    check_domain_value(ex, c.domain, &value)?;
    Ok(value)
}

/// PG `ExecEvalConstraintNotNull` / `ExecEvalConstraintCheck` for `domain` over `value`: NOT NULL
/// violations and CHECK results of FALSE are errors; a NULL check result passes.
fn check_domain_value(ex: &mut Executor, domain: Oid, value: &Datum) -> Result<()> {
    let rt = domain_rt(ex, domain)?;
    if rt.not_null && value.is_null() {
        let name = domain_name(ex, domain)?;
        let msg = format!("domain {name} does not allow null values");
        let mut err = PgError::new(SqlState::NOT_NULL_VIOLATION, msg).with_datatype(name);
        err.schema = domain_schema(ex, domain);
        return Err(err);
    }
    for check in &rt.checks {
        let result = eval(ex, &check.expr, std::slice::from_ref(value))?;
        if matches!(result, Datum::Bool(false)) {
            let name = domain_name(ex, domain)?;
            let msg = format!("value for domain {name} violates check constraint \"{}\"", check.name);
            let mut err = PgError::new(SqlState::CHECK_VIOLATION, msg)
                .with_datatype(name)
                .with_constraint(check.name.clone());
            err.schema = domain_schema(ex, domain);
            return Err(err);
        }
    }
    Ok(())
}

/// `ARRAY[...]`: evaluate the elements in order and build the array with `subscript::construct`
/// (which applies the multidimensional rules).
pub fn eval_array(ex: &mut Executor, a: &ArrayExpr, row: &[Datum]) -> Result<Datum> {
    let mut items = Vec::with_capacity(a.elements.len());
    for e in &a.elements {
        items.push(eval(ex, e, row)?);
    }
    let arr = subscript::construct(a.elem_type, items, a.multidims)?;
    Ok(Datum::Array(Box::new(arr)))
}

/// `ROW(...)`: a record of the evaluated arguments in order.
pub fn eval_row(ex: &mut Executor, r: &RowExpr, row: &[Datum]) -> Result<Datum> {
    let mut fields = Vec::with_capacity(r.args.len());
    for e in &r.args {
        fields.push(eval(ex, e, row)?);
    }
    Ok(Datum::Record(fields))
}

/// `(composite).field`: NULL record -> NULL; otherwise the 1-based field of the record.
pub fn eval_field_select(ex: &mut Executor, f: &FieldSelect, row: &[Datum]) -> Result<Datum> {
    let value = eval(ex, &f.arg, row)?;
    match value {
        Datum::Null => Ok(Datum::Null),
        Datum::Record(fields) => {
            let idx = i64::from(f.field) - 1;
            let index = usize::try_from(idx).map_err(|_| PgError::internal("invalid composite field number"))?;
            if let Some(value) = fields.get(index) {
                return Ok(value.clone());
            }
            // Metadata-only ADD ATTRIBUTE can leave stored tuples physically shorter than the
            // current descriptor. PostgreSQL reads such trailing attributes as NULL.
            if ex.se().env.composite_fields(f.arg.ty.oid).is_some_and(|desc| index < desc.len()) {
                return Ok(Datum::Null);
            }
            Err(PgError::internal(format!("record has no field {}", f.field)))
        }
        other => Err(type_mismatch("record", &other)),
    }
}

/// `arr[...]` / `arr[lo:hi]` (module docs). NULL subscripts and NULL arrays give NULL; out-of-range
/// elements give NULL. The subscripts are evaluated after the array, as PG does.
pub fn eval_subscript(ex: &mut Executor, s: &SubscriptExpr, result_type: TypeId, row: &[Datum]) -> Result<Datum> {
    let _ = result_type;
    let array = eval(ex, &s.array, row)?;
    let mut subs: Vec<Option<Subscript>> = Vec::with_capacity(s.subscripts.len());
    for item in &s.subscripts {
        match subscript_position(ex, item, s.is_slice, row)? {
            Some(sub) => subs.push(Some(sub)),
            None => return Ok(Datum::Null),
        }
    }
    let arr = match array {
        Datum::Null => return Ok(Datum::Null),
        Datum::Array(a) => a,
        other => return Err(type_mismatch("array", &other)),
    };
    subscript::fetch(&*arr, &subs)
}

/// Helper shared with `eval_srf`/`dml_*`: apply an assignment-context cast of `value` from `from` to
/// `to` using the same machinery as `Cast` (`cast::cast_datum` with `CastContext::Assignment`).
pub fn coerce_assignment(ex: &mut Executor, value: Datum, from: TypeId, to: TypeId) -> Result<Datum> {
    if from == to || value.is_null() {
        return Ok(value);
    }
    let arg_types = [from];
    with_func_ctx(&*ex.cx, &arg_types, to, |fc| cast_datum(value, from, to, CastContext::Assignment, fc))
}

/// Evaluate an expression and coerce the result with `coerce_assignment` if its static type differs
/// from `to`.
pub fn eval_coerced(ex: &mut Executor, e: &BoundExpr, row: &[Datum], to: TypeId) -> Result<Datum> {
    let value = eval(ex, e, row)?;
    if e.ty == to {
        return Ok(value);
    }
    coerce_assignment(ex, value, e.ty, to)
}

// ----------------------------------------------------------------------------------------------
// Subscripts

/// Evaluate an integer subscript: `Ok(None)` for a NULL value.
fn eval_index(ex: &mut Executor, e: &BoundExpr, row: &[Datum]) -> Result<Option<i32>> {
    match eval(ex, e, row)? {
        Datum::Null => Ok(None),
        Datum::Int4(i) => Ok(Some(i)),
        other => Err(type_mismatch("integer", &other)),
    }
}

/// One subscript position. `Ok(None)` means the position is NULL, which makes the whole result NULL.
fn subscript_position(ex: &mut Executor, item: &SubscriptItem, is_slice: bool, row: &[Datum]) -> Result<Option<Subscript>> {
    if !is_slice {
        let idx = match &item.lower {
            Some(e) => e,
            None => return Err(PgError::internal("array subscript without an index expression")),
        };
        return Ok(eval_index(ex, idx, row)?.map(Subscript::Index));
    }
    let lo = match &item.lower {
        Some(e) => match eval_index(ex, e, row)? {
            Some(v) => Some(v),
            None => return Ok(None),
        },
        None => None,
    };
    let hi = match &item.upper {
        Some(e) => match eval_index(ex, e, row)? {
            Some(v) => Some(v),
            None => return Ok(None),
        },
        None => None,
    };
    Ok(Some(Subscript::Slice(lo, hi)))
}

// ----------------------------------------------------------------------------------------------
// Domains

/// Name of a domain for error messages (`format_type_be` in PG; the unqualified type name here).
fn domain_name(ex: &Executor, domain: Oid) -> Result<String> {
    ex.se()
        .env
        .snapshot()
        .type_obj(domain)
        .map(|t| t.name.clone())
        .or_else(|| pgarbor_types::catalog_data::builtin_type(domain).map(|t| t.name.to_string()))
        .ok_or_else(|| PgError::internal(format!("domain type {domain} does not exist")))
}

/// Schema name of a domain for error fields (PG `errdatatype` sets `schema_name` from the type's namespace).
fn domain_schema(ex: &Executor, domain: Oid) -> Option<String> {
    let snap = ex.se().env.snapshot();
    let ns = snap.type_obj(domain)?.namespace;
    snap.namespace(ns).map(|n| n.name.clone())
}

/// Bound constraints of `domain`, from the per-execution cache or freshly bound and cached.
fn domain_rt(ex: &mut Executor, domain: Oid) -> Result<Rc<DomainRt>> {
    if let Some(found) = ex.rt.domains.get(&domain) {
        return Ok(found.clone());
    }
    let built = Rc::new(build_domain_rt(ex.se(), domain)?);
    ex.rt.domains.insert(domain, built.clone());
    Ok(built)
}

/// Collect the NOT NULL flags and bound CHECKs of `domain` and of every domain it is built on,
/// base-first (module docs).
fn build_domain_rt(se: &SessionEnv, domain: Oid) -> Result<DomainRt> {
    let snap = se.env.snapshot();
    // (domain oid, base type, not_null), outermost domain first.
    let mut levels: Vec<(Oid, TypeId, bool)> = Vec::new();
    let mut cur = domain;
    loop {
        if levels.len() >= MAX_DOMAIN_DEPTH {
            return Err(PgError::internal(format!("domain {domain} nests too deeply")));
        }
        let (base, not_null) = match snap.type_obj(cur) {
            Some(obj) => match &obj.def {
                TypeDef::Domain { base, not_null, .. } => (*base, *not_null),
                TypeDef::Enum { .. } => return Err(PgError::internal(format!("type {cur} is not a domain"))),
            },
            // Builtin information_schema domains (sql_identifier, character_data, ...) live in the
            // static type table, not in the user-type catalog; they carry no enforced NOT NULL.
            None => match pgarbor_types::catalog_data::builtin_type(cur) {
                Some(t) if t.typtype == b'd' => (TypeId::new(t.basetype, t.typmod), false),
                _ => return Err(PgError::internal(format!("domain type {cur} does not exist"))),
            },
        };
        levels.push((cur, base, not_null));
        let base_is_domain = snap.type_obj(base.oid).map_or(false, |t| t.is_domain());
        if !base_is_domain {
            break;
        }
        cur = base.oid;
    }

    let mut out = DomainRt { not_null: false, checks: Vec::new() };
    for &(dom, base, not_null) in levels.iter().rev() {
        if not_null {
            out.not_null = true;
        }
        // PG `load_domaintype_info` applies one domain's CHECKs in constraint-name order (`dcs_cmp`,
        // strcmp), so the first failing name is the one reported.
        let mut cons = snap.domain_constraints(dom);
        cons.sort_by(|a, b| a.name.cmp(&b.name));
        for con in cons {
            if con.kind != pgarbor_catalog::ConstraintKind::Check || !con.enforced {
                continue;
            }
            let stored = con
                .check
                .as_ref()
                .ok_or_else(|| PgError::internal(format!("check constraint {} has no expression", con.name)))?;
            let expr = bind_domain_check(se, base, &stored.sql)?;
            out.checks.push(DomainCheckRt { name: con.name.clone(), expr });
        }
    }
    Ok(out)
}

/// Parse and bind one stored CHECK text whose `VALUE` is a column of type `base`, then rewrite it to
/// read the value from `InputRef(0)`.
fn bind_domain_check(se: &SessionEnv, base: TypeId, sql: &str) -> Result<BoundExpr> {
    let ctx = BindContext {
        env: &se.env,
        session: BindSession { database: EngineServices::database_name(se.services) },
        io_config: se.io.clone(),
        statement_timestamp: se.statement_ts,
        declared_params: Vec::new(),
    };
    let src = SourceMap::new(sql);
    let mut b = Binder::new(&ctx, &src);
    table_expr_scope(&mut b, "domain", &[("value".to_string(), base)])?;
    let parsed = parse_expr_text(sql)?;
    let bound = bind_expr(&mut b, &parsed, ExprCtx::check())?;
    b.scopes.pop();
    columns_to_input_refs(bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecContext, NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use crate::runtime::Runtime;
    use pgarbor_catalog::testing;
    use pgarbor_sql::ir::{CoercionForm, ExprKind};
    use pgarbor_types::cast::plan_cast;
    use pgarbor_types::IoConfig;
    use std::cell::Cell;

    /// Run `f` over a fresh executor with a bootstrapped catalog and an in-memory transaction.
    fn run<T>(f: impl FnOnce(&mut Executor) -> Result<T>) -> Result<T> {
        let (_kv, env) = testing::bootstrapped()?;
        let mut txn = MemTxn::new();
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &NoGuc, &NoServices, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let mut rt = Runtime::default();
        let mut ex = Executor::new(&mut rt, &mut cx);
        f(&mut ex)
    }

    fn int4(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn int4_to_int8_cast() {
        run(|ex| {
            let plan = plan_cast(&ex.se().env, TypeId::INT4, TypeId::INT8, CastContext::Explicit)?;
            let c = CastExpr { arg: int4(7), plan, form: CoercionForm::Explicit };
            let d = eval_cast(ex, &c, TypeId::INT8, &[])?;
            assert!(matches!(d, Datum::Int8(7)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn text_to_int4_failure_message() {
        let err = run(|ex| {
            let plan = plan_cast(&ex.se().env, TypeId::TEXT, TypeId::INT4, CastContext::Explicit)?;
            let arg = BoundExpr::constant(Datum::Text("abc".to_string()), TypeId::TEXT);
            let c = CastExpr { arg, plan, form: CoercionForm::Explicit };
            eval_cast(ex, &c, TypeId::INT4, &[])
        })
        .unwrap_err();
        assert_eq!(err.sqlstate.code(), "22P02");
        assert_eq!(err.message, "invalid input syntax for type integer: \"abc\"");
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn array_element_and_slice() {
        run(|ex| {
            let elements = vec![int4(10), int4(20), int4(30)];
            let arr = ArrayExpr { elements, elem_type: TypeId::INT4.oid, multidims: false };
            let array = BoundExpr::new(ExprKind::Array(Box::new(arr)), TypeId::INT4_ARRAY);

            // ARRAY[10,20,30][2] = 20
            let element = SubscriptExpr {
                array: array.clone(),
                subscripts: vec![SubscriptItem { lower: Some(int4(2)), upper: None }],
                is_slice: false,
            };
            assert!(matches!(eval_subscript(ex, &element, TypeId::INT4, &[])?, Datum::Int4(20)));

            // ARRAY[10,20,30][5] = NULL
            let missing = SubscriptExpr {
                array: array.clone(),
                subscripts: vec![SubscriptItem { lower: Some(int4(5)), upper: None }],
                is_slice: false,
            };
            assert!(matches!(eval_subscript(ex, &missing, TypeId::INT4, &[])?, Datum::Null));

            // ARRAY[10,20,30][2:3] = {20,30}
            let slice = SubscriptExpr {
                array,
                subscripts: vec![SubscriptItem { lower: Some(int4(2)), upper: Some(int4(3)) }],
                is_slice: true,
            };
            match eval_subscript(ex, &slice, TypeId::INT4_ARRAY, &[])? {
                Datum::Array(a) => assert_eq!(a.items, vec![Datum::Int4(20), Datum::Int4(30)]),
                other => panic!("expected an array, got {other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "needs EXE-05"]
    fn row_field_select() {
        run(|ex| {
            let args = vec![
                int4(1),
                BoundExpr::constant(Datum::Text("x".to_string()), TypeId::TEXT),
            ];
            let row = RowExpr { args, names: vec!["a".to_string(), "b".to_string()] };
            let record = BoundExpr::new(ExprKind::Row(Box::new(row)), TypeId::of(2249));
            let sel = FieldSelect { arg: record, field: 2, field_name: "b".to_string() };
            match eval_field_select(ex, &sel, &[])? {
                Datum::Text(s) => assert_eq!(s, "x"),
                other => panic!("expected text, got {other:?}"),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn coerce_assignment_identity_needs_no_cast() {
        run(|ex| {
            let v = coerce_assignment(ex, Datum::Int4(3), TypeId::INT4, TypeId::INT4)?;
            assert!(matches!(v, Datum::Int4(3)));
            let n = coerce_assignment(ex, Datum::Null, TypeId::INT4, TypeId::INT8)?;
            assert!(matches!(n, Datum::Null));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn builtin_information_schema_domain_resolves() {
        run(|ex| {
            // information_schema.sql_identifier (13312), a builtin domain over `name`.
            assert_eq!(domain_name(ex, 13312)?, "sql_identifier");
            let rt = domain_rt(ex, 13312)?;
            assert!(!rt.not_null && rt.checks.is_empty());
            assert!(domain_rt(ex, 999_999).is_err());
            Ok(())
        })
        .unwrap();
    }
}
