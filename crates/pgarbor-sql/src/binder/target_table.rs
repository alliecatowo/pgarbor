//! DML target tables: resolution, privilege marks, defaults, generated columns, CHECK constraints
//! (SQL-25). PG references: `parse_clause.c` (`setTargetTable`), `parse_relation.c`
//! (`addRangeTableEntry`), `rewriteHandler.c` (`rewriteTargetListIU`, `build_column_default`,
//! `get_assignment_input`), `analyze.c` (`transformInsertStmt` target handling), `execMain.c` /
//! `execExpr.c` for the constraint expressions the executor needs.
//!
//! [`open_target_table`] resolves the table named in `INSERT INTO t`, `UPDATE t`, `DELETE FROM t`,
//! `COPY t`:
//! * `Binder::qual` + `env.resolve_relation(schema, name)`: 42P01 `relation "t" does not exist`.
//! * Kind checks (`CheckValidRelation`-style, all 42809 / 55000): plain tables and (later)
//!   partitions are writable. A VIEW: v1 supports only the write-error 0A000 `INSERT into a view is not
//!   supported by PGArbor yet` (auto-updatable views are later work; document in the inventory); a
//!   materialized view: 42809 `cannot change materialized view "v"`; a sequence: 42809 `cannot change
//!   sequence "s"`; an index/composite type: 42809 `"x" is not a table`; a system catalog / virtual
//!   relation (OID < 16384): 42501 `permission denied: "pg_class" is a system catalog` (PGArbor catalogs are read-only);
//!   foreign table: 0A000.
//! * Alias: `UPDATE t AS x` / `INSERT INTO t AS x` -> the range entry's alias (columns referenced
//!   as `x.c`; `t.c` is then invalid: 42P01 `invalid reference to FROM-clause entry for table "t"`
//!   HINT `Perhaps you meant to reference the table alias "x".`).
//! * Builds a [`TargetTable`] via [`build_target_table`]; adds `RangeEntry` as range-table entry 1 of
//!   the DML scope (`ScopeKind::{Insert,Update,Delete}`), with `access.insert|update|delete = true`.
//!
//! [`build_target_table`] (`Relation` -> `TargetTable`):
//! * `columns`: one `TargetColumn` per attnum slot. `default`: the column's `StoredExpr` default
//!   re-parsed (`parser::parse_expr_text`) and bound in an expression scope over the table
//!   (`stored::bind_stored_expr`) and assignment-coerced to the column type; IDENTITY columns get
//!   `nextval('<seq>'::regclass)` of the identity sequence (the catalog `Sequence` owned by the
//!   column); `generated: Some((GeneratedKind::Stored, bound expr))` for generated columns, bound
//!   over the target's own columns (`Column{rt_index: 1, attnum}`); `not_null` from
//!   `Column::not_null`; `collation` from the column.
//! * `checks`: every enforced CHECK constraint of the table (`env.snapshot().constraints_of(oid)`,
//!   `ConstraintKind::Check`, `enforced == true`) parsed + bound (boolean). Domain checks of column
//!   types are enforced via `CoerceToDomain` nodes, not listed here.
//! * Cost note: binding defaults for EVERY column on every INSERT is wasteful; bind lazily only the
//!   defaults that can be used (`bind_defaults: bool` parameter: UPDATE/DELETE pass false).
//!
//! ## Implementation decisions (SQL-25)
//! * `bind_stored_expr` takes the catalog text itself and re-parses it (frozen signature), so the
//!   default/generated/CHECK text is passed straight through; no separate `parse_expr_text` call.
//! * Generation expressions are bound regardless of `bind_defaults`: the executor recomputes
//!   stored generated columns on UPDATE too (PG `ExecComputeStoredGenerated`), so they are not a
//!   "default" in the lazy sense.
//! * The `GeneratedKind` of a generated column is the catalog's kind (`Stored` or `Virtual`).
//! * The target is registered visible in the namespace for every kind (PG `setTargetTable` adds it
//!   with both `rel` and `col` visibility). The INSERT VALUES binder is responsible for its own
//!   scope; `TargetCheck`s are bound for every kind (the executor applies them to INSERT/UPDATE).
//! * `DmlKind::Copy` takes the INSERT privilege flag (COPY FROM); COPY TO callers use
//!   `DmlKind::Copy` only where the target is written.
//! * UPDATE/DELETE on a view use PG-style wording (`UPDATE on a view ...`, `DELETE from a view ...`)
//!   with the same 0A000 SQLSTATE as INSERT; the spec names only the INSERT text.
use pgarbor_catalog::model::{Column, ConstraintKind, RelKind, TypeDef};
use pgarbor_catalog::snapshot::{ClassRef, UserTypeRef};
use pgarbor_catalog::Relation;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{builtin_proc, Volatility};
use pgarbor_types::{oids, Datum, TypeId};
use sqlparser::ast::{Ident, ObjectName};

use super::coerce::{coerce_assignment, coerce_to_boolean};
use super::context::{Binder, BoundArg, ExprCtx};
use super::from::relation_range_entry;
use super::scope::{add_namespace_for_range, add_range_entry};
use super::stored::bind_stored_expr;
use crate::diag::with_position;
use crate::ir::{BoundExpr, ExprKind, FuncCallExpr, RangeEntry, TargetCheck, TargetColumn, TargetTable};

/// Which statement opens the target (selects the privilege flag and error wording).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlKind {
    Insert,
    Update,
    Delete,
    Copy,
}

/// `nextval(regclass)` in the builtin catalog (`pg_proc.oid` 1574).
const NEXTVAL_PROC_OID: Oid = 1574;

/// Resolve + register the DML target; returns the target description and its 1-based range-table
/// index (always 1 for INSERT/COPY; UPDATE/DELETE register it before FROM/USING items so also 1).
pub fn open_target_table(
    b: &mut Binder,
    name: &ObjectName,
    alias: Option<&Ident>,
    kind: DmlKind,
    bind_defaults: bool,
) -> Result<(TargetTable, u32)> {
    let qn = b.qual(name)?;
    let pos = qn.pos;
    // Copy the shared reference out of the binder so the relation borrow does not pin `b`.
    let env = b.env;
    let schema = qn.schema.as_deref();

    match env.resolve_class(schema, &qn.name).map_err(|e| with_position(e, pos))? {
        ClassRef::Sequence(_) => {
            return Err(reject(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot change sequence \"{}\"", qn.name),
                pos,
            ));
        }
        ClassRef::Index(_) | ClassRef::SystemIndex(_) => {
            return Err(reject(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", qn.name), pos));
        }
        ClassRef::Relation(_) => {}
    }
    let rel = env.resolve_relation(schema, &qn.name).map_err(|e| with_position(e, pos))?;

    // Catalog and virtual relations are read-only (OID below FIRST_NORMAL_OID).
    if rel.is_virtual() {
        return Err(reject(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied: \"{}\" is a system catalog", rel.name),
            pos,
        ));
    }

    match rel.kind {
        RelKind::Table | RelKind::PartitionedTable => {}
        RelKind::View => {
            // Auto-updatable view DML (including WITH [LOCAL|CASCADED] CHECK OPTION enforcement) is not
            // implemented; the leased 0A000 keeps the same text and marks the gap for the harness.
            let what = match kind {
                DmlKind::Insert => "INSERT into a view",
                DmlKind::Update => "UPDATE on a view",
                DmlKind::Delete => "DELETE from a view",
                DmlKind::Copy => "COPY into a view",
            };
            return Err(with_position(PgError::unimplemented("SQL-25", what), pos));
        }
        RelKind::MatView => {
            return Err(reject(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot change materialized view \"{}\"", rel.name),
                pos,
            ));
        }
        RelKind::ForeignTable => {
            let msg = match kind {
                DmlKind::Insert | DmlKind::Copy => format!("cannot insert into foreign table \"{}\"", rel.name),
                DmlKind::Update => format!("cannot update foreign table \"{}\"", rel.name),
                DmlKind::Delete => format!("cannot delete from foreign table \"{}\"", rel.name),
            };
            return Err(reject(SqlState::FEATURE_NOT_SUPPORTED, msg, pos));
        }
        RelKind::Index | RelKind::PartitionedIndex | RelKind::Sequence | RelKind::CompositeType | RelKind::Toast => {
            return Err(reject(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", rel.name), pos));
        }
    }

    let alias_name: Option<String> = alias.map(|id| b.ident(id));
    let entry = target_range_entry(b, rel, alias_name.as_deref(), kind)?;
    let rt = add_range_entry(b, entry)?;
    add_namespace_for_range(b, rt, true, true)?;
    let target = build_target_table(b, rel, bind_defaults)?;
    Ok((target, rt))
}

/// `Relation` -> `TargetTable` (module docs).
pub fn build_target_table(b: &mut Binder, rel: &Relation, bind_defaults: bool) -> Result<TargetTable> {
    let env = b.env;
    let snap = env.snapshot();
    let schema = snap
        .namespace(rel.namespace)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::internal(format!("namespace {} of relation {} missing", rel.namespace, rel.oid)))?;

    let mut columns: Vec<TargetColumn> = Vec::with_capacity(rel.columns.len());
    for c in &rel.columns {
        let mut tc = TargetColumn {
            attnum: c.attnum,
            name: c.name.clone(),
            ty: c.type_id,
            dropped: c.dropped,
            not_null: c.not_null,
            collation: c.collation,
            default: None,
            identity: c.identity.map(|i| i.kind),
            generated: None,
        };
        if c.dropped {
            columns.push(tc);
            continue;
        }
        match (c.generated, c.default.as_ref()) {
            (Some(kind), Some(def)) => {
                let bound = bind_stored_expr(b, &def.expr.sql, rel.oid, ExprCtx::generated())?;
                let coerced = coerce_assignment(b, &BoundArg::new(bound, None), &c.name, c.type_id)?;
                tc.generated = Some((kind, coerced));
            }
            (Some(_), None) => {
                return Err(PgError::internal(format!("generated column \"{}\" has no expression", c.name)));
            }
            (None, _) => {
                if bind_defaults {
                    tc.default = column_default(b, rel, c)?;
                }
            }
        }
        columns.push(tc);
    }

    let mut checks: Vec<TargetCheck> = Vec::new();
    for con in snap.constraints_of(rel.oid) {
        if con.kind != ConstraintKind::Check || !con.enforced {
            continue;
        }
        let st = match con.check.as_ref() {
            Some(st) => st,
            None => continue,
        };
        let bound = bind_stored_expr(b, &st.sql, rel.oid, ExprCtx::check())?;
        let expr = if bound.ty.oid == oids::BOOL {
            bound
        } else {
            coerce_to_boolean(b, &BoundArg::new(bound, None), "CHECK")?
        };
        checks.push(TargetCheck { constraint: con.oid, name: con.name.clone(), expr });
    }

    Ok(TargetTable {
        oid: rel.oid,
        namespace: rel.namespace,
        schema,
        name: rel.name.clone(),
        kind: rel.kind,
        columns,
        checks,
    })
}

/// The `RangeEntry` of a writable target (delegates to `from::relation_range_entry` and sets the
/// access flags for `kind`).
pub fn target_range_entry(b: &mut Binder, rel: &Relation, alias: Option<&str>, kind: DmlKind) -> Result<RangeEntry> {
    let mut entry = relation_range_entry(b, rel, alias, false)?;
    entry.visible = true;
    // A DML target is not read unless an expression references it; PG checks SELECT per column
    // reference, not for the whole target.
    entry.access.select = false;
    entry.access.insert = matches!(kind, DmlKind::Insert | DmlKind::Copy);
    entry.access.update = kind == DmlKind::Update;
    entry.access.delete = kind == DmlKind::Delete;
    Ok(entry)
}

/// Effective default of one live column: IDENTITY -> `nextval(seq)`, otherwise the stored default;
/// both assignment-coerced to the column type (PG `build_column_default`). `None` = no default.
fn column_default(b: &mut Binder, rel: &Relation, c: &Column) -> Result<Option<BoundExpr>> {
    if let Some(identity) = c.identity {
        let env = b.env;
        let seq = match env.snapshot().sequences_owned_by(rel.oid, c.attnum).first() {
            Some(s) => s.oid,
            None => identity.sequence,
        };
        let call = nextval_call(seq)?;
        let coerced = coerce_assignment(b, &BoundArg::new(call, None), &c.name, c.type_id)?;
        return Ok(Some(coerced));
    }
    // A column without its own default takes its domain's default (PG `build_column_default` ->
    // `get_typdefault`); the domain CHECKs then apply through the assignment coercion.
    let stored_sql = match c.default.as_ref() {
        Some(def) => def.expr.sql.clone(),
        None => match domain_default_sql(b, c.type_id.oid) {
            Some(sql) => sql,
            None => return Ok(None),
        },
    };
    let bound = bind_stored_expr(b, &stored_sql, rel.oid, ExprCtx::column_default())?;
    let coerced = coerce_assignment(b, &BoundArg::new(bound, None), &c.name, c.type_id)?;
    Ok(Some(coerced))
}

/// Stored default text of the domain `ty` (`None` when `ty` is not a domain or has no default).
fn domain_default_sql(b: &Binder, ty: Oid) -> Option<String> {
    let snap = b.env.snapshot();
    let Some(UserTypeRef::Type(t)) = snap.user_type(ty) else { return None };
    let obj = snap.type_obj(t)?;
    match &obj.def {
        TypeDef::Domain { default: Some(d), .. } => Some(d.sql.clone()),
        _ => None,
    }
}

/// `nextval('seq'::regclass)` as a bound call of the builtin `nextval(regclass)`; the result type is
/// the builtin's declared return type (`bigint`).
fn nextval_call(seq: Oid) -> Result<BoundExpr> {
    let proc = builtin_proc(NEXTVAL_PROC_OID)
        .ok_or_else(|| PgError::internal("builtin nextval(regclass) missing from the catalog"))?;
    let regclass_arg = BoundExpr::constant(Datum::Oid(seq), TypeId::of(oids::REGCLASS));
    let call = FuncCallExpr {
        func: proc.oid,
        name: proc.name.to_string(),
        args: vec![regclass_arg],
        variadic: false,
        retset: proc.retset,
        volatility: Volatility::Volatile,
        strict: proc.strict,
        is_cast_call: false,
    };
    Ok(BoundExpr::new(ExprKind::FuncCall(Box::new(call)), TypeId::of(proc.ret)))
}

/// A user-facing error at `pos` (position attached only when not already set).
fn reject(sqlstate: SqlState, message: String, pos: Option<u32>) -> PgError {
    with_position(PgError::new(sqlstate, message), pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::binder::scope::{QueryScope, ScopeKind};
    use crate::source::SourceMap;
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_types::context::IoConfig;

    fn bind_ctx(env: &CatalogEnv) -> BindContext<'_> {
        BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        }
    }

    #[test]
    fn missing_relation_is_42p01() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let ctx = bind_ctx(&env);
        let src = SourceMap::new("insert into nope values (1)");
        let mut b = Binder::new(&ctx, &src);
        let name = ObjectName::from(Ident::new("nope"));
        let err = open_target_table(&mut b, &name, None, DmlKind::Insert, true).err().expect("error expected");
        assert_eq!(err.sqlstate, SqlState::UNDEFINED_TABLE);
        assert_eq!(err.message, "relation \"nope\" does not exist");
    }

    #[test]
    fn system_catalog_is_read_only() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let ctx = bind_ctx(&env);
        let src = SourceMap::new("delete from pg_class");
        let mut b = Binder::new(&ctx, &src);
        let name = ObjectName::from(Ident::new("pg_class"));
        let err = open_target_table(&mut b, &name, None, DmlKind::Delete, false).err().expect("error expected");
        assert_eq!(err.sqlstate, SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(err.message, "permission denied: \"pg_class\" is a system catalog");
    }

    #[test]
    #[ignore = "needs SQL-05, SQL-17 and SQL-29"]
    fn plain_default_and_checks_are_bound() {
        use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec};
        use pgarbor_catalog::model::{Persistence, StoredExpr};

        let (mut kv, env) = testing::bootstrapped().unwrap();
        let mut a = ColumnSpec::new("a", TypeId::INT4);
        a.default = Some(StoredExpr::new("7"));
        let spec = CreateTableSpec {
            namespace: pgarbor_catalog::builtin::PUBLIC_NAMESPACE,
            name: "t".to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![a],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let (_res, env2) = testing::run(&mut kv, &env, |t| {
            t.create_table(&spec)?;
            Ok(())
        })
        .unwrap();
        let ctx = bind_ctx(&env2);
        let src = SourceMap::new("insert into t values (1)");
        let mut b = Binder::new(&ctx, &src);
        b.scopes.push(QueryScope::new(ScopeKind::Insert));
        let name = ObjectName::from(Ident::new("t"));
        let (target, rt) = open_target_table(&mut b, &name, None, DmlKind::Insert, true).unwrap();
        assert_eq!(rt, 1);
        assert_eq!(target.schema, "public");
        assert!(target.columns[0].default.is_some());
        assert!(target.checks.is_empty());
    }
}
