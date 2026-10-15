//! Stored expressions: binding catalog expression text and producing `StoredExpr` (SQL-29, sonnet).
//! The catalog stores defaults, CHECKs, index expressions/predicates, generation expressions and view
//! queries as CANONICAL TEXT (`pg_get_expr` format) plus a dependency list (`StoredExpr { sql, bound:
//! None, depends_on }`, catalog contract invariant 5 and amendment 04).
//!
//! ## Two directions
//! 1. **DDL time** ([`store_expr`]): after binding a user expression (`CREATE TABLE t(a int DEFAULT
//!    nextval('s'), CHECK (a > 0))`) deparse it with `deparse::expr::deparse_expr` (canonical PG text,
//!    e.g. `nextval('s'::regclass)`, `(a > 0)`) and collect dependencies with [`collect_dependencies`].
//! 2. **Use time** ([`bind_stored_expr`]): re-parse `StoredExpr::sql` (`parser::parse_expr_text`) and bind it
//!    again in the scope of its table (DML defaults/checks/generated columns, ALTER COLUMN TYPE USING
//!    fallback, partial index predicate matching). Stored text came out of our own deparser, so
//!    bind errors there are internal (XX000 `could not bind stored expression`).
//!
//! ## Binding scopes for DDL expressions
//! A table that does not exist yet (CREATE TABLE) or changes (ALTER) has no `Relation` to open: use
//! [`table_expr_scope`], which pushes a `ScopeKind::TableExpr` scope whose single range entry (rt 1)
//! is a synthetic relation named `table_name` with the given `(column name, type)` list in attnum order.
//! Column references bind to `Column{rt_index: 1, attnum}`. The caller pops the scope with
//! `Binder::scopes.pop()`.
//!
//! ## Dependencies ([`collect_dependencies`])
//! Walk the bound expression (`BoundExpr::walk`): `Column{rt_index: 1}` -> `ObjectAddress::column(table,
//! attnum)` (when the table OID is known; for CREATE TABLE the catalog adds the self-column deps itself:
//! pass `table: None`), `FuncCall`/`Op`/`Aggref` of user functions (OID >= 16384) -> `ObjClass::Proc`,
//! user types in casts/consts/arrays (`ty.oid >= 16384`, enum/domain/composite) -> `ObjClass::Type`, a
//! `regclass` constant (`'s'::regclass`, created by `nextval('s')`) -> the sequence/relation, `SubLink`
//! bodies are rejected before this point. Builtin objects (OID < 16384) are never dependencies. Order:
//! first occurrence, de-duplicated.
//!
//! PG references (behavior only, no code copied): `src/backend/catalog/dependency.c`
//! (`find_expr_references_walker`, `recordDependencyOnSingleRelExpr`), `src/backend/catalog/heap.c`
//! (`StoreRelCheck`, `StoreAttrDefault`), `src/backend/parser/parse_expr.c`.
use pgarbor_catalog::model::common::ObjClass;
use pgarbor_catalog::model::RelKind;
use pgarbor_catalog::{ObjectAddress, StoredExpr};
use pgarbor_common::{Oid, PgError, Result, FIRST_NORMAL_OID};
use pgarbor_types::text::format::quote_identifier;
use pgarbor_types::{Datum, TypeEnv, TypeId};

use super::coerce::coerce_assignment;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr::bind_expr;
use super::scope::{add_namespace_for_range, add_range_entry, QueryScope, ScopeKind};
use crate::deparse::expr::deparse_expr;
use crate::deparse::{ColumnNamer, DeparseCtx, DeparseOptions};
use crate::ir::{AccessNeeded, BoundExpr, ColumnRef, ExprKind, OuterRef, RangeColumn, RangeEntry, RangeKind};
use crate::parser::parse_expr_text;

/// Bind catalog expression text in the scope of an existing relation (`rel_oid`): opens a
/// `TableExpr` scope over the relation's columns (`from::relation_range_entry`), parses and binds the
/// text with `ec`, pops the scope. The caller applies the final coercion to the column type
/// (`coerce::coerce_assignment`).
pub fn bind_stored_expr(b: &mut Binder, sql: &str, rel_oid: Oid, ec: ExprCtx) -> Result<BoundExpr> {
    // Copy the catalog reference out first so no borrow of `b` survives into the scope push.
    let env = b.env;
    let (name, kind, columns) = {
        let rel = env
            .snapshot()
            .relation(rel_oid)
            .ok_or_else(|| PgError::internal(format!("could not open relation with OID {rel_oid} for stored expression")))?;
        let columns: Vec<RangeColumn> = rel
            .columns
            .iter()
            .map(|c| RangeColumn { name: c.name.clone(), ty: c.type_id, dropped: c.dropped })
            .collect();
        let kind = RangeKind::Relation {
            oid: rel_oid,
            schema: String::new(),
            name: rel.name.clone(),
            rel_kind: rel.kind.clone(),
            only: false,
            pseudo: None,
        };
        (rel.name.clone(), kind, columns)
    };
    push_table_scope(b, &name, kind, columns)?;
    let res = bind_text_in_scope(b, sql, ec);
    let _ = b.scopes.pop();
    res
}

/// Push a `TableExpr` scope over a synthetic relation (module docs). `columns` = (name, type) in attnum
/// order; dropped/system columns not representable here.
pub fn table_expr_scope(b: &mut Binder, table_name: &str, columns: &[(String, TypeId)]) -> Result<()> {
    let cols: Vec<RangeColumn> = columns
        .iter()
        .map(|(name, ty)| RangeColumn { name: name.clone(), ty: *ty, dropped: false })
        .collect();
    let kind = RangeKind::Relation {
        oid: 0,
        schema: String::new(),
        name: table_name.to_string(),
        rel_kind: RelKind::Table,
        only: false,
        pseudo: None,
    };
    push_table_scope(b, table_name, kind, cols)
}

/// Deparse + dependencies -> `StoredExpr` (module docs, direction 1). `table`: OID of the owning table
/// when it exists (ALTER), `None` for CREATE TABLE (catalog supplies those edges).
pub fn store_expr(b: &mut Binder, bound: &BoundExpr, table: Option<Oid>, column_names: &dyn Fn(i16) -> Option<String>) -> Result<StoredExpr> {
    let namer = StoredColumnNamer { names: column_names };
    let ctx = DeparseCtx {
        env: b.env as &dyn TypeEnv,
        catalog: b.env,
        namer: &namer,
        opts: DeparseOptions::default(),
    };
    let sql = deparse_expr(&ctx, bound)?;
    let depends_on = collect_dependencies(b, bound, table)?;
    Ok(StoredExpr { sql, bound: None, depends_on })
}

/// Dependency list of a bound expression (module docs).
pub fn collect_dependencies(b: &Binder, e: &BoundExpr, table: Option<Oid>) -> Result<Vec<ObjectAddress>> {
    // Classification depends only on the bound tree and OIDs, not on the catalog snapshot.
    let _ = b;
    let mut out: Vec<ObjectAddress> = Vec::new();
    visit_deps(e, table, &mut out);
    Ok(out)
}

/// Convenience for DDL binders: bind AST expression `e` in the current `TableExpr` scope with `ec`,
/// coerce it (assignment context) to `target` if given, and return (bound, stored). `columns_ref`
/// receives the attnums of columns referenced, in order of first reference (CHECK `conkey`).
pub fn bind_ddl_expr(
    b: &mut Binder,
    e: &sqlparser::ast::Expr,
    ec: ExprCtx,
    target: Option<TypeId>,
    table: Option<Oid>,
    columns_ref: &mut Vec<i16>,
    column_names: &dyn Fn(i16) -> Option<String>,
) -> Result<(BoundExpr, StoredExpr)> {
    let bound = bind_expr(b, e, ec)?;
    let bound = match target {
        Some(t) => {
            let arg = BoundArg::new(bound, b.pos(e));
            // NOTE: the DDL caller does not pass the column name; see the report on this lease.
            coerce_assignment(b, &arg, "", t)?
        }
        None => bound,
    };
    let mut refs: Vec<ColumnRef> = Vec::new();
    bound.collect_columns(0, &mut refs);
    for r in refs {
        if r.rt_index == 1 && r.attnum > 0 && !columns_ref.contains(&r.attnum) {
            columns_ref.push(r.attnum);
        }
    }
    let stored = store_expr(b, &bound, table, column_names)?;
    Ok((bound, stored))
}

// ---------------------------------------------------------------------------------------------
// private helpers
// ---------------------------------------------------------------------------------------------

/// Open a `TableExpr` scope with one synthetic relation entry (rt 1) and register its namespace.
/// On failure the scope is popped again so the binder state stays balanced.
fn push_table_scope(b: &mut Binder, table_name: &str, kind: RangeKind, columns: Vec<RangeColumn>) -> Result<()> {
    b.scopes.push(QueryScope::new(ScopeKind::TableExpr));
    let entry = RangeEntry {
        kind,
        alias: table_name.to_string(),
        columns,
        lateral: false,
        visible: true,
        access: AccessNeeded::default(),
    };
    let res = match add_range_entry(b, entry) {
        Ok(_) => add_namespace_for_range(b, 1, true, true),
        Err(e) => Err(e),
    };
    if res.is_err() {
        let _ = b.scopes.pop();
    }
    res
}

/// Parse and bind stored text in the current (already pushed) scope. Any failure is internal: the
/// text came from our own deparser (XX000 `could not bind stored expression`).
fn bind_text_in_scope(b: &mut Binder, sql: &str, ec: ExprCtx) -> Result<BoundExpr> {
    let ast = parse_expr_text(sql).map_err(internal_stored)?;
    bind_expr(b, &ast, ec).map_err(internal_stored)
}

fn internal_stored(e: PgError) -> PgError {
    PgError::internal("could not bind stored expression").with_detail(e.message)
}

/// Renders `Column{levels_up 0, rt 1, attnum}` as the quoted name the caller provides. Anything else
/// (outer levels, other range entries, system columns, plan-time nodes) is not representable in a
/// stored table expression and yields `None`, which makes the deparser fail with XX000.
struct StoredColumnNamer<'n> {
    names: &'n dyn Fn(i16) -> Option<String>,
}

impl ColumnNamer for StoredColumnNamer<'_> {
    fn column(&self, col: &ColumnRef) -> Option<String> {
        if col.levels_up != 0 || col.rt_index != 1 || col.attnum < 1 {
            return None;
        }
        let name = (self.names)(col.attnum)?;
        quote_identifier(&name).ok()
    }
    fn input_ref(&self, _index: u32) -> Option<String> {
        None
    }
    fn outer_ref(&self, _outer: &OuterRef) -> Option<String> {
        None
    }
}

fn push_dep(out: &mut Vec<ObjectAddress>, addr: ObjectAddress) {
    if !out.contains(&addr) {
        out.push(addr);
    }
}

/// Pre-order visit: the node's own dependencies first, then its children left to right, so the
/// resulting order is first occurrence.
fn visit_deps(e: &BoundExpr, table: Option<Oid>, out: &mut Vec<ObjectAddress>) {
    let user = |oid: Oid| oid >= FIRST_NORMAL_OID;
    match &e.kind {
        ExprKind::Column(c) => {
            if let Some(t) = table {
                if c.levels_up == 0 && c.rt_index == 1 {
                    if c.attnum > 0 {
                        push_dep(out, ObjectAddress::column(t, c.attnum));
                    } else if c.attnum == 0 {
                        push_dep(out, ObjectAddress::relation(t));
                    }
                }
            }
        }
        ExprKind::Const(Datum::Oid(o)) => {
            if e.ty.oid == pgarbor_types::oids::REGCLASS && user(*o) {
                push_dep(out, ObjectAddress::relation(*o));
            }
        }
        ExprKind::FuncCall(f) => {
            if user(f.func) {
                push_dep(out, ObjectAddress::proc(f.func));
            }
        }
        ExprKind::Op(o) => {
            if user(o.op.op) {
                push_dep(out, ObjectAddress::new(ObjClass::Operator, o.op.op));
            }
            if user(o.op.proc) {
                push_dep(out, ObjectAddress::proc(o.op.proc));
            }
        }
        ExprKind::Aggref(a) => {
            if user(a.agg) {
                push_dep(out, ObjectAddress::proc(a.agg));
            }
        }
        ExprKind::WindowFunc(w) => {
            if user(w.func) {
                push_dep(out, ObjectAddress::proc(w.func));
            }
        }
        _ => {}
    }
    // User types of any non-column node (casts, constants, arrays, enum/domain/composite results).
    // Column types are already owned by the table's own column entry, so columns add no edge here.
    if !matches!(e.kind, ExprKind::Column(_)) && user(e.ty.oid) {
        push_dep(out, ObjectAddress::type_(e.ty.oid));
    }
    e.for_each_child(&mut |c| visit_deps(c, table, out));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::ir::{FuncCallExpr, OpExpr, OpRef};
    use crate::source::SourceMap;
    use pgarbor_types::catalog_data::Volatility;
    use pgarbor_types::context::IoConfig;

    fn func(oid: Oid, name: &str, args: Vec<BoundExpr>) -> BoundExpr {
        BoundExpr::new(
            ExprKind::FuncCall(Box::new(FuncCallExpr {
                func: oid,
                name: name.to_string(),
                args,
                variadic: false,
                retset: false,
                volatility: Volatility::Volatile,
                strict: true,
                is_cast_call: false,
            })),
            TypeId::INT4,
        )
    }

    fn seq_oid_const(oid: Oid) -> BoundExpr {
        BoundExpr::constant(Datum::Oid(oid), TypeId::REGCLASS)
    }

    #[test]
    fn nextval_default_depends_on_sequence_only() {
        // nextval('s'::regclass) with the sequence at OID 16400; nextval (1574) is builtin.
        let e = func(1574, "nextval", vec![seq_oid_const(16400)]);
        let deps = visit_collect(&e, Some(16500));
        assert_eq!(deps, vec![ObjectAddress::relation(16400)]);
    }

    #[test]
    fn user_function_and_column_dependencies_in_order() {
        // f(a) where f is user function 16390 and a is attnum 2 of table 16500.
        let col = BoundExpr::column(0, 1, 2, TypeId::INT4);
        let e = func(16390, "f", vec![col]);
        let deps = visit_collect(&e, Some(16500));
        assert_eq!(deps, vec![ObjectAddress::proc(16390), ObjectAddress::column(16500, 2)]);
    }

    #[test]
    fn create_table_has_no_column_dependencies_and_dedupes() {
        // table = None (CREATE TABLE): column refs add nothing; two identical user calls add one edge.
        let col = BoundExpr::column(0, 1, 1, TypeId::INT4);
        let e = BoundExpr::new(
            ExprKind::Op(Box::new(OpExpr {
                op: OpRef {
                    op: 16391,
                    proc: 16390,
                    name: "===".to_string(),
                    volatility: Volatility::Immutable,
                    strict: true,
                },
                args: vec![func(16390, "f", vec![col.clone()]), func(16390, "f", vec![col])],
            })),
            TypeId::BOOL,
        );
        let deps = visit_collect(&e, None);
        assert_eq!(
            deps,
            vec![ObjectAddress::new(ObjClass::Operator, 16391), ObjectAddress::proc(16390)]
        );
    }

    #[test]
    fn user_type_of_constant_is_a_dependency() {
        // A constant whose type is a user type (enum/domain/composite, OID >= 16384).
        let e = BoundExpr::constant(Datum::Text("x".to_string()), TypeId::of(16450));
        let deps = visit_collect(&e, Some(16500));
        assert_eq!(deps, vec![ObjectAddress::type_(16450)]);
    }

    /// Test-only wrapper that runs the private visitor like `collect_dependencies` does.
    fn visit_collect(e: &BoundExpr, table: Option<Oid>) -> Vec<ObjectAddress> {
        let mut out = Vec::new();
        visit_deps(e, table, &mut out);
        out
    }

    fn test_binder_env() -> Result<pgarbor_catalog::CatalogEnv> {
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        Ok(env)
    }

    #[test]
    #[ignore = "needs SQL-01 (parse_expr_text), SQL-05 (scope), SQL-10 (bind_expr), SQL-43 (deparse)"]
    fn check_a_gt_0_text_and_columns() -> Result<()> {
        let env = test_binder_env()?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new("a > 0");
        let mut b = Binder::new(&ctx, &src);
        table_expr_scope(&mut b, "t", &[("a".to_string(), TypeId::INT4)])?;
        let ast = parse_expr_text("a > 0")?;
        let names = |n: i16| if n == 1 { Some("a".to_string()) } else { None };
        let mut cols: Vec<i16> = Vec::new();
        let (_bound, stored) = bind_ddl_expr(&mut b, &ast, ExprCtx::check(), None, None, &mut cols, &names)?;
        assert_eq!(stored.sql, "(a > 0)");
        assert_eq!(cols, vec![1]);
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10, SQL-43 and a sequence created through pgarbor_catalog::testing::run"]
    fn default_nextval_stores_regclass_text_and_sequence_dependency() -> Result<()> {
        // Expected: `nextval('s'::regclass)` with depends_on == [ObjectAddress::relation(seq_oid)].
        // The sequence fixture must be created by the catalog testing helpers before binding.
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10 and a relation fixture (bind_stored_expr opens it by OID)"]
    fn rebind_round_trip_matches_original_bind() -> Result<()> {
        // bind("a > 0") -> store -> bind_stored_expr(store.sql) must yield an equal BoundExpr tree.
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-01, SQL-05, SQL-10"]
    fn subquery_in_check_is_rejected() -> Result<()> {
        let env = test_binder_env()?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new("a > (select 1)");
        let mut b = Binder::new(&ctx, &src);
        table_expr_scope(&mut b, "t", &[("a".to_string(), TypeId::INT4)])?;
        let ast = parse_expr_text("a > (select 1)")?;
        let names = |n: i16| if n == 1 { Some("a".to_string()) } else { None };
        let mut cols: Vec<i16> = Vec::new();
        match bind_ddl_expr(&mut b, &ast, ExprCtx::check(), None, None, &mut cols, &names) {
            Err(e) => {
                assert!(e.sqlstate == pgarbor_common::SqlState::FEATURE_NOT_SUPPORTED);
                assert!(e.message == "cannot use subquery in check constraint");
                Ok(())
            }
            Ok(_) => Err(PgError::internal("subquery in CHECK was accepted")),
        }
    }
}
