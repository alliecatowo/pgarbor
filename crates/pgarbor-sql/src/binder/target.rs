//! SELECT / RETURNING target lists (SQL-19). PG references: `parse_target.c`
//! (`transformTargetList`, `transformTargetEntry`, `FigureColname`, `FigureColnameInternal`,
//! `ExpandColumnRefStar`, `ExpandAllTables`, `ExpandSingleTable`, `markTargetListOrigin`,
//! `resolveTargetListUnknowns` in `parse_clause`/`parse_target`).
//!
//! `bind_target_list(b, items, ec)` converts `Vec<SelectItem>` into `Vec<TargetEntry>` (never junk;
//! junk entries are added later by `order`/`group`):
//! * `UnnamedExpr(e)` -> `expr::bind_expr(e, ec)`; name from [`figure_colname`].
//! * `ExprWithAlias { expr, alias }` -> alias folded with `Binder::ident` (AS keyword optional).
//! * `Wildcard(opts)` -> `scope::expand_star(None)`; `QualifiedWildcard(name, opts)` ->
//!   `scope::expand_star(Some(name))`, or `expr_misc::expand_composite` when `name` is not a range
//!   entry but a composite column (`(t.c).*` is `Expr::...Wildcard`); options `EXCLUDE`, `EXCEPT`,
//!   `REPLACE`, `RENAME`, `ILIKE` (BigQuery/Snowflake): 42601. Each expansion yields one entry per
//!   visible column with the column's own name and origin.
//! * Set-returning functions in the list set `scope.has_target_srfs`; `SELECT` list items of
//!   composite type are NOT expanded unless written with `.*`.
//!
//! ## Output names ([`figure_colname`], PG `FigureColname`)
//! column ref -> column name (last identifier; `t.c` -> `c`); `t.*`/`*` n/a; function call -> function
//! name (last part; `count(*)` -> `count`); `CAST(x AS t)`/`x::t` -> the operand's name if it has
//! one, else the TYPE name (`select 1::int` -> `int4`; `select 'a'::text` -> `text`); `CASE` ->
//! `case`; `COALESCE` -> `coalesce`; `GREATEST` -> `greatest`; `LEAST` -> `least`; `NULLIF` ->
//! `nullif`; `EXISTS(..)` -> `exists`; `ARRAY[..]` and `ARRAY(select)` -> `array`; `ROW(..)` ->
//! `row`; `(x).field` -> `field`; subscript `a[1]` -> `a` (the root's name); `x IS NULL` and other
//! operators / literals / parameters -> `?column?`; `CURRENT_DATE` & co. -> their keyword lower-case
//! (`current_date`, `current_timestamp`, `current_user`->`current_user`, `session_user`, `user` ->
//! `user`, `current_catalog`, `current_schema`, `localtime`, `localtimestamp`); `EXTRACT(..)` ->
//! `extract`; `SUBSTRING` -> `substring`; `POSITION` -> `position`; `OVERLAY` -> `overlay`; `TRIM` ->
//! `btrim`/`ltrim`/`rtrim`; `x AT TIME ZONE z` -> `timezone`; `COLLATE` -> operand's name; sub-select
//! scalar `(select a)` -> the sub-select's FIRST output column name; `interval '1 day'` ->
//! `interval`; a typed literal `date '2020-01-01'` -> `date`; `GROUPING(..)` -> `grouping`; `x ->>
//! 'a'` -> `?column?`; boolean tests -> `?column?`; `NOT x` -> `?column?`; `TRUE` -> `bool`;
//! numeric/string literals -> `?column?`. Verify each against the oracle with `select <expr>` and
//! `\gdesc`-style column names before relying on a rule.
//!
//! ## Column origin (`markTargetListOrigin`)
//! If the bound expression (after stripping implicit relabel casts) is a `Column{levels_up: 0}`
//! referencing a plain relation range entry, `origin = ColumnOrigin{table: oid, attnum}`; a column of
//! a sub-select/view/CTE range entry resolves recursively to the underlying base column when that
//! output is itself a plain column reference (via [`origin_of`]); anything else `None`.
use pgarbor_common::Result;
use pgarbor_types::{oids, registry};
use sqlparser::ast::{
    AccessExpr, DataType, Expr, Ident, Query, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, TrimWhereField,
    Value, WildcardAdditionalOptions,
};

use super::context::{Binder, BoundArg, ExprCtx};
use super::{coerce, expr, expr_misc, scope, types};
use crate::diag;
use crate::ir::{BoundExpr, ColumnOrigin, CoercionForm, ExprKind, RangeKind, SubLinkKind, TargetEntry};
use crate::names::{fold_ident, truncate_ident};

/// Name strength pair of PG `FigureColnameInternal`: `(strength, name)`. Strength 2 = good name,
/// 1 = second-best (overridden by a cast's type name), 0 = no name.
type Fig = (u8, Option<String>);

/// Bind a SELECT list (or RETURNING list when `ec` = `ExprCtx::returning()`).
pub fn bind_target_list(b: &mut Binder, items: &[SelectItem], ec: ExprCtx) -> Result<Vec<TargetEntry>> {
    let mut out: Vec<TargetEntry> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::UnnamedExpr(e) => {
                let pos = b.pos(e);
                let bound = expr::bind_expr(b, e, ec)?;
                let name = figure_inner(b, e, Some(&bound))?.1.unwrap_or_else(|| "?column?".to_string());
                let bound = finish_output(b, bound, pos)?;
                note_srf(b, &bound)?;
                push_target(b, &mut out, bound, name)?;
            }
            SelectItem::ExprWithAlias { expr: e, alias } => {
                let pos = b.pos(e);
                let bound = expr::bind_expr(b, e, ec)?;
                let name = b.ident(alias);
                let bound = finish_output(b, bound, pos)?;
                note_srf(b, &bound)?;
                push_target(b, &mut out, bound, name)?;
            }
            SelectItem::ExprWithAliases { .. } => {
                let pos = b.pos(item);
                return Err(diag::syntax_error_near("(", pos));
            }
            SelectItem::Wildcard(opts) => {
                let pos = b.pos(item);
                check_wildcard_options(opts, pos)?;
                let cols = scope::expand_star(b, None, pos)?;
                push_star(b, &mut out, cols)?;
            }
            SelectItem::QualifiedWildcard(kind, opts) => {
                let pos = b.pos(item);
                check_wildcard_options(opts, pos)?;
                let cols = bind_qualified_star(b, kind, ec, pos)?;
                push_star(b, &mut out, cols)?;
            }
        }
    }
    Ok(out)
}

/// Default output column name of an expression (table above). Pure over the AST; `Binder` is only
/// needed to fold identifiers. Returns `"?column?"` when the rules give no name.
pub fn figure_colname(b: &mut Binder, e: &Expr) -> Result<String> {
    Ok(figure_inner(b, e, None)?.1.unwrap_or_else(|| "?column?".to_string()))
}

/// Origin of a bound expression (module docs).
pub fn origin_of(b: &Binder, e: &BoundExpr) -> Result<Option<ColumnOrigin>> {
    // Strip implicit relabel casts (no runtime effect, PG RelabelType).
    let mut cur = e;
    while let ExprKind::Cast(c) = &cur.kind {
        if c.form == CoercionForm::Implicit && c.plan.is_noop() {
            cur = &c.arg;
        } else {
            break;
        }
    }
    let cr = match &cur.kind {
        ExprKind::Column(cr) => *cr,
        _ => return Ok(None),
    };
    // Scope that owns the referenced range table (levels_up 0 = innermost).
    let idx = match b.scopes.len().checked_sub(cr.levels_up as usize + 1) {
        Some(i) => i,
        None => return Ok(None),
    };
    let rt_idx = match (cr.rt_index as usize).checked_sub(1) {
        Some(i) => i,
        None => return Ok(None),
    };
    let rte = match b.scopes[idx].range_table.get(rt_idx) {
        Some(r) => r,
        None => return Ok(None),
    };
    match &rte.kind {
        // Views are expanded to subqueries by the binder, so a relation is a table (or matview).
        RangeKind::Relation { oid, .. } => Ok(Some(ColumnOrigin { table: *oid, attnum: cr.attnum })),
        // Copy up from the sub-select's output column (PG: markTargetListOrigin RTE_SUBQUERY).
        RangeKind::Subquery { query } => {
            if cr.attnum < 1 {
                return Ok(None);
            }
            Ok(query.output.get((cr.attnum - 1) as usize).and_then(|c| c.origin))
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Target-list helpers
// ---------------------------------------------------------------------------------------------

/// Resolve a bound output expression of type `unknown` (`select 'a'`, `select null`) to text.
fn finish_output(b: &mut Binder, bound: BoundExpr, pos: Option<u32>) -> Result<BoundExpr> {
    if bound.ty.oid == oids::UNKNOWN {
        let arg = BoundArg::new(bound, pos);
        return coerce::resolve_unknown_output(b, &arg);
    }
    Ok(bound)
}

/// Mark the innermost scope when a target expression contains a set-returning function.
fn note_srf(b: &mut Binder, bound: &BoundExpr) -> Result<()> {
    if bound.contains_srf() {
        b.scope_mut()?.has_target_srfs = true;
    }
    Ok(())
}

fn push_target(b: &Binder, out: &mut Vec<TargetEntry>, bound: BoundExpr, name: String) -> Result<()> {
    let origin = origin_of(b, &bound)?;
    out.push(TargetEntry { expr: bound, name, junk: false, origin });
    Ok(())
}

fn push_star(b: &Binder, out: &mut Vec<TargetEntry>, cols: Vec<(String, BoundExpr)>) -> Result<()> {
    for (name, bound) in cols {
        push_target(b, out, bound, name)?;
    }
    Ok(())
}

/// Options of `*` other than the plain form are not PostgreSQL syntax: 42601 at the keyword.
fn check_wildcard_options(opts: &WildcardAdditionalOptions, pos: Option<u32>) -> Result<()> {
    let token = if opts.opt_ilike.is_some() {
        "ILIKE"
    } else if opts.opt_exclude.is_some() {
        "EXCLUDE"
    } else if opts.opt_except.is_some() {
        "EXCEPT"
    } else if opts.opt_replace.is_some() {
        "REPLACE"
    } else if opts.opt_rename.is_some() {
        "RENAME"
    } else {
        return Ok(());
    };
    Err(diag::syntax_error_near(token, pos))
}

/// `(expr).*`, `t.*`, `schema.t.*`, and `col.*` on a composite column (PG `ExpandColumnRefStar`):
/// a table name wins over a column of the same name; a non-table name that is a column expands
/// the composite; anything else reaches `expand_star` for the PG 42P01 error.
fn bind_qualified_star(
    b: &mut Binder,
    kind: &SelectItemQualifiedWildcardKind,
    ec: ExprCtx,
    pos: Option<u32>,
) -> Result<Vec<(String, BoundExpr)>> {
    match kind {
        SelectItemQualifiedWildcardKind::Expr(e) => {
            let bound = expr::bind_expr(b, e, ec)?;
            expr_misc::expand_composite(b, bound, pos)
        }
        SelectItemQualifiedWildcardKind::ObjectName(name) => {
            let q = b.qual(name)?;
            if let Some(cat) = q.catalog.as_deref() {
                if cat != b.ctx.session.database.as_str() {
                    let full = format!("{}.{}.{}", cat, q.schema.as_deref().unwrap_or(""), q.name);
                    return Err(diag::cross_database(&full, pos));
                }
            }
            match q.schema.clone() {
                None => {
                    if scope::find_relation_alias(b, &q.name)?.is_some() {
                        return scope::expand_star(b, Some(&q), pos);
                    }
                    match scope::resolve_column(b, &[], &q.name, pos, false)? {
                        Some(rc) => expand_composite_of(b, rc.expr, pos),
                        None => scope::expand_star(b, Some(&q), pos),
                    }
                }
                Some(schema) => {
                    if namespace_has_qualified(b, &schema, &q.name) {
                        return scope::expand_star(b, Some(&q), pos);
                    }
                    let qualifier = vec![schema];
                    match scope::resolve_column(b, &qualifier, &q.name, pos, false)? {
                        Some(rc) => expand_composite_of(b, rc.expr, pos),
                        None => scope::expand_star(b, Some(&q), pos),
                    }
                }
            }
        }
    }
}

fn expand_composite_of(b: &mut Binder, arg: BoundExpr, pos: Option<u32>) -> Result<Vec<(String, BoundExpr)>> {
    expr_misc::expand_composite(b, arg, pos)
}

/// Is `schema.table` a visible relation item of the innermost FROM namespace?
fn namespace_has_qualified(b: &Binder, schema: &str, table: &str) -> bool {
    match b.scopes.last() {
        Some(s) => s
            .namespace
            .iter()
            .any(|ns| ns.rel_visible && ns.alias.as_deref() == Some(table) && ns.schema.as_deref() == Some(schema)),
        None => false,
    }
}

// ---------------------------------------------------------------------------------------------
// FigureColname
// ---------------------------------------------------------------------------------------------

fn named(s: &str) -> Fig {
    (2, Some(s.to_string()))
}

/// Identifier as PG stores it: folded, then truncated to the name length limit (no NOTICE here;
/// the NOTICE is raised when the expression itself was bound).
fn plain_name(id: &Ident) -> String {
    let folded = fold_ident(id);
    truncate_ident(&folded).unwrap_or(folded)
}

/// PG `TypeName` last name for a cast or typed literal: the canonical `pg_type.typname` (`int` ->
/// `int4`, `varchar(3)` -> `varchar`, `int[]` -> `int4`, the element type for arrays).
fn type_colname(b: &mut Binder, dt: &DataType, pos: Option<u32>) -> Result<String> {
    let resolved = types::resolve_data_type(b, dt, pos)?;
    let mut oid = resolved.id.oid;
    if let Some(elem) = registry::element_type_of(b.env, oid) {
        oid = elem;
    }
    Ok(registry::type_info(b.env, oid)?.name.into_owned())
}

/// Recursive workhorse of `FigureColnameInternal`. `bound` is the bound form of THIS node when the
/// caller has it (used only for scalar sub-selects, whose first output name is known after binding).
fn figure_inner(b: &mut Binder, e: &Expr, bound: Option<&BoundExpr>) -> Result<Fig> {
    let fig: Fig = match e {
        Expr::Identifier(id) => named_or_none(plain_name(id)),
        Expr::CompoundIdentifier(parts) => match parts.last() {
            Some(id) => named_or_none(plain_name(id)),
            None => (0, None),
        },
        Expr::CompoundFieldAccess { root, access_chain } => {
            // A_Indirection: the last named field wins; subscripts are ignored; otherwise the root.
            let mut field: Option<String> = None;
            for acc in access_chain {
                if let AccessExpr::Dot(Expr::Identifier(id)) = acc {
                    field = Some(plain_name(id));
                }
            }
            match field {
                Some(f) => (2, Some(f)),
                None => figure_inner(b, root, None)?,
            }
        }
        // Parentheses are transparent in the grammar.
        Expr::Nested(inner) => figure_inner(b, inner, bound)?,
        Expr::Collate { expr: inner, .. } => figure_inner(b, inner, None)?,
        Expr::Cast { expr: operand, data_type, .. } => {
            let (strength, name) = figure_inner(b, operand, None)?;
            if strength <= 1 {
                let pos = b.pos(e);
                (1, Some(type_colname(b, data_type, pos)?))
            } else {
                (strength, name)
            }
        }
        Expr::Function(f) => match f.name.0.last().and_then(|p| p.as_ident()) {
            Some(id) => {
                let n = plain_name(id);
                // PG parses TRIM(x) as the function btrim.
                if n == "trim" {
                    named("btrim")
                } else {
                    named_or_none(n)
                }
            }
            None => (0, None),
        },
        Expr::Case { else_result, .. } => {
            let inner = match else_result {
                Some(x) => figure_inner(b, x, None)?,
                None => (0, None),
            };
            if inner.0 > 1 {
                inner
            } else {
                (1, Some("case".to_string()))
            }
        }
        Expr::Array(_) => named("array"),
        Expr::Exists { .. } => named("exists"),
        Expr::Tuple(_) => named("row"),
        Expr::Subquery(q) => {
            let from_bound = match bound.map(|x| &x.kind) {
                Some(ExprKind::SubLink(sl)) if matches!(sl.kind, SubLinkKind::Scalar) => {
                    sl.subquery.output.first().map(|c| c.name.clone())
                }
                _ => None,
            };
            let name = match from_bound {
                Some(n) => n,
                None => first_ast_output_name(b, q)?,
            };
            (2, Some(name))
        }
        Expr::Value(v) => match &v.value {
            // PG makes TRUE/FALSE a boolean constant whose type name is `bool`.
            Value::Boolean(_) => (1, Some("bool".to_string())),
            _ => (0, None),
        },
        Expr::TypedString(ts) => {
            let pos = b.pos(e);
            (1, Some(type_colname(b, &ts.data_type, pos)?))
        }
        Expr::Interval(_) => (1, Some("interval".to_string())),
        Expr::AtTimeZone { .. } => named("timezone"),
        Expr::Extract { .. } => named("extract"),
        Expr::Ceil { .. } => named("ceil"),
        Expr::Floor { .. } => named("floor"),
        Expr::Position { .. } => named("position"),
        Expr::Substring { .. } => named("substring"),
        Expr::Trim { trim_where, .. } => match trim_where {
            Some(TrimWhereField::Leading) => named("ltrim"),
            Some(TrimWhereField::Trailing) => named("rtrim"),
            _ => named("btrim"),
        },
        Expr::Overlay { .. } => named("overlay"),
        _ => (0, None),
    };
    Ok(fig)
}

fn named_or_none(name: String) -> Fig {
    (2, Some(name))
}

/// Name of the first output column of an untransformed sub-select (PG: the first target entry's
/// `resname`, which is always set, so it is `?column?` when nothing better applies).
fn first_ast_output_name(b: &mut Binder, q: &Query) -> Result<String> {
    let first = match q.body.as_ref() {
        SetExpr::Select(sel) => sel.projection.first(),
        _ => None,
    };
    let name = match first {
        Some(SelectItem::ExprWithAlias { alias, .. }) => plain_name(alias),
        Some(SelectItem::UnnamedExpr(x)) => figure_inner(b, x, None)?.1.unwrap_or_else(|| "?column?".to_string()),
        _ => "?column?".to_string(),
    };
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::ir::{
        AccessNeeded, BoundQuery, BoundSelect, OutputColumn, QueryBody, RangeColumn, RangeEntry,
    };
    use crate::binder::scope::{QueryScope, ScopeKind};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> Expr {
        Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
    }

    /// Output name of a stand-alone expression text (no catalog-dependent type rules).
    fn colname(sql: &str) -> String {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "pgarbor".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        figure_colname(&mut b, &parse_expr(sql)).unwrap()
    }

    #[test]
    fn names_of_column_references() {
        assert_eq!(colname("a"), "a");
        assert_eq!(colname("t.c"), "c");
        assert_eq!(colname("a[1]"), "a");
        assert_eq!(colname("(a)"), "a");
        assert_eq!(colname("a::text"), "a");
    }

    #[test]
    fn names_of_functions_and_keyword_forms() {
        assert_eq!(colname("count(*)"), "count");
        assert_eq!(colname("coalesce(a, 1)"), "coalesce");
        assert_eq!(colname("greatest(1, 2)"), "greatest");
        assert_eq!(colname("least(1, 2)"), "least");
        assert_eq!(colname("nullif(a, 1)"), "nullif");
        assert_eq!(colname("grouping(a)"), "grouping");
        assert_eq!(colname("exists(select 1)"), "exists");
        assert_eq!(colname("array[1, 2]"), "array");
        assert_eq!(colname("extract(year from a)"), "extract");
        assert_eq!(colname("substring(a from 1)"), "substring");
        assert_eq!(colname("position('a' in b)"), "position");
        assert_eq!(colname("overlay(a placing 'x' from 1)"), "overlay");
        assert_eq!(colname("trim(a)"), "btrim");
        assert_eq!(colname("trim(leading 'x' from a)"), "ltrim");
        assert_eq!(colname("a at time zone 'UTC'"), "timezone");
    }

    #[test]
    fn names_of_case_and_literals() {
        assert_eq!(colname("case when true then 1 end"), "case");
        assert_eq!(colname("case when true then 1 else a end"), "a");
        assert_eq!(colname("true"), "bool");
        assert_eq!(colname("1 + 1"), "?column?");
        assert_eq!(colname("a is null"), "?column?");
        assert_eq!(colname("not a"), "?column?");
        assert_eq!(colname("1"), "?column?");
        assert_eq!(colname("'x'"), "?column?");
    }

    #[test]
    fn names_of_scalar_subquery_use_first_output_column() {
        // Without a bound sub-select the AST fallback reads the first select item.
        assert_eq!(colname("(select a)"), "a");
        assert_eq!(colname("(select 1)"), "?column?");
    }

    #[test]
    #[ignore = "needs SQL-07"]
    fn names_of_casts_use_type_names() {
        assert_eq!(colname("1::int"), "int4");
        assert_eq!(colname("'a'::text"), "text");
        assert_eq!(colname("null::int"), "int4");
        assert_eq!(colname("date '2020-01-01'"), "date");
        assert_eq!(colname("interval '1 day'"), "interval");
    }

    #[test]
    fn origin_follows_subquery_output() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "pgarbor".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);

        let inner = BoundQuery {
            ctes: Vec::new(),
            body: QueryBody::Select(Box::new(BoundSelect::empty())),
            output: vec![OutputColumn {
                name: "a".to_string(),
                ty: TypeId::INT4,
                origin: Some(ColumnOrigin { table: 16400, attnum: 2 }),
            }],
            order_by: Vec::new(),
            limit: None,
            offset: None,
            limit_with_ties: false,
            locking: Vec::new(),
        };
        let mut scope_v = QueryScope::new(ScopeKind::Select);
        scope_v.range_table.push(RangeEntry {
            kind: RangeKind::Subquery { query: Box::new(inner) },
            alias: "v".to_string(),
            columns: vec![RangeColumn { name: "a".to_string(), ty: TypeId::INT4, dropped: false }],
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        });
        b.scopes.push(scope_v);

        let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
        assert_eq!(origin_of(&b, &col).unwrap(), Some(ColumnOrigin { table: 16400, attnum: 2 }));
        assert_eq!(origin_of(&b, &BoundExpr::bool_const(true)).unwrap(), None);
    }
}
