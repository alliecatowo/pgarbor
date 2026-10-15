//! FROM clause items: relations, views, sub-selects, table functions, VALUES (SQL-17).
//! PG references: `parse_clause.c` (`transformFromClause`, `transformFromClauseItem`,
//! `transformTableEntry`, `transformRangeSubselect`, `transformRangeFunction`,
//! `transformRangeTableFunc`), `parse_relation.c` (`addRangeTableEntry*`, `buildNSItemFromLists`,
//! `chooseScalarFunctionAlias`, `addRangeTableEntryForFunction`), `rewriteHandler.c`
//! (`ApplyRetrieveRule`: views are expanded by re-parsing their stored definition).
//!
//! `bind_from_clause(b, from)` processes `Vec<TableWithJoins>` (comma-separated items, each a
//! `relation` plus `joins`). For each item: `bind_table_factor` for the relation, then
//! `join::bind_join` for every join in order. Items are cross-joined implicitly (they become
//! separate entries of `BoundSelect::from`). The namespace is reset so that items of the SAME FROM
//! list do not see each other except through LATERAL (`NsItem::lateral_ok`).
//!
//! ## Table factors (sqlparser `TableFactor`)
//! * `Table { name, alias, args: None, with_hints, version, partitions, with_ordinality, sample, .. }`
//!   - resolution order: unqualified name -> CTE (`cte::find_cte`) first, then catalog relation via
//!     `env.resolve_relation(schema, name)` (42P01 `relation "x" does not exist`, position = name);
//!   - the catalog `Relation`: table/matview/sequence/virtual -> `RangeKind::Relation` (columns from
//!     `Relation::columns`, dropped ones flagged); `RelKind::View` -> expand: `parser::parse_one(view
//!     .query.sql)` + `select::bind_subselect` with the view's column names as aliases, produce
//!     `RangeKind::Subquery` (guard recursion with `Binder::expanding_views`, 42P17 `infinite recursion detected in rules for relation "v"`);
//!     composite type/index/foreign table as FROM item -> 42809 `"x" is not a table or view`?
//!     (PG: `cannot select from index "i"`? -> use 42809 `"i" is an index`);
//!   - `ONLY t` sets `only`; `t*` fine; `TABLESAMPLE` -> 0A000; `WITH (hint)`, `VERSION`, `PARTITION (..)`
//!     (MySQL), `INDEXED BY`: 42601;
//!   - alias: `AS a` / `a` plus optional column alias list `a(c1, c2)` (fewer aliases than columns is
//!     fine, more -> 42P10 `table "t" has 2 columns available but 3 columns specified`,
//!     `diag::too_many_column_aliases`); alias folding via `Binder::ident`. No alias -> relation
//!     name is the qualifier (unqualified; `schema.t` can still be referenced as `schema.t.col`).
//!   - `AccessNeeded.select = true` for the entry (columns added as referenced).
//! * `Table { args: Some(..) }` / `Function { lateral, name, args, alias, with_ordinality }` = table
//!   function: each function expression is bound with `ExprCtx::function_in_from()`; allowed
//!   LATERAL references to earlier FROM items (implicitly lateral). Result columns: scalar SRF ->
//!   one column named after the alias, else the function name (`generate_series`), type = return
//!   type; composite/OUT-parameter functions (`BuiltinProc::out_columns`) -> their columns;
//!   `AS t(a int, b text)` column definition lists (sqlparser: alias columns carry `data_type`)
//!   for `record`-returning functions; `WITH ORDINALITY` adds a trailing `ordinality bigint` column;
//!   `ROWS FROM (f(), g())` = `TableFactor::TableFunction`? (check 0.63: multi-function); a non-set
//!   function in FROM is allowed (one row). Builtin examples: `generate_series(1,3)`,
//!   `generate_series(ts, ts, interval)`, `unnest(array[1,2])`, `unnest(a, b)` (multi-array form),
//!   `jsonb_array_elements`, `jsonb_each` (key,value), `regexp_matches`, `string_to_table`.
//!   Aggregate or window function in FROM: 42809? (`aggregate functions are not allowed in FROM clause
//!   of their own query level` 42803).
//! * `UNNEST { alias, array_exprs, with_offset, with_ordinality }` -> same as `unnest(..)` function.
//! * `Derived { lateral, subquery, alias }` -> `select::bind_subselect`; `RangeKind::Subquery`;
//!   alias REQUIRED in PG < 16, optional in PG 16+: no alias gets the generated name `unnamed_subquery`
//!   (which cannot be referenced); duplicate output names allowed. `LATERAL` lets the sub-select reference
//!   earlier items (`lateral_ok`); non-lateral subqueries see only OUTER scopes.
//! * `Values` as `Derived` with `SetExpr::Values` -> `values::bind_values_body`, `RangeKind::Values`,
//!   columns `column1, column2, ...` unless aliased; alias `*VALUES*` when absent.
//! * `NestedJoin { table_with_joins, alias }` -> bind the inner joins (parenthesized join); alias on a
//!   nested join names the joined row (rare; support without alias, 0A000 with alias).
//! * `JsonTable`, `OpenJsonTable`, `Pivot`, `Unpivot`, `TableFunction` (non-PG), `XmlTable`,
//!   `SemanticView`, `MatchRecognize`, `Unnest` with offset: 0A000/42601 naming the construct.
//!
//! ## Helpers
//! [`relation_range_entry`] builds the `RangeEntry` for a catalog relation (also used by DML
//! targets and `INSERT ... ON CONFLICT excluded`). [`apply_column_aliases`] applies alias lists.
use pgarbor_catalog::model::RelKind;
use pgarbor_catalog::{CatalogEnv, Relation};
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::catalog_data::builtin_proc;
use pgarbor_types::{oids, TypeEnv, TypeId};
use sqlparser::ast::{
    Function, FunctionArg, FunctionArgExpr, FunctionArgumentList, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Query, SetExpr, Statement, TableAlias, TableFactor, TableWithJoins,
};

use super::context::{Binder, ExprCtx};
use super::{cte, expr_func, join, scope, select, types, values};
use crate::diag;
use crate::ir::{AccessNeeded, ExprKind, FromItem, OutputColumn, RangeColumn, RangeEntry, RangeFunction, RangeKind};
use crate::names::QualName;
use crate::parser;

/// Bind the whole FROM clause, returning the top-level (implicitly cross-joined) items.
///
/// The namespace index where this FROM list starts is kept in `QueryScope::from_depth`; items at
/// or after it were added by this list, and only LATERAL items (and implicitly lateral function
/// calls) may reference them. The previous value is restored on exit.
pub fn bind_from_clause(b: &mut Binder, from: &[TableWithJoins]) -> Result<Vec<FromItem>> {
    if from.is_empty() {
        return Ok(Vec::new());
    }
    let base = b.scope()?.namespace.len() as u32;
    let prev = b.scope()?.from_depth;
    b.scope_mut()?.from_depth = base;
    let result = bind_from_items(b, from);
    if let Ok(sc) = b.scope_mut() {
        sc.from_depth = prev;
    }
    result
}

/// Items of a FROM list: each comma item is a factor followed by its joins, in order.
fn bind_from_items(b: &mut Binder, from: &[TableWithJoins]) -> Result<Vec<FromItem>> {
    let mut out = Vec::with_capacity(from.len());
    for twj in from {
        let mut item = bind_table_factor(b, &twj.relation, true)?;
        for j in &twj.joins {
            item = join::bind_join(b, item, j)?;
        }
        out.push(item);
    }
    Ok(out)
}

/// Bind one table factor (without joins); registers range entry + namespace item in the innermost
/// scope and returns `FromItem::Range(rt_index)` (or the inner join tree for `NestedJoin`).
pub fn bind_table_factor(b: &mut Binder, tf: &TableFactor, lateral_ok: bool) -> Result<FromItem> {
    match tf {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
            ..
        } => {
            let pos = b.pos(name);
            if sample.is_some() {
                return Err(diag::unsupported_construct("TABLESAMPLE", pos));
            }
            if !with_hints.is_empty() {
                return Err(diag::syntax_error_near("WITH", pos));
            }
            if version.is_some() {
                return Err(diag::syntax_error_near("FOR", pos));
            }
            if !partitions.is_empty() {
                return Err(diag::syntax_error_near("PARTITION", pos));
            }
            if !index_hints.is_empty() {
                return Err(diag::syntax_error_near("USE", pos));
            }
            if json_path.is_some() {
                return Err(diag::unsupported_construct("JSON path in FROM", pos));
            }
            match args {
                Some(fa) => {
                    if fa.settings.is_some() {
                        return Err(diag::unsupported_construct("SETTINGS in a table function", pos));
                    }
                    bind_function_factor(b, name, fa.args.clone(), *with_ordinality, alias.as_ref())
                }
                None => {
                    if *with_ordinality {
                        return Err(diag::syntax_error_near("WITH", pos));
                    }
                    bind_named_table(b, name, alias.as_ref())
                }
            }
        }
        TableFactor::Function { name, args, with_ordinality, alias, .. } => {
            bind_function_factor(b, name, args.clone(), *with_ordinality, alias.as_ref())
        }
        TableFactor::UNNEST { alias, array_exprs, with_offset, with_offset_alias, with_ordinality } => {
            if *with_offset || with_offset_alias.is_some() {
                return Err(diag::unsupported_construct("UNNEST ... WITH OFFSET", None));
            }
            let args: Vec<FunctionArg> = array_exprs
                .iter()
                .map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e.clone())))
                .collect();
            let name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new("unnest"))]);
            bind_function_factor(b, &name, args, *with_ordinality, alias.as_ref())
        }
        TableFactor::Derived { lateral, subquery, alias, sample } => {
            if sample.is_some() {
                return Err(diag::unsupported_construct("TABLESAMPLE", None));
            }
            let lateral_here = *lateral && lateral_ok;
            hide_earlier_items(b, !lateral_here);
            let result = bind_derived(b, subquery, lateral_here, alias.as_ref());
            hide_earlier_items(b, false);
            result
        }
        TableFactor::NestedJoin { table_with_joins, alias } => {
            if alias.is_some() {
                return Err(diag::unsupported_construct("alias on a parenthesized join", None));
            }
            let mut item = bind_table_factor(b, &table_with_joins.relation, lateral_ok)?;
            for j in &table_with_joins.joins {
                item = join::bind_join(b, item, j)?;
            }
            Ok(item)
        }
        other => {
            let pos = b.pos(other);
            let what = match other {
                TableFactor::JsonTable { .. } => "JSON_TABLE",
                TableFactor::OpenJsonTable { .. } => "OPENJSON",
                TableFactor::Pivot { .. } => "PIVOT",
                TableFactor::Unpivot { .. } => "UNPIVOT",
                TableFactor::MatchRecognize { .. } => "MATCH_RECOGNIZE",
                TableFactor::TableFunction { .. } => "TABLE() function in FROM",
                _ => "this FROM clause item",
            };
            Err(diag::unsupported_construct(what, pos))
        }
    }
}

/// `RangeEntry` for a catalog relation (not a view): columns = all `Relation::columns` (dropped
/// flagged), `alias` = alias or relation name, empty `AccessNeeded` with `select = true`.
pub fn relation_range_entry(b: &Binder, rel: &Relation, alias: Option<&str>, only: bool) -> Result<RangeEntry> {
    let schema = b
        .env
        .snapshot()
        .namespace(rel.namespace)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::internal(format!("namespace of relation \"{}\" not found", rel.name)))?;
    let columns = rel
        .columns
        .iter()
        .map(|c| RangeColumn { name: c.name.clone(), ty: c.type_id.clone(), dropped: c.dropped })
        .collect();
    Ok(RangeEntry {
        kind: RangeKind::Relation {
            oid: rel.oid,
            schema,
            name: rel.name.clone(),
            rel_kind: rel.kind,
            only,
            pseudo: None,
        },
        alias: alias.map(str::to_string).unwrap_or_else(|| rel.name.clone()),
        columns,
        lateral: false,
        visible: true,
        access: select_access(),
    })
}

/// Apply a `TableAlias` (name + column list) to `entry`: renames the first N visible columns;
/// too many aliases -> `diag::too_many_column_aliases`.
pub fn apply_column_aliases(b: &mut Binder, entry: &mut RangeEntry, alias: &TableAlias) -> Result<()> {
    if alias.columns.is_empty() {
        return Ok(());
    }
    // A column definition list (`AS t(a int)`) is only legal for record-returning functions, and
    // those are handled by `bind_function_factor`.
    if let Some(dt) = alias.columns.iter().find_map(|c| c.data_type.as_ref()) {
        return Err(diag::syntax_error_near(&dt.to_string(), None));
    }
    let available = entry.columns.iter().filter(|c| !c.dropped).count();
    if alias.columns.len() > available {
        return Err(diag::too_many_column_aliases(&entry.alias, available, alias.columns.len(), None));
    }
    let names: Vec<String> = alias.columns.iter().map(|c| b.ident(&c.name)).collect();
    let mut names = names.into_iter();
    for col in entry.columns.iter_mut().filter(|c| !c.dropped) {
        match names.next() {
            Some(n) => col.name = n,
            None => break,
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ helpers

/// Attach a position to an error (`None` clears it).
fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    e.position = pos;
    e
}

/// Clear the position of an error raised while binding stored (non-user) text.
fn stored_text_error(mut e: PgError) -> PgError {
    e.position = None;
    e
}

fn select_access() -> AccessNeeded {
    AccessNeeded { select: true, ..AccessNeeded::default() }
}

/// Output columns of a bound query or VALUES list as range columns (none dropped).
fn range_columns(out: &[OutputColumn]) -> Vec<RangeColumn> {
    out.iter().map(|c| RangeColumn { name: c.name.clone(), ty: c.ty, dropped: false }).collect()
}

/// A query with no WITH / ORDER BY / LIMIT / FETCH / locking clause (plain `VALUES` body).
fn is_plain_query(q: &Query) -> bool {
    q.with.is_none() && q.order_by.is_none() && q.limit_clause.is_none() && q.fetch.is_none() && q.locks.is_empty()
}

/// `db.s.t` naming another database: 0A000 (only the connected database may be named).
fn check_database(b: &Binder, qn: &QualName) -> Result<()> {
    if let Some(cat) = qn.catalog.as_deref() {
        if cat != b.ctx.session.database.as_str() {
            return Err(diag::cross_database(&format!("{}.{}", cat, qn.display()), qn.pos));
        }
    }
    Ok(())
}

/// Lateral visibility of the items this FROM list added before the current factor: when the
/// factor may not reference them they are marked `lateral_only` (so a reference fails with 42P01
/// `invalid reference to FROM-clause entry`), otherwise they are visible.
fn hide_earlier_items(b: &mut Binder, hidden: bool) {
    if let Ok(sc) = b.scope_mut() {
        let base = sc.from_depth as usize;
        for ns in sc.namespace.iter_mut().skip(base) {
            ns.lateral_only = hidden;
        }
    }
}

/// Push `entry` on the range table, add its namespace item (visible to later LATERAL items) and
/// return the FROM tree leaf.
fn register_entry(b: &mut Binder, entry: RangeEntry) -> Result<FromItem> {
    let rt = scope::add_range_entry(b, entry)?;
    scope::add_namespace_for_range(b, rt, true, true)?;
    let sc = b.scope_mut()?;
    if let Some(ns) = sc.namespace.last_mut() {
        ns.lateral_ok = true;
        ns.lateral_only = false;
    }
    Ok(FromItem::Range(rt))
}

/// A named FROM item: a CTE of the visible WITH clauses, else a catalog relation (views expand).
fn bind_named_table(b: &mut Binder, name: &ObjectName, alias: Option<&TableAlias>) -> Result<FromItem> {
    let qn = b.qual(name)?;
    let pos = qn.pos;
    check_database(b, &qn)?;
    let alias_name: Option<String> = alias.map(|a| b.ident(&a.name));

    if qn.schema.is_none() && qn.catalog.is_none() {
        if let Some(cr) = cte::find_cte(b, &qn.name, pos)? {
            let mut entry = RangeEntry {
                kind: RangeKind::Cte {
                    cte_index: cr.cte_index,
                    levels_up: cr.levels_up,
                    recursive_self: cr.recursive_self,
                },
                alias: alias_name.clone().unwrap_or_else(|| qn.name.clone()),
                columns: range_columns(&cr.columns),
                lateral: false,
                visible: true,
                access: AccessNeeded::default(),
            };
            if let Some(a) = alias {
                apply_column_aliases(b, &mut entry, a)?;
            }
            return register_entry(b, entry);
        }
    }

    let env: &CatalogEnv = b.env;
    let rel = env.resolve_relation(qn.schema.as_deref(), &qn.name).map_err(|e| at(e, pos))?;
    if rel.kind == RelKind::View {
        return expand_view(b, rel, alias_name, alias, pos);
    }
    if rel.kind == RelKind::CompositeType {
        let e = PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot open relation \"{}\"", qn.name))
            .with_detail("This operation is not supported for composite types.");
        return Err(at(e, pos));
    }
    let mut entry = relation_range_entry(b, rel, alias_name.as_deref(), false)?;
    if let Some(a) = alias {
        apply_column_aliases(b, &mut entry, a)?;
    }
    register_entry(b, entry)
}

/// Expand a view: re-parse its stored definition and bind it as a sub-select whose output names
/// are the view's column names (PG `ApplyRetrieveRule`, here at bind time).
fn expand_view(
    b: &mut Binder,
    rel: &Relation,
    alias_name: Option<String>,
    alias: Option<&TableAlias>,
    pos: Option<u32>,
) -> Result<FromItem> {
    let view = rel
        .view
        .as_ref()
        .ok_or_else(|| PgError::internal(format!("view \"{}\" has no definition", rel.name)))?;
    if b.expanding_views.contains(&rel.oid) {
        let e = PgError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!("infinite recursion detected in rules for relation \"{}\"", rel.name),
        );
        return Err(at(e, pos));
    }
    let raw = parser::parse_one(&view.query.sql).map_err(stored_text_error)?;
    let query: &Query = match raw.as_ast() {
        Some(Statement::Query(q)) => &**q,
        _ => {
            return Err(PgError::internal(format!(
                "stored definition of view \"{}\" is not a query",
                rel.name
            )))
        }
    };
    let names: Vec<String> = rel.columns.iter().filter(|c| !c.dropped).map(|c| c.name.clone()).collect();

    b.expanding_views.push(rel.oid);
    let bound = select::bind_subselect(b, query, &names).map_err(stored_text_error);
    b.expanding_views.pop();
    let bound = bound?;

    let columns = range_columns(&bound.output);
    let mut entry = RangeEntry {
        kind: RangeKind::Subquery { query: Box::new(bound) },
        alias: alias_name.unwrap_or_else(|| rel.name.clone()),
        columns,
        lateral: false,
        visible: true,
        access: select_access(),
    };
    if let Some(a) = alias {
        apply_column_aliases(b, &mut entry, a)?;
    }
    register_entry(b, entry)
}

/// A derived table: `(SELECT ...)`, `(VALUES ...)` or `LATERAL (...)`.
fn bind_derived(b: &mut Binder, subquery: &Query, lateral: bool, alias: Option<&TableAlias>) -> Result<FromItem> {
    if is_plain_query(subquery) {
        if let SetExpr::Values(values_body) = &*subquery.body {
            let bv = values::bind_values_body(b, values_body, None)?;
            let columns = range_columns(&values::values_output_columns(&bv, &[]));
            let alias_name = match alias {
                Some(a) => b.ident(&a.name),
                None => "*VALUES*".to_string(),
            };
            let mut entry = RangeEntry {
                kind: RangeKind::Values { values: bv },
                alias: alias_name,
                columns,
                lateral,
                visible: true,
                access: AccessNeeded::default(),
            };
            if let Some(a) = alias {
                apply_column_aliases(b, &mut entry, a)?;
            }
            return register_entry(b, entry);
        }
    }
    let bq = select::bind_subselect(b, subquery, &[])?;
    let columns = range_columns(&bq.output);
    let alias_name = match alias {
        Some(a) => b.ident(&a.name),
        None => "unnamed_subquery".to_string(),
    };
    let mut entry = RangeEntry {
        kind: RangeKind::Subquery { query: Box::new(bq) },
        alias: alias_name,
        columns,
        lateral,
        visible: true,
        access: AccessNeeded::default(),
    };
    if let Some(a) = alias {
        apply_column_aliases(b, &mut entry, a)?;
    }
    register_entry(b, entry)
}

/// A table function in FROM (`f(args)`, `UNNEST(..)`, `t(args)` with a table-function argument
/// list). Function calls here are implicitly LATERAL. Column naming follows PG
/// `addRangeTableEntryForFunction` / `chooseScalarFunctionAlias`.
fn bind_function_factor(
    b: &mut Binder,
    name: &ObjectName,
    args: Vec<FunctionArg>,
    with_ordinality: bool,
    alias: Option<&TableAlias>,
) -> Result<FromItem> {
    let qn = b.qual(name)?;
    check_database(b, &qn)?;
    let table_alias: Option<String> = alias.map(|a| b.ident(&a.name));

    let func = Function {
        name: name.clone(),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList { duplicate_treatment: None, args, clauses: Vec::new() }),
        within_group: Vec::new(),
        filter: None,
        null_treatment: None,
        over: None,
    };
    let call = expr_func::bind_function_call(b, &func, ExprCtx::function_in_from())?;
    let func_oid = match &call.kind {
        ExprKind::FuncCall(fc) => Some(fc.func),
        _ => None,
    };
    let ret = call.ty;

    let any_typed = alias.map_or(false, |a| a.columns.iter().any(|c| c.data_type.is_some()));
    let all_typed = alias.map_or(false, |a| !a.columns.is_empty() && a.columns.iter().all(|c| c.data_type.is_some()));
    let outs = match func_oid {
        Some(oid) if oid < 16384 => builtin_proc(oid).map(|p| p.out_columns()).unwrap_or_default(),
        _ => Vec::new(),
    };
    let is_record = ret.oid == oids::RECORD;
    let composite = !is_record && b.env.type_info(ret.oid).map_or(false, |t| t.is_composite());

    let mut names_from_defs = false;
    let mut fn_columns: Vec<RangeColumn> = if !outs.is_empty() {
        if any_typed {
            return Err(only_record_error());
        }
        outs.iter()
            .map(|(n, t)| RangeColumn { name: (*n).to_string(), ty: TypeId::of(*t), dropped: false })
            .collect()
    } else if is_record {
        let defs = match alias {
            Some(a) if all_typed => &a.columns,
            _ => return Err(required_record_error()),
        };
        names_from_defs = true;
        let mut cols = Vec::with_capacity(defs.len());
        for d in defs {
            let dt = match &d.data_type {
                Some(dt) => dt,
                None => return Err(required_record_error()),
            };
            let col_name = b.ident(&d.name);
            let ty = types::resolve_data_type(b, dt, None)?.id;
            cols.push(RangeColumn { name: col_name, ty, dropped: false });
        }
        cols
    } else if composite {
        if any_typed {
            return Err(only_record_error());
        }
        let fields = b
            .env
            .composite_fields(ret.oid)
            .ok_or_else(|| PgError::internal(format!("no attributes known for composite type {}", ret.oid)))?;
        fields
            .iter()
            .map(|f| RangeColumn { name: f.name.clone(), ty: f.type_id.clone(), dropped: f.dropped })
            .collect()
    } else {
        if any_typed {
            return Err(only_record_error());
        }
        // Single scalar result: named after the table alias, else the function name.
        let col_name = table_alias.clone().unwrap_or_else(|| qn.name.clone());
        vec![RangeColumn { name: col_name, ty: ret, dropped: false }]
    };

    let fn_count = fn_columns.len();
    if with_ordinality {
        fn_columns.push(RangeColumn { name: "ordinality".to_string(), ty: TypeId::of(oids::INT8), dropped: false });
    }
    let mut entry = RangeEntry {
        kind: RangeKind::Function { funcs: Vec::new(), with_ordinality },
        alias: table_alias.unwrap_or_else(|| qn.name.clone()),
        columns: fn_columns,
        lateral: true,
        visible: true,
        access: AccessNeeded::default(),
    };
    if let Some(a) = alias {
        if !names_from_defs {
            apply_column_aliases(b, &mut entry, a)?;
        }
    }
    let fn_part: Vec<RangeColumn> = entry.columns[..fn_count].to_vec();
    entry.kind = RangeKind::Function {
        funcs: vec![RangeFunction { expr: call, columns: fn_part }],
        with_ordinality,
    };
    register_entry(b, entry)
}

fn only_record_error() -> PgError {
    PgError::new(
        SqlState::SYNTAX_ERROR,
        "a column definition list is only allowed for functions returning \"record\"",
    )
}

fn required_record_error() -> PgError {
    PgError::new(
        SqlState::SYNTAX_ERROR,
        "a column definition list is required for functions returning \"record\"",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::binder::scope::{QueryScope, ScopeKind};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// The first FROM factor of a single SELECT statement.
    fn first_from(sql: &str) -> TableFactor {
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL parses");
        match stmts.into_iter().next() {
            Some(Statement::Query(q)) => match *q.body {
                SetExpr::Select(sel) => sel.from.into_iter().next().expect("one FROM item").relation,
                _ => panic!("expected a plain SELECT"),
            },
            _ => panic!("expected a query"),
        }
    }

    fn ctx_for(env: &CatalogEnv) -> BindContext<'_> {
        BindContext {
            env,
            session: BindSession { database: "pgarbor".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        }
    }

    fn col(name: &str) -> RangeColumn {
        RangeColumn { name: name.to_string(), ty: TypeId::of(oids::INT4), dropped: false }
    }

    fn dropped(name: &str) -> RangeColumn {
        RangeColumn { name: name.to_string(), ty: TypeId::of(oids::INT4), dropped: true }
    }

    fn entry_with(columns: Vec<RangeColumn>) -> RangeEntry {
        RangeEntry {
            kind: RangeKind::Function { funcs: Vec::new(), with_ordinality: false },
            alias: "t".to_string(),
            columns,
            lateral: false,
            visible: true,
            access: AccessNeeded::default(),
        }
    }

    #[test]
    fn column_alias_list_skips_dropped_slots() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = ctx_for(&env);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        let mut entry = entry_with(vec![dropped("gone"), col("c1"), col("c2")]);
        let alias = match first_from("SELECT * FROM t AS t(x, y)") {
            TableFactor::Table { alias: Some(a), .. } => a,
            _ => panic!("expected aliased table"),
        };
        apply_column_aliases(&mut b, &mut entry, &alias).expect("aliases apply");
        let names: Vec<&str> = entry.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["gone", "x", "y"]);
    }

    #[test]
    fn too_many_column_aliases_is_42p10() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = ctx_for(&env);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        let mut entry = entry_with(vec![dropped("gone"), col("c1"), col("c2")]);
        let alias = match first_from("SELECT * FROM t AS t(x, y, z)") {
            TableFactor::Table { alias: Some(a), .. } => a,
            _ => panic!("expected aliased table"),
        };
        let e = apply_column_aliases(&mut b, &mut entry, &alias).expect_err("too many aliases");
        assert_eq!(e.sqlstate, SqlState::INVALID_COLUMN_REFERENCE);
        assert_eq!(e.message, "table \"t\" has 2 columns available but 3 columns specified");
    }

    #[test]
    fn missing_relation_is_42p01_at_name() {
        let sql = "SELECT * FROM nope";
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = ctx_for(&env);
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        let tf = first_from(sql);
        let e = bind_table_factor(&mut b, &tf, true).expect_err("nope does not exist");
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_TABLE);
        assert_eq!(e.message, "relation \"nope\" does not exist");
        assert_eq!(e.position, Some(15));
    }

    #[test]
    fn catalog_relation_entry_names_schema_and_relation() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let rel = env.resolve_relation(Some("pg_catalog"), "pg_class").expect("pg_class exists");
        let ctx = ctx_for(&env);
        let src = SourceMap::new("");
        let b = Binder::new(&ctx, &src);
        let entry = relation_range_entry(&b, rel, None, false).expect("entry builds");
        assert_eq!(entry.alias, "pg_class");
        assert!(!entry.columns.is_empty());
        match &entry.kind {
            RangeKind::Relation { schema, name, .. } => {
                assert_eq!(schema.as_str(), "pg_catalog");
                assert_eq!(name.as_str(), "pg_class");
            }
            other => panic!("expected a relation entry, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs SQL-05"]
    fn aliased_relation_registers_range_entry_and_namespace() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = ctx_for(&env);
        let sql = "SELECT * FROM pg_catalog.pg_class AS c";
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        b.scopes.push(QueryScope::new(ScopeKind::Select));
        let tf = first_from(sql);
        let item = bind_table_factor(&mut b, &tf, true).expect("binds");
        assert_eq!(item, FromItem::Range(1));
        assert_eq!(b.scope().expect("scope").range_table[0].alias, "c");
    }

    #[test]
    #[ignore = "needs SQL-05, SQL-12"]
    fn scalar_set_returning_function_is_named_after_alias() {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = ctx_for(&env);
        let sql = "SELECT * FROM generate_series(1, 3) g";
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        b.scopes.push(QueryScope::new(ScopeKind::Select));
        let tf = first_from(sql);
        bind_table_factor(&mut b, &tf, true).expect("binds");
        let entry = &b.scope().expect("scope").range_table[0];
        assert_eq!(entry.columns.len(), 1);
        assert_eq!(entry.columns[0].name, "g");
        assert_eq!(entry.columns[0].ty, TypeId::of(oids::INT4));
    }
}
