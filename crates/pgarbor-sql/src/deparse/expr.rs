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
use pgarbor_common::{PgError, Result};
use pgarbor_types::TypeId;

use super::DeparseCtx;
use crate::ir::BoundExpr;

/// Deparse one expression (module docs).
#[allow(unused_variables)]
pub fn deparse_expr(ctx: &DeparseCtx, e: &BoundExpr) -> Result<String> {
    Err(PgError::unimplemented("SQL-43", "expression deparse (deparse::expr::deparse_expr)"))
}

/// Deparse a constant of type `ty` exactly like `get_const_expr` (shared with EXPLAIN and DEFAULT text).
#[allow(unused_variables)]
pub fn deparse_const(ctx: &DeparseCtx, value: &pgarbor_types::Datum, ty: TypeId) -> Result<String> {
    Err(PgError::unimplemented("SQL-43", "constant deparse (deparse::expr::deparse_const)"))
}

/// `generate_function_name`: possibly schema-qualified, quoted function name for OID `func`.
#[allow(unused_variables)]
pub fn function_display_name(ctx: &DeparseCtx, func: pgarbor_common::Oid, name: &str) -> Result<String> {
    Err(PgError::unimplemented("SQL-43", "function name display (deparse::expr::function_display_name)"))
}

/// `generate_qualified_type_name`/`format_type_with_typemod` for casts: `character varying(10)`,
/// `integer[]`, `public.mood` when not visible.
#[allow(unused_variables)]
pub fn type_display_name(ctx: &DeparseCtx, ty: TypeId) -> Result<String> {
    Err(PgError::unimplemented("SQL-43", "type name display (deparse::expr::type_display_name)"))
}
