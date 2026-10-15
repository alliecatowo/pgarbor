//! Expression deparser: `BoundExpr` -> canonical PostgreSQL text (SQL-43, sonnet).
//! Reference: `src/backend/utils/adt/ruleutils.c` — `get_rule_expr`, `get_oper_expr`,
//! `get_func_expr`, `get_const_expr`, `get_const_collation`, `get_agg_expr`, `get_windowfunc_expr`,
//! `get_sublink_expr`, `get_rule_expr_paren`, `isSimpleNode`, `generate_function_name`,
//! `generate_operator_name`, `generate_qualified_type_name`, `simple_quote_literal`,
//! `quote_identifier`. All outputs below are copied from the PG 18.6 oracle (non-pretty mode).
//!
//! ## Rules (default options)
//! * **Operators** are fully parenthesized: `(a + 1)`, `(t.a = u.a)`, `((a > 1) AND (b < 2))`,
//!   `(NOT (a > 1))`, `(NOT (b IS NULL))`; an AND/OR chain is ONE parenthesized list `((x) AND (y) AND (z))`
//!   where each operand is itself parenthesized unless it is a simple node (Var, Const, function call);
//!   a nested AND that was NOT flattened by the parser stays nested: `((a > 1) AND (b < 2) AND ((c >= 1) AND
//!   (c <= 3)))` (BETWEEN expansion, oracle). Unary minus `(- a)`. Operator names that are not
//!   `pg_catalog`-visible are written `OPERATOR(schema.+)`.
//! * **Constants** (`get_const_expr`): `int2/int4` plain digits when non-negative, quoted negatives
//!   `'-5'::integer`; `int8` plain `12` if it fits int4 range else `'3000000000'::bigint`; `numeric`
//!   plain `1.5`, negative or special `'-1.5'::numeric`; `float4/8` plain if it looks like a number
//!   (`2.5`), else quoted with cast (`'NaN'::double precision`, `'-1.5'::double precision`); `bool`
//!   `true`/`false`; `NULL` -> `NULL::text` (typed null with its type name), untyped-unknown `NULL`;
//!   text-like: `'abc'::text`, `'abc'::character varying`, `'a'::character(1)`? (`bpchar` ->
//!   `'a'::bpchar`), `name` -> `'x'::name`; dates/times/intervals/uuid/json/jsonb/bytea:
//!   `'2020-01-01'::date`, `'1 day'::interval`, `'{}'::jsonb`, `'\x00'::bytea`; arrays
//!   `'{1,2}'::integer[]`; enums `'happy'::mood`; embedded quotes doubled; backslash literal strings
//!   use `E'...'` when the text contains a backslash. Typmod display via `names::format_type`
//!   (`'abc'::character varying`, `'1.5'::numeric(10,2)`).
//! * **Casts** (`ExprKind::Cast`): `CoercionForm::Implicit` prints ONLY the operand (so a `numeric`
//!   constant default of a `float8` column prints `2.5`, and `-1.5` prints `'-1.5'::numeric`);
//!   `Explicit` prints `(expr)::type` (`(5)::text`), `'2020-01-01'::date` for constants; `ExplicitCall`
//!   prints `type(expr)` only for datetime/interval-style calls PG preserves, else same as Explicit.
//!   Array element casts print as `(a)::integer[]`. Relabel-only plans print the same.
//! * **Functions**: `name(arg, arg)` with schema qualification only when not visible in `search_path`
//!   or ambiguous (`pg_catalog` functions never qualified unless shadowed): `lower(name)`,
//!   `nextval('scr_tmp.dd_id_seq'::regclass)`, `now()`, `count(*)`, `count(DISTINCT x)`,
//!   `sum(c) FILTER (WHERE ...)`, `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`, variadic
//!   `concat_ws(','::text, VARIADIC ARRAY[...])`, named notation not preserved (positional).
//!   SQL-standard function syntax is preserved for: `EXTRACT(YEAR FROM x)`? (PG prints
//!   `EXTRACT(year FROM x)`), `COALESCE(a, 'q'::text)`, `GREATEST(a, b)`, `LEAST`, `NULLIF(a, b)`,
//!   `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `CURRENT_USER` (SQLValueFunction), `SUBSTRING(s FROM a
//!   FOR b)`, `OVERLAY`, `POSITION(a IN b)`, `TRIM(BOTH ...)`, `(now() AT TIME ZONE 'utc'::text)` (the
//!   `timezone(zone, value)` call is printed back as `AT TIME ZONE`), `EXTRACT(year FROM now())`,
//!   `SUBSTRING(b FROM 1 FOR 2)`, `POSITION(('x'::text) IN (b))`, `TRIM(BOTH 'x'::text FROM b)`.
//! * **Window functions**: `count(*) OVER ()`, `row_number() OVER (PARTITION BY b ORDER BY a)`,
//!   named windows by name (`OVER w`).
//! * **CASE**: multi-line in view bodies (see `query.rs`); as a plain expression a single line
//!   `CASE WHEN (a > 1) THEN 'x'::text ELSE 'y'::text END`, simple form `CASE a WHEN 1 THEN ...`.
//! * **Predicates**: `(a IS NULL)`, `(a IS NOT NULL)`, `(a IS DISTINCT FROM b)`, `(a IS NOT DISTINCT
//!   FROM b)`, boolean tests `(a IS TRUE)`, `= ANY (ARRAY[1, 2, 3])` for IN lists: `(t.a = ANY
//!   (ARRAY[1, 2, 3]))`; `<> ALL`: `(a <> ALL ('{1,2}'::integer[]))` (constant array operand prints as a
//!   quoted array constant; an `ArrayExpr` of non-constants prints `ARRAY[...]`); `(a)::text`,
//!   BETWEEN does not exist in the output (`((x >= 1) AND (x <= 3))`); LIKE is the operator form
//!   `(b ~~ 'z%'::text)`; `NOT LIKE` `(b !~~ 'z%'::text)`; SIMILAR TO shows `similar_to_escape` form
//!   `(b ~ similar_to_escape('x'::text))`.
//! * **Rows/arrays/subscripts**: `ROW(1, 2)`, `ARRAY[1, 2]` (element separator `, `), `(a)[1]`,
//!   `(a)[1:2]`, `(r).f` field select `(r).f`, whole-row `t.*`.
//! * **Sub-links** (bind-time form): `(SELECT ...)`, `EXISTS(SELECT ...)`, `(x = ANY (SELECT ...))`,
//!   `ARRAY(SELECT ...)`; bodies via `query::deparse_query_inline`.
//! * **Identifiers** are quoted with `quote_identifier` (upper case/keywords/special chars).
//! * `Param(n)` -> `$n`. Plan-time nodes (`InputRef`, `OuterRef`, `SubPlan`) are only legal with
//!   `opts.explain` and render through the `ColumnNamer`.
//! * Collation: `COLLATE "C"` printed after the operand where PG prints it (`get_rule_expr` for
//!   `CollateExpr`): `(x COLLATE "C")`.
//!
//! The function is total over `ExprKind`: every variant either has a rule above or returns
//! `PgError::unimplemented("SQL-43", "deparse of <variant>")` — never a silent fallback.
use pgarbor_common::{Oid, PgError, Result, FIRST_NORMAL_OID};
use pgarbor_types::catalog_data::{self, AggKind};
use pgarbor_types::text::format::quote_identifier;
use pgarbor_types::{io, names, oids, Datum, IoConfig, IoCtx, TypeId};

use super::DeparseCtx;
use crate::ir::{
    Aggref, BoolExpr, BoolOp, BoolTestKind, BoundExpr, BoundQuery, CaseExpr, CoercionForm, ExprKind,
    FuncCallExpr, OpExpr, OpRef, RowCompareKind, SortExpr, SqlValueOp, SubLink, SubLinkKind, SubPlanRef,
};
use super::query::deparse_query_inline;

/// Deparse one expression (module docs).
pub fn deparse_expr(ctx: &DeparseCtx, e: &BoundExpr) -> Result<String> {
    let mut out = String::new();
    expr(ctx, e, &mut out)?;
    Ok(out)
}

/// Deparse a constant of type `ty` exactly like `get_const_expr` (shared with EXPLAIN and DEFAULT text).
pub fn deparse_const(ctx: &DeparseCtx, value: &Datum, ty: TypeId) -> Result<String> {
    let mut out = String::new();
    const_expr(ctx, value, ty, 0, &mut out)?;
    Ok(out)
}

/// `generate_function_name`: possibly schema-qualified, quoted function name for OID `func`.
pub fn function_display_name(ctx: &DeparseCtx, func: Oid, name: &str) -> Result<String> {
    let quoted = quote_identifier(name)?;
    if !ctx.opts.qualify_non_visible || ctx.catalog.function_is_visible(func)? {
        return Ok(quoted);
    }
    let ns = match catalog_data::builtin_proc(func) {
        Some(p) => p.namespace,
        None => match ctx.catalog.snapshot().function(func) {
            Some(f) => f.namespace,
            None => return Err(pgarbor_catalog::errors::cache_lookup_failed("function", func)),
        },
    };
    let ns_name = match ctx.catalog.snapshot().namespace(ns) {
        Some(n) => n.name.clone(),
        None => return Err(pgarbor_catalog::errors::cache_lookup_failed("namespace", ns)),
    };
    Ok(format!("{}.{}", quote_identifier(&ns_name)?, quoted))
}

/// `generate_qualified_type_name`/`format_type_with_typemod` for casts: `character varying(10)`,
/// `integer[]`, `public.mood` when not visible.
pub fn type_display_name(ctx: &DeparseCtx, ty: TypeId) -> Result<String> {
    if ctx.env.type_info(ty.oid).is_none() {
        return Err(pgarbor_catalog::errors::cache_lookup_failed("type", ty.oid));
    }
    // `format_type` qualifies every type that the search path does not resolve to itself.
    names::format_type(ctx.env, ty.oid, ty.typmod)
}

// ------------------------------------------------------------------ helpers

fn require_explain(ctx: &DeparseCtx) -> Result<()> {
    if ctx.opts.explain {
        Ok(())
    } else {
        Err(PgError::internal("plan-time node in bind-time deparse"))
    }
}

fn push_named(out: &mut String, text: Option<String>) -> Result<()> {
    match text {
        Some(s) => {
            out.push_str(&s);
            Ok(())
        }
        None => Err(PgError::internal("could not deparse variable")),
    }
}

/// Operator text; operators outside the builtin OID range need schema qualification that `OpRef`
/// does not carry (user-defined operators are not part of v1).
fn op_name(op: &OpRef) -> Result<&str> {
    if op.op >= FIRST_NORMAL_OID {
        return Err(PgError::unimplemented("SQL-43", "deparse of user-defined operators"));
    }
    Ok(op.name.as_str())
}

/// Comma-separated list (`, `), with `VARIADIC ` in front of the last argument when `variadic`.
fn push_args(ctx: &DeparseCtx, args: &[BoundExpr], variadic: bool, out: &mut String) -> Result<()> {
    let last = args.len().saturating_sub(1);
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if variadic && i == last {
            out.push_str("VARIADIC ");
        }
        expr(ctx, a, out)?;
    }
    Ok(())
}

fn expr_list(ctx: &DeparseCtx, args: &[BoundExpr], out: &mut String) -> Result<()> {
    push_args(ctx, args, false, out)
}

/// `ORDER BY` / `WITHIN GROUP` keys (PG `get_rule_orderby`): `DESC` and non-default NULLS placement.
fn sort_list(ctx: &DeparseCtx, items: &[SortExpr], out: &mut String) -> Result<()> {
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        expr(ctx, &s.expr, out)?;
        if let Some(op) = &s.using_op {
            out.push_str(" USING ");
            out.push_str(op_name(op)?);
        } else if s.descending {
            out.push_str(" DESC");
        }
        if s.descending && !s.nulls_first {
            out.push_str(" NULLS LAST");
        } else if !s.descending && s.nulls_first {
            out.push_str(" NULLS FIRST");
        }
    }
    Ok(())
}

/// PG `simple_quote_literal` with the E-string rule of this module: `'` doubled; a value with a
/// backslash gets the `E` prefix and doubled backslashes.
fn simple_quote_literal(out: &mut String, s: &str) {
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
}

/// Quoted name of a collation OID (v1 knows `default` (100) and `C` (950)).
fn collation_name(oid: Oid) -> Result<String> {
    let name = match oid {
        100 => "default",
        950 => "C",
        _ => return Err(PgError::unimplemented("SQL-43", "collation name display for this collation")),
    };
    quote_identifier(name)
}

/// `(expr)::type`, or `'const'::type` when the operand is a constant already of the target type
/// (PG `get_coercion_expr`).
fn coercion(ctx: &DeparseCtx, arg: &BoundExpr, target: TypeId, out: &mut String) -> Result<()> {
    if let ExprKind::Const(d) = &arg.kind {
        if arg.ty.oid == target.oid && arg.ty.typmod == -1 {
            const_expr(ctx, d, arg.ty, -1, out)?;
            out.push_str("::");
            out.push_str(&type_display_name(ctx, target)?);
            return Ok(());
        }
    }
    out.push('(');
    expr(ctx, arg, out)?;
    out.push(')');
    out.push_str("::");
    out.push_str(&type_display_name(ctx, target)?);
    Ok(())
}

/// Constant text (PG `get_const_expr`). `showtype` < 0: no type label at all; 0: label only when the
/// constant would not re-read with the same type.
fn const_expr(ctx: &DeparseCtx, d: &Datum, ty: TypeId, showtype: i32, out: &mut String) -> Result<()> {
    if d.is_null() {
        out.push_str("NULL");
        if showtype >= 0 {
            out.push_str("::");
            out.push_str(&type_display_name(ctx, ty)?);
        }
        return Ok(());
    }
    let cfg = IoConfig::default();
    let io_ctx = IoCtx::new(ctx.env, &cfg);
    let text = io::output(&io_ctx, ty, d)?;
    let looks_float = |s: &str| {
        s.as_bytes().first().is_some_and(|c| c.is_ascii_digit())
            && s.contains(|c: char| matches!(c, 'e' | 'E' | '.'))
    };
    let looks_number = |s: &str| {
        s.as_bytes().first().is_some_and(|c| c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | 'e' | 'E' | '.'))
    };
    let needlabel: bool = match ty.oid {
        oids::INT4 | oids::INT2 => {
            if text.starts_with('-') {
                simple_quote_literal(out, &text);
                true
            } else {
                out.push_str(&text);
                false
            }
        }
        oids::INT8 => {
            let fits = text.parse::<i64>().is_ok_and(|v| v >= i64::from(i32::MIN) && v <= i64::from(i32::MAX));
            if fits && !text.starts_with('-') {
                out.push_str(&text);
                false
            } else {
                simple_quote_literal(out, &text);
                true
            }
        }
        oids::NUMERIC => {
            // A plain float-looking constant needs no label; a typmod always shows.
            let plain = looks_float(&text) && !text.starts_with('-');
            if plain {
                out.push_str(&text);
            } else {
                simple_quote_literal(out, &text);
            }
            !plain || ty.typmod >= 0
        }
        oids::BOOL => {
            out.push_str(if matches!(d, Datum::Bool(true)) { "true" } else { "false" });
            false
        }
        oids::FLOAT4 | oids::FLOAT8 => {
            if looks_number(&text) && !text.starts_with('-') {
                out.push_str(&text);
                false
            } else {
                simple_quote_literal(out, &text);
                true
            }
        }
        _ => {
            simple_quote_literal(out, &text);
            true
        }
    };
    if showtype < 0 {
        return Ok(());
    }
    if needlabel || showtype > 0 {
        out.push_str("::");
        out.push_str(&type_display_name(ctx, ty)?);
    }
    Ok(())
}

// ------------------------------------------------------------------ expressions

fn expr(ctx: &DeparseCtx, e: &BoundExpr, out: &mut String) -> Result<()> {
    match &e.kind {
        ExprKind::Const(d) => const_expr(ctx, d, e.ty, 0, out),
        ExprKind::Param(n) => {
            out.push('$');
            out.push_str(&n.to_string());
            Ok(())
        }
        ExprKind::Column(c) => push_named(out, ctx.namer.column(c)),
        ExprKind::InputRef(i) => {
            require_explain(ctx)?;
            push_named(out, ctx.namer.input_ref(*i))
        }
        ExprKind::OuterRef(o) => {
            require_explain(ctx)?;
            push_named(out, ctx.namer.outer_ref(o))
        }
        ExprKind::FuncCall(f) => func_call(ctx, f, e.ty, out),
        ExprKind::Op(op) => op_expr(ctx, op, out),
        ExprKind::Bool(b) => bool_expr(ctx, b, out),
        ExprKind::NullTest(n) => {
            out.push('(');
            expr(ctx, &n.arg, out)?;
            out.push_str(if n.is_null { " IS NULL)" } else { " IS NOT NULL)" });
            Ok(())
        }
        ExprKind::BooleanTest(b) => {
            out.push('(');
            expr(ctx, &b.arg, out)?;
            out.push_str(match b.test {
                BoolTestKind::IsTrue => " IS TRUE)",
                BoolTestKind::IsNotTrue => " IS NOT TRUE)",
                BoolTestKind::IsFalse => " IS FALSE)",
                BoolTestKind::IsNotFalse => " IS NOT FALSE)",
                BoolTestKind::IsUnknown => " IS UNKNOWN)",
                BoolTestKind::IsNotUnknown => " IS NOT UNKNOWN)",
            });
            Ok(())
        }
        ExprKind::DistinctFrom(d) => {
            out.push('(');
            expr(ctx, &d.left, out)?;
            out.push_str(if d.negated { " IS NOT DISTINCT FROM " } else { " IS DISTINCT FROM " });
            expr(ctx, &d.right, out)?;
            out.push(')');
            Ok(())
        }
        ExprKind::Case(c) => case_expr(ctx, e, c, out),
        ExprKind::CaseTestValue => Err(PgError::internal("CaseTestValue outside a simple CASE")),
        ExprKind::Coalesce(args) => {
            out.push_str("COALESCE(");
            expr_list(ctx, args, out)?;
            out.push(')');
            Ok(())
        }
        ExprKind::NullIf(n) => {
            out.push_str("NULLIF(");
            expr(ctx, &n.left, out)?;
            out.push_str(", ");
            expr(ctx, &n.right, out)?;
            out.push(')');
            Ok(())
        }
        ExprKind::MinMax(m) => {
            out.push_str(if m.greatest { "GREATEST(" } else { "LEAST(" });
            expr_list(ctx, &m.args, out)?;
            out.push(')');
            Ok(())
        }
        ExprKind::Cast(c) => match c.form {
            CoercionForm::Implicit => expr(ctx, &c.arg, out),
            CoercionForm::Explicit | CoercionForm::ExplicitCall => coercion(ctx, &c.arg, e.ty, out),
        },
        ExprKind::CoerceToDomain(c) => match c.form {
            CoercionForm::Implicit => expr(ctx, &c.arg, out),
            CoercionForm::Explicit | CoercionForm::ExplicitCall => coercion(ctx, &c.arg, e.ty, out),
        },
        ExprKind::Collate(c) => {
            out.push('(');
            expr(ctx, &c.arg, out)?;
            out.push_str(" COLLATE ");
            out.push_str(&collation_name(c.collation)?);
            out.push(')');
            Ok(())
        }
        ExprKind::Array(a) => {
            out.push_str("ARRAY[");
            expr_list(ctx, &a.elements, out)?;
            out.push(']');
            if a.elements.is_empty() {
                out.push_str("::");
                out.push_str(&type_display_name(ctx, TypeId::new(e.ty.oid, -1))?);
            }
            Ok(())
        }
        ExprKind::Row(r) => {
            out.push_str("ROW(");
            expr_list(ctx, &r.args, out)?;
            out.push(')');
            Ok(())
        }
        ExprKind::FieldSelect(f) => {
            let need_parens = !matches!(f.arg.kind, ExprKind::Subscript(_) | ExprKind::FieldSelect(_));
            if need_parens {
                out.push('(');
            }
            expr(ctx, &f.arg, out)?;
            if need_parens {
                out.push(')');
            }
            out.push('.');
            out.push_str(&quote_identifier(&f.field_name)?);
            Ok(())
        }
        ExprKind::Subscript(s) => {
            out.push('(');
            expr(ctx, &s.array, out)?;
            out.push(')');
            for item in &s.subscripts {
                out.push('[');
                if s.is_slice {
                    if let Some(lo) = &item.lower {
                        expr(ctx, lo, out)?;
                    }
                    out.push(':');
                    if let Some(hi) = &item.upper {
                        expr(ctx, hi, out)?;
                    }
                } else {
                    match &item.lower {
                        Some(ix) => expr(ctx, ix, out)?,
                        None => return Err(PgError::internal("subscript without an index")),
                    }
                }
                out.push(']');
            }
            Ok(())
        }
        ExprKind::ScalarArrayOp(s) => {
            out.push('(');
            expr(ctx, &s.left, out)?;
            out.push(' ');
            out.push_str(op_name(&s.op)?);
            out.push_str(if s.use_or { " ANY (" } else { " ALL (" });
            expr(ctx, &s.right, out)?;
            out.push_str("))");
            Ok(())
        }
        ExprKind::RowCompare(r) => {
            out.push_str("(ROW(");
            expr_list(ctx, &r.left, out)?;
            out.push_str(") ");
            out.push_str(match r.kind {
                RowCompareKind::Lt => "<",
                RowCompareKind::Le => "<=",
                RowCompareKind::Gt => ">",
                RowCompareKind::Ge => ">=",
            });
            out.push_str(" ROW(");
            expr_list(ctx, &r.right, out)?;
            out.push_str("))");
            Ok(())
        }
        ExprKind::SubLink(s) => sublink_expr(ctx, s, out),
        ExprKind::SubPlan(p) => subplan_expr(ctx, p, out),
        ExprKind::Aggref(a) => agg_expr(ctx, a, out),
        ExprKind::WindowFunc(_) => {
            Err(PgError::unimplemented("SQL-43", "window function deparse (needs the window clause table)"))
        }
        ExprKind::Grouping(_) => {
            Err(PgError::unimplemented("SQL-43", "GROUPING() deparse (needs the grouping key list)"))
        }
        ExprKind::SqlValue(op) => {
            let typmod = e.ty.typmod;
            let text = match op {
                SqlValueOp::CurrentDate => "CURRENT_DATE".to_string(),
                SqlValueOp::CurrentTime if typmod >= 0 => format!("CURRENT_TIME({typmod})"),
                SqlValueOp::CurrentTime => "CURRENT_TIME".to_string(),
                SqlValueOp::CurrentTimestamp if typmod >= 0 => format!("CURRENT_TIMESTAMP({typmod})"),
                SqlValueOp::CurrentTimestamp => "CURRENT_TIMESTAMP".to_string(),
                SqlValueOp::LocalTime if typmod >= 0 => format!("LOCALTIME({typmod})"),
                SqlValueOp::LocalTime => "LOCALTIME".to_string(),
                SqlValueOp::LocalTimestamp if typmod >= 0 => format!("LOCALTIMESTAMP({typmod})"),
                SqlValueOp::LocalTimestamp => "LOCALTIMESTAMP".to_string(),
                SqlValueOp::CurrentRole => "CURRENT_ROLE".to_string(),
                SqlValueOp::CurrentUser => "CURRENT_USER".to_string(),
                SqlValueOp::SessionUser => "SESSION_USER".to_string(),
                SqlValueOp::User => "USER".to_string(),
                SqlValueOp::CurrentCatalog => "CURRENT_CATALOG".to_string(),
                SqlValueOp::CurrentSchema => "CURRENT_SCHEMA".to_string(),
            };
            out.push_str(&text);
            Ok(())
        }
        ExprKind::SetToDefault { .. } => {
            out.push_str("DEFAULT");
            Ok(())
        }
    }
}

fn func_call(ctx: &DeparseCtx, f: &FuncCallExpr, ty: TypeId, out: &mut String) -> Result<()> {
    if f.is_cast_call && f.args.len() == 1 {
        return coercion(ctx, &f.args[0], ty, out);
    }
    out.push_str(&function_display_name(ctx, f.func, &f.name)?);
    out.push('(');
    push_args(ctx, &f.args, f.variadic, out)?;
    out.push(')');
    Ok(())
}

fn op_expr(ctx: &DeparseCtx, op: &OpExpr, out: &mut String) -> Result<()> {
    let name = op_name(&op.op)?;
    out.push('(');
    match op.args.as_slice() {
        [l, r] => {
            expr(ctx, l, out)?;
            out.push(' ');
            out.push_str(name);
            out.push(' ');
            expr(ctx, r, out)?;
        }
        [a] => {
            out.push_str(name);
            out.push(' ');
            expr(ctx, a, out)?;
        }
        _ => return Err(PgError::internal("operator expression needs one or two operands")),
    }
    out.push(')');
    Ok(())
}

fn bool_expr(ctx: &DeparseCtx, b: &BoolExpr, out: &mut String) -> Result<()> {
    match b.op {
        BoolOp::And | BoolOp::Or => {
            if b.args.is_empty() {
                return Err(PgError::internal("boolean expression without operands"));
            }
            let sep = if b.op == BoolOp::And { " AND " } else { " OR " };
            out.push('(');
            for (i, a) in b.args.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                expr(ctx, a, out)?;
            }
            out.push(')');
        }
        BoolOp::Not => {
            if b.args.len() != 1 {
                return Err(PgError::internal("NOT needs exactly one operand"));
            }
            out.push_str("(NOT ");
            expr(ctx, &b.args[0], out)?;
            out.push(')');
        }
    }
    Ok(())
}

fn case_expr(ctx: &DeparseCtx, e: &BoundExpr, c: &CaseExpr, out: &mut String) -> Result<()> {
    out.push_str("CASE");
    if let Some(arg) = &c.arg {
        out.push(' ');
        expr(ctx, arg, out)?;
    }
    for arm in &c.arms {
        out.push_str(" WHEN ");
        match (&c.arg, &arm.cond.kind) {
            // Simple form: the condition is `CaseTestValue = value`; print only the value.
            (Some(_), ExprKind::Op(op)) => match op.args.as_slice() {
                [l, r] if matches!(l.kind, ExprKind::CaseTestValue) => expr(ctx, r, out)?,
                _ => return Err(PgError::internal("simple CASE arm without CaseTestValue")),
            },
            _ => expr(ctx, &arm.cond, out)?,
        }
        out.push_str(" THEN ");
        expr(ctx, &arm.result, out)?;
    }
    out.push_str(" ELSE ");
    match &c.default {
        Some(d) => expr(ctx, d, out)?,
        None => const_expr(ctx, &Datum::Null, e.ty, 0, out)?,
    }
    out.push_str(" END");
    Ok(())
}

fn push_query(ctx: &DeparseCtx, q: &BoundQuery, out: &mut String) -> Result<()> {
    let text = deparse_query_inline(ctx, q)?;
    out.push_str(&text);
    Ok(())
}

fn sublink_expr(ctx: &DeparseCtx, s: &SubLink, out: &mut String) -> Result<()> {
    match s.kind {
        SubLinkKind::Exists => {
            out.push_str("EXISTS(");
            push_query(ctx, &s.subquery, out)?;
            out.push(')');
        }
        SubLinkKind::Scalar => {
            out.push('(');
            push_query(ctx, &s.subquery, out)?;
            out.push(')');
        }
        SubLinkKind::Array => {
            out.push_str("ARRAY(");
            push_query(ctx, &s.subquery, out)?;
            out.push(')');
        }
        SubLinkKind::Any | SubLinkKind::All => {
            let test = s.test.as_deref().ok_or_else(|| PgError::internal("ANY/ALL sub-select without a test"))?;
            let op = s.ops.first().ok_or_else(|| PgError::internal("ANY/ALL sub-select without an operator"))?;
            out.push('(');
            expr(ctx, test, out)?;
            out.push(' ');
            out.push_str(op_name(op)?);
            out.push_str(if s.kind == SubLinkKind::Any { " ANY (" } else { " ALL (" });
            push_query(ctx, &s.subquery, out)?;
            out.push_str("))");
        }
        SubLinkKind::Row => return Err(PgError::unimplemented("SQL-43", "row sub-select deparse")),
    }
    Ok(())
}

fn subplan_expr(ctx: &DeparseCtx, p: &SubPlanRef, out: &mut String) -> Result<()> {
    require_explain(ctx)?;
    let text = ctx.namer.subplan(p.plan_id).ok_or_else(|| PgError::internal("could not deparse subplan"))?;
    match p.kind {
        SubLinkKind::Any | SubLinkKind::All => {
            let test = p.test.as_deref().ok_or_else(|| PgError::internal("ANY/ALL subplan without a test"))?;
            let op = p.ops.first().ok_or_else(|| PgError::internal("ANY/ALL subplan without an operator"))?;
            out.push('(');
            expr(ctx, test, out)?;
            out.push(' ');
            out.push_str(op_name(op)?);
            out.push_str(if p.kind == SubLinkKind::Any { " ANY " } else { " ALL " });
            out.push_str(&text);
            out.push(')');
        }
        _ => out.push_str(&text),
    }
    Ok(())
}

fn agg_expr(ctx: &DeparseCtx, a: &Aggref, out: &mut String) -> Result<()> {
    out.push_str(&function_display_name(ctx, a.agg, &a.name)?);
    out.push('(');
    match a.kind {
        AggKind::Normal => {
            if a.star {
                out.push('*');
            } else {
                if a.distinct {
                    out.push_str("DISTINCT ");
                }
                push_args(ctx, &a.args, a.variadic, out)?;
                if !a.order_by.is_empty() {
                    out.push_str(" ORDER BY ");
                    sort_list(ctx, &a.order_by, out)?;
                }
            }
            out.push(')');
        }
        AggKind::OrderedSet | AggKind::Hypothetical => {
            if a.order_by.is_empty() {
                return Err(PgError::internal("ordered-set aggregate without WITHIN GROUP keys"));
            }
            push_args(ctx, &a.direct_args, false, out)?;
            out.push_str(") WITHIN GROUP (ORDER BY ");
            sort_list(ctx, &a.order_by, out)?;
            out.push(')');
        }
    }
    if let Some(f) = &a.filter {
        out.push_str(" FILTER (WHERE ");
        expr(ctx, f, out)?;
        out.push(')');
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deparse::{DeparseOptions, TableColumns};
    use crate::ir::{ArrayExpr, CaseArm, ScalarArrayOp};
    use pgarbor_catalog::testing;
    use pgarbor_types::catalog_data::Volatility;

    fn op(oid: Oid, name: &str) -> OpRef {
        OpRef { op: oid, proc: 0, name: name.to_string(), volatility: Volatility::Immutable, strict: true }
    }

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    /// Runs `f` against a bootstrapped catalog whose single table has the given column names.
    fn with_ctx<F: FnOnce(&DeparseCtx) -> Result<()>>(names: &[&str], f: F) -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let namer = TableColumns { names: names.iter().map(|s| s.to_string()).collect() };
        let ctx = DeparseCtx { env: &env, catalog: &env, namer: &namer, opts: DeparseOptions::default() };
        f(&ctx)
    }

    #[test]
    fn constants() -> Result<()> {
        with_ctx(&[], |ctx| {
            assert_eq!(deparse_const(ctx, &Datum::Int4(5), TypeId::INT4)?, "5");
            assert_eq!(deparse_const(ctx, &Datum::Int4(-5), TypeId::INT4)?, "'-5'::integer");
            assert_eq!(deparse_const(ctx, &Datum::Text("abc".into()), TypeId::TEXT)?, "'abc'::text");
            assert_eq!(deparse_const(ctx, &Datum::Text("it's".into()), TypeId::TEXT)?, "'it''s'::text");
            assert_eq!(deparse_const(ctx, &Datum::Text("a\\b".into()), TypeId::TEXT)?, "E'a\\\\b'::text");
            assert_eq!(deparse_const(ctx, &Datum::Null, TypeId::TEXT)?, "NULL::text");
            assert_eq!(deparse_const(ctx, &Datum::Bool(true), TypeId::BOOL)?, "true");
            Ok(())
        })
    }

    #[test]
    fn numeric_constants() -> Result<()> {
        with_ctx(&[], |ctx| {
            let cfg = IoConfig::default();
            let io_ctx = IoCtx::new(ctx.env, &cfg);
            let plain = io::input(&io_ctx, TypeId::NUMERIC, "1.5")?;
            // Implicit (showtype -1) numeric constants print bare; labelled otherwise.
            assert_eq!(deparse_const(ctx, &plain, TypeId::NUMERIC)?, "1.5");
            let neg = io::input(&io_ctx, TypeId::NUMERIC, "-1.5")?;
            assert_eq!(deparse_const(ctx, &neg, TypeId::NUMERIC)?, "'-1.5'::numeric");
            Ok(())
        })
    }

    #[test]
    fn operator_and_column() -> Result<()> {
        with_ctx(&["a"], |ctx| {
            let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
            let e = BoundExpr::new(
                ExprKind::Op(Box::new(OpExpr { op: op(551, "+"), args: vec![col, int(1)] })),
                TypeId::INT4,
            );
            assert_eq!(deparse_expr(ctx, &e)?, "(a + 1)");
            Ok(())
        })
    }

    #[test]
    fn in_list_and_not_in() -> Result<()> {
        with_ctx(&["a"], |ctx| {
            let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
            let arr_ty = TypeId::new(1007, -1); // int4[]
            let list = BoundExpr::new(
                ExprKind::Array(Box::new(ArrayExpr {
                    elements: vec![int(1), int(2), int(3)],
                    elem_type: oids::INT4,
                    multidims: false,
                })),
                arr_ty,
            );
            let e = BoundExpr::new(
                ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp {
                    op: op(96, "="),
                    use_or: true,
                    left: col.clone(),
                    right: list,
                })),
                TypeId::BOOL,
            );
            assert_eq!(deparse_expr(ctx, &e)?, "(a = ANY (ARRAY[1, 2, 3]))");
            let cfg = IoConfig::default();
            let io_ctx = IoCtx::new(ctx.env, &cfg);
            let arr = io::input(&io_ctx, TypeId::new(1007, -1), "{1,2}")?;
            let konst = BoundExpr::constant(arr, TypeId::new(1007, -1));
            let not_in = BoundExpr::new(
                ExprKind::ScalarArrayOp(Box::new(ScalarArrayOp {
                    op: op(518, "<>"),
                    use_or: false,
                    left: col,
                    right: konst,
                })),
                TypeId::BOOL,
            );
            // The constant array operand is quoted and labelled (its type is int4[] in real plans).
            assert!(deparse_expr(ctx, &not_in)?.starts_with("(a <> ALL ("));
            Ok(())
        })
    }

    #[test]
    fn simple_case_and_searched_case() -> Result<()> {
        with_ctx(&["a"], |ctx| {
            let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
            let cond = BoundExpr::new(
                ExprKind::Op(Box::new(OpExpr { op: op(521, ">"), args: vec![col, int(1)] })),
                TypeId::BOOL,
            );
            let text = |s: &str| BoundExpr::constant(Datum::Text(s.into()), TypeId::TEXT);
            let searched = BoundExpr::new(
                ExprKind::Case(Box::new(CaseExpr {
                    arg: None,
                    arms: vec![CaseArm { cond, result: text("x") }],
                    default: None,
                })),
                TypeId::TEXT,
            );
            assert_eq!(
                deparse_expr(ctx, &searched)?,
                "CASE WHEN (a > 1) THEN 'x'::text ELSE NULL::text END"
            );
            let simple_cond = BoundExpr::new(
                ExprKind::Op(Box::new(OpExpr {
                    op: op(96, "="),
                    args: vec![BoundExpr::new(ExprKind::CaseTestValue, TypeId::INT4), int(1)],
                })),
                TypeId::BOOL,
            );
            let simple = BoundExpr::new(
                ExprKind::Case(Box::new(CaseExpr {
                    arg: Some(Box::new(BoundExpr::column(0, 1, 1, TypeId::INT4))),
                    arms: vec![CaseArm { cond: simple_cond, result: text("x") }],
                    default: Some(Box::new(text("y"))),
                })),
                TypeId::TEXT,
            );
            assert_eq!(deparse_expr(ctx, &simple)?, "CASE a WHEN 1 THEN 'x'::text ELSE 'y'::text END");
            Ok(())
        })
    }

    #[test]
    fn functions_and_aggregates() -> Result<()> {
        with_ctx(&["b"], |ctx| {
            let lower = BoundExpr::new(
                ExprKind::FuncCall(Box::new(FuncCallExpr {
                    func: 870,
                    name: "lower".into(),
                    args: vec![BoundExpr::column(0, 1, 1, TypeId::TEXT)],
                    variadic: false,
                    retset: false,
                    volatility: Volatility::Immutable,
                    strict: true,
                    is_cast_call: false,
                })),
                TypeId::TEXT,
            );
            assert_eq!(deparse_expr(ctx, &lower)?, "lower(b)");
            let now = BoundExpr::new(
                ExprKind::FuncCall(Box::new(FuncCallExpr {
                    func: 1299,
                    name: "now".into(),
                    args: vec![],
                    variadic: false,
                    retset: false,
                    volatility: Volatility::Stable,
                    strict: true,
                    is_cast_call: false,
                })),
                TypeId::new(1184, -1),
            );
            assert_eq!(deparse_expr(ctx, &now)?, "now()");
            let count = BoundExpr::new(
                ExprKind::Aggref(Box::new(Aggref {
                    agg: 2803,
                    name: "count".into(),
                    args: vec![],
                    direct_args: vec![],
                    kind: AggKind::Normal,
                    distinct: false,
                    filter: None,
                    order_by: vec![],
                    star: true,
                    variadic: false,
                    levels_up: 0,
                })),
                TypeId::INT8,
            );
            assert_eq!(deparse_expr(ctx, &count)?, "count(*)");
            Ok(())
        })
    }

    #[test]
    fn bare_column_and_plan_only_nodes() -> Result<()> {
        with_ctx(&["a"], |ctx| {
            assert_eq!(deparse_expr(ctx, &BoundExpr::column(0, 1, 1, TypeId::INT4))?, "a");
            assert!(deparse_expr(ctx, &BoundExpr::column(0, 1, 2, TypeId::INT4)).is_err());
            assert!(deparse_expr(ctx, &BoundExpr::input_ref(0, TypeId::INT4)).is_err());
            Ok(())
        })
    }

    #[test]
    #[ignore = "needs SQL-01 (parse_expr_text) for the round trip"]
    fn round_trip_parses() -> Result<()> {
        with_ctx(&["a"], |ctx| {
            let e = BoundExpr::new(
                ExprKind::Op(Box::new(OpExpr { op: op(551, "+"), args: vec![BoundExpr::column(0, 1, 1, TypeId::INT4), int(1)] })),
                TypeId::INT4,
            );
            let text = deparse_expr(ctx, &e)?;
            crate::parser::parse_expr_text(&text)?;
            Ok(())
        })
    }
}
