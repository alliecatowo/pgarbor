//! CREATE TABLE and ALTER TABLE execution (lease EXE-33, north-star). Catalog operations:
//! `pgarbor_catalog::ddl::{table, alter_table, alter_column, constraint, index}`.
//!
//! ## `exec_create_table(cx, spec) -> ExecResult`
//! `run_ddl(cx, |ddl| ddl.create_table(&spec))`. `Ok(None)` (IF NOT EXISTS skip) -> tag `CREATE TABLE`
//! (the catalog queued the `relation "t" already exists, skipping` NOTICE). `Some(created)`: a new table
//! is empty, so no index build is needed; create the SEQUENCE STATE of every `created.sequences` entry
//! (serial/identity) through `ddl_followup::run_followups` with the outcome's follow-ups (the catalog
//! queues `CreateSequenceState`). Tag `CREATE TABLE`.
//!
//! ## `exec_alter_table(cx, alter) -> ExecResult` (`BoundAlterTable`)
//! One `run_ddl` for the WHOLE statement (single catalog version bump, atomic). Apply
//! `alter.commands` in order; each variant calls the `CatalogTxn` method named in its doc
//! (`pgarbor_sql::ir::AlterTableCmd`), passing `&pgarbor_sql::rewrite::SqlExprRewriter` where the method
//! takes an `ExprRewriter`, `DropBehavior::{Cascade, Restrict}` from the `cascade` flag.
//! Data-level work comes back as `FollowUp`s in the outcome and is executed AFTER the closure by
//! `ddl_followup::run_followups`, in order, with `ColumnRewrite`s built here:
//! * `AddColumn`: `ddl.add_column(table, &spec, if_not_exists)`; a volatile default makes the catalog emit
//!   `RewriteTable`; supply `ColumnRewrite::FromDefault{attnum}` (the attnum returned by add_column);
//!   a STORED generated column supplies `Generated{attnum}`. A constant default needs no rewrite
//!   (`Column::missing_value`).
//! * `AlterColumnType{..}`: `ddl.alter_column_type(table, &column, new_type, new_collation, new_ndims,
//!   new_default, needs_rewrite)`; if `needs_rewrite` supply `Using{attnum, expr: using}` (binder
//!   guarantees `using` is `Some` then).
//! * `AddConstraint(spec)`: `ddl.add_constraint(table, &spec)` (PK/UNIQUE queue `BuildIndex`, NOT NULL /
//!   CHECK / FK queue the matching `Validate*` unless `NOT VALID`).
//! * the remaining commands map one-to-one (`drop_column`, `rename_column(.., rewriter)`,
//!   `set_column_default`, `set_not_null`, `drop_not_null`, `add_identity`, `set_identity`,
//!   `drop_identity`, `set_generation_expression`, `drop_expression`, `set_column_attribute`,
//!   `drop_constraint`, `rename_constraint`, `validate_constraint`, `alter_constraint`,
//!   `set_relation_options`, `set_persistence`, `set_replica_identity`, `set_row_security`).
//! * DROP COLUMN needs no data work (slots stay; `DropRelationData` for dropped indexes arrives as a
//!   follow-up).
//! Tag: `alter.keyword` -> `ALTER TABLE` / `ALTER VIEW` / `ALTER MATERIALIZED VIEW` / `ALTER INDEX` /
//! `ALTER SEQUENCE` / `ALTER FOREIGN TABLE` / `ALTER TYPE` (same as `BoundStatement::tag()`).
//! `SET NOT NULL` over NULLs: 23502 `column "a" of relation "t" contains null values`.
use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec};
use pgarbor_catalog::model::relation::{GeneratedKind, Persistence};
use pgarbor_catalog::{CatalogTxn, DropBehavior, FollowUp};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{AlterRelKeyword, AlterTableCmd, BoundAlterTable};

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::ddl_followup::{run_followups, ColumnRewrite};
use crate::result::ExecResult;

/// CREATE TABLE (module docs). The catalog queues `CreateSequenceState` for serial/identity
/// sequences; any created sequence without one gets it added here so its state always exists.
pub fn exec_create_table(cx: &mut ExecContext, spec: &CreateTableSpec) -> Result<ExecResult> {
    for constraint in &spec.constraints { reject_deferred(constraint)?; }
    let (created, outcome) = run_ddl(cx, |ddl| ddl.create_table(spec))?;
    let mut follow_ups = outcome.follow_ups;
    if let Some(created) = &created {
        for seq in &created.sequences {
            let present = follow_ups
                .iter()
                .any(|f| matches!(f, FollowUp::CreateSequenceState { sequence } if sequence == seq));
            if !present {
                follow_ups.push(FollowUp::CreateSequenceState { sequence: *seq });
            }
        }
    }
    if !follow_ups.is_empty() {
        run_followups(cx, &follow_ups, None, &[])?;
    }
    Ok(ExecResult::command("CREATE TABLE"))
}

/// ALTER TABLE (module docs): one catalog statement for all sub-commands, then the data follow-ups.
pub fn exec_alter_table(cx: &mut ExecContext, alter: &BoundAlterTable) -> Result<ExecResult> {
    let table = alter.relation;
    let mut rewrites: Vec<ColumnRewrite> = Vec::new();
    let ((), outcome) = run_ddl(cx, |ddl| {
        for cmd in &alter.commands {
            apply_command(ddl, table, cmd, &mut rewrites)?;
        }
        Ok(())
    })?;
    if !outcome.follow_ups.is_empty() {
        run_followups(cx, &outcome.follow_ups, Some(table), &rewrites)?;
    }
    Ok(ExecResult::command(alter_tag(alter.keyword)))
}

/// Command tag for an ALTER keyword (same mapping as `BoundStatement::tag`).
fn alter_tag(keyword: AlterRelKeyword) -> &'static str {
    match keyword {
        AlterRelKeyword::Table => "ALTER TABLE",
        AlterRelKeyword::View => "ALTER VIEW",
        AlterRelKeyword::MaterializedView => "ALTER MATERIALIZED VIEW",
        AlterRelKeyword::Index => "ALTER INDEX",
        AlterRelKeyword::Sequence => "ALTER SEQUENCE",
        AlterRelKeyword::ForeignTable => "ALTER FOREIGN TABLE",
        AlterRelKeyword::Type => "ALTER TYPE",
    }
}

/// `CASCADE` -> `DropBehavior::Cascade`, otherwise `RESTRICT`.
fn behavior(cascade: bool) -> DropBehavior {
    if cascade {
        DropBehavior::Cascade
    } else {
        DropBehavior::Restrict
    }
}

/// Attnum and stored-generated flag of `column` in the catalog as it is inside the open DDL
/// statement (so it sees the statement's own earlier changes).
fn column_info(ddl: &mut CatalogTxn<'_>, table: Oid, column: &str) -> Result<(i16, bool)> {
    let snap = ddl.snapshot()?;
    let rel = snap
        .relation(table)
        .ok_or_else(|| PgError::internal(format!("relation {table} missing during ALTER TABLE")))?;
    let col = rel
        .column_by_name(column)
        .ok_or_else(|| PgError::internal(format!("column \"{column}\" missing during ALTER TABLE")))?;
    Ok((col.attnum, matches!(col.generated, Some(GeneratedKind::Stored))))
}

/// The row rewrite an ADD COLUMN needs, mirroring the catalog's `add_column` branch order: a generated
/// column (STORED only), else a serial or identity column, else a volatile default (the catalog emits
/// `RewriteTable` for each of these). Constant defaults and VIRTUAL generated columns need none.
fn add_column_rewrite(spec: &ColumnSpec, attnum: i16) -> Option<ColumnRewrite> {
    if let Some(g) = &spec.generated {
        return if matches!(g.kind, GeneratedKind::Stored) {
            Some(ColumnRewrite::Generated { attnum })
        } else {
            None
        };
    }
    if spec.serial.is_some() || spec.identity.is_some() || (spec.default.is_some() && spec.default_is_volatile) {
        Some(ColumnRewrite::FromDefault { attnum })
    } else {
        None
    }
}

/// Apply one ALTER TABLE sub-command to the open catalog statement (module docs).
fn apply_command(
    ddl: &mut CatalogTxn<'_>,
    table: Oid,
    cmd: &AlterTableCmd,
    rewrites: &mut Vec<ColumnRewrite>,
) -> Result<()> {
    match cmd {
        AlterTableCmd::AddColumn { spec, if_not_exists } => {
            if let Some(attnum) = ddl.add_column(table, spec, *if_not_exists)? {
                if let Some(rw) = add_column_rewrite(spec, attnum) {
                    rewrites.push(rw);
                }
            }
        }
        AlterTableCmd::DropColumn { name, cascade, missing_ok } => {
            ddl.drop_column(table, name, behavior(*cascade), *missing_ok)?;
        }
        AlterTableCmd::RenameColumn { old, new } => {
            ddl.rename_column(table, old, new, &pgarbor_sql::rewrite::SqlExprRewriter)?;
        }
        AlterTableCmd::AlterColumnType {
            column,
            new_type,
            new_collation,
            new_ndims,
            using,
            new_default,
            needs_rewrite,
        } => {
            let old_missing = if *needs_rewrite {
                let snap = ddl.snapshot()?;
                snap.relation(table)
                    .and_then(|r| r.column_by_name(column))
                    .and_then(|c| c.missing_value.clone().map(|m| (m, c.type_id)))
            } else {
                None
            };
            ddl.alter_column_type(
                table,
                column,
                *new_type,
                *new_collation,
                *new_ndims,
                new_default.clone(),
                *needs_rewrite,
            )?;
            if *needs_rewrite {
                let (attnum, _) = column_info(ddl, table, column)?;
                let expr = using.clone().ok_or_else(|| {
                    PgError::internal("ALTER COLUMN TYPE needs a row rewrite but has no USING expression")
                })?;
                rewrites.push(ColumnRewrite::Using { attnum, expr, old_missing });
            }
        }
        AlterTableCmd::SetDefault { column, expr } => {
            ddl.set_column_default(table, column, expr.clone())?;
        }
        AlterTableCmd::SetNotNull { column } => {
            ddl.set_not_null(table, column)?;
        }
        AlterTableCmd::DropNotNull { column } => {
            ddl.drop_not_null(table, column)?;
        }
        AlterTableCmd::AddIdentity { column, identity } => {
            ddl.add_identity(table, column, identity)?;
        }
        AlterTableCmd::SetIdentity { column, kind, options } => {
            ddl.set_identity(table, column, *kind, options)?;
        }
        AlterTableCmd::DropIdentity { column, missing_ok } => {
            ddl.drop_identity(table, column, *missing_ok)?;
        }
        AlterTableCmd::SetExpression { column, expr } => {
            ddl.set_generation_expression(table, column, expr.clone())?;
            let (attnum, stored) = column_info(ddl, table, column)?;
            if stored {
                rewrites.push(ColumnRewrite::Generated { attnum });
            }
        }
        AlterTableCmd::DropExpression { column, missing_ok } => {
            ddl.drop_expression(table, column, *missing_ok)?;
        }
        AlterTableCmd::SetColumnAttribute { column, attr } => {
            ddl.set_column_attribute(table, column, attr)?;
        }
        AlterTableCmd::AddConstraint(spec) => {
            reject_deferred(spec)?;
            ddl.add_constraint(table, spec)?;
        }
        AlterTableCmd::DropConstraint { name, cascade, missing_ok } => {
            ddl.drop_constraint(table, name, behavior(*cascade), *missing_ok)?;
        }
        AlterTableCmd::RenameConstraint { old, new } => {
            ddl.rename_constraint(table, old, new)?;
        }
        AlterTableCmd::ValidateConstraint { name } => {
            ddl.validate_constraint(table, name)?;
        }
        AlterTableCmd::AlterConstraint { name, deferrable, initially_deferred, enforced } => {
            if *deferrable == Some(true) || *initially_deferred == Some(true) {
                return Err(PgError::unimplemented("EXE-33", "deferred constraint enforcement"));
            }
            ddl.alter_constraint(table, name, *deferrable, *initially_deferred, *enforced)?;
        }
        AlterTableCmd::SetOptions { set, reset } => {
            ddl.set_relation_options(table, set, reset)?;
        }
        AlterTableCmd::SetLogged(logged) => {
            // `SetLogged(true)` = SET LOGGED, `false` = SET UNLOGGED.
            let persistence = if *logged { Persistence::Permanent } else { Persistence::Unlogged };
            ddl.set_persistence(table, persistence)?;
        }
        AlterTableCmd::ReplicaIdentity(setting) => {
            ddl.set_replica_identity(table, setting)?;
        }
        AlterTableCmd::RowSecurity { enable, force } => {
            if *enable == Some(true) || *force == Some(true) {
                return Err(PgError::unimplemented("EXE-33", "row security policy enforcement"));
            }
            ddl.set_row_security(table, *enable, *force)?;
        }
        AlterTableCmd::ClusterOn { index } => {
            // cluster.c `mark_index_clustered`: the chosen index gets the flag, its siblings lose it.
            let changes: Vec<(Oid, bool)> = ddl
                .snapshot()?
                .indexes_of(table)
                .into_iter()
                .filter(|i| i.clustered != (Some(i.oid) == *index))
                .map(|i| (i.oid, Some(i.oid) == *index))
                .collect();
            for (oid, flag) in changes {
                ddl.update_index(oid, |i| {
                    i.clustered = flag;
                    Ok(())
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;
    use pgarbor_catalog::ddl::spec::{ConstraintSpec, ConstraintSpecKind};
    use pgarbor_catalog::{testing, CatalogEnv, RelKind, StoredExpr};
    use pgarbor_common::SqlState;
    use pgarbor_types::context::IoConfig;
    use pgarbor_types::TypeId;
    use std::cell::Cell;

    /// A fresh bootstrapped catalog in a `MemTxn`.
    fn setup() -> (MemTxn, CatalogEnv) {
        let (kv, env) = testing::bootstrapped().expect("bootstrap");
        let mut txn = MemTxn::new();
        txn.kv = kv;
        (txn, env)
    }

    fn table_spec(env: &CatalogEnv, name: &str, columns: Vec<ColumnSpec>, if_not_exists: bool) -> CreateTableSpec {
        CreateTableSpec {
            namespace: env.creation_namespace().expect("namespace"),
            name: name.to_string(),
            owner: env.current_user(),
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns,
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists,
            inherits: Vec::new(),
        }
    }

    fn table_oid(env: &CatalogEnv, name: &str) -> Oid {
        let ns = env.creation_namespace().expect("namespace");
        env.snapshot().class_in_namespace(ns, name).expect("table exists").oid()
    }

    fn alter(table: Oid, commands: Vec<AlterTableCmd>) -> BoundAlterTable {
        BoundAlterTable { relation: table, keyword: AlterRelKeyword::Table, commands }
    }

    #[test]
    fn create_table_then_if_not_exists_skips_with_notice() {
        let (mut txn, env) = setup();
        let rng = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);

        let spec = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], false);
        let res = exec_create_table(&mut cx, &spec).expect("create table");
        assert_eq!(res.tag(), "CREATE TABLE");
        let oid = table_oid(&cx.se.env, "t");
        assert!(cx.se.env.snapshot().relation(oid).is_some());

        let again = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], true);
        let res2 = exec_create_table(&mut cx, &again).expect("create if not exists");
        assert_eq!(res2.tag(), "CREATE TABLE");
        let notes = cx.se.take_notices();
        assert!(notes.iter().any(|n| n.message == "relation \"t\" already exists, skipping"));
    }

    #[test]
    fn alter_add_column_rename_and_duplicate_error() {
        let (mut txn, env) = setup();
        let rng = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);

        let spec = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], false);
        exec_create_table(&mut cx, &spec).expect("create table");
        let t = table_oid(&cx.se.env, "t");

        let stmt = alter(
            t,
            vec![
                AlterTableCmd::AddColumn { spec: ColumnSpec::new("b", TypeId::INT4), if_not_exists: false },
                AlterTableCmd::RenameColumn { old: "b".into(), new: "bb".into() },
            ],
        );
        let res = exec_alter_table(&mut cx, &stmt).expect("alter table");
        assert_eq!(res.tag(), "ALTER TABLE");
        {
            let rel = cx.se.env.snapshot().relation(t).expect("relation");
            assert!(rel.column_by_name("bb").is_some());
            assert!(rel.column_by_name("b").is_none());
        }

        let dup = alter(
            t,
            vec![AlterTableCmd::AddColumn { spec: ColumnSpec::new("a", TypeId::INT4), if_not_exists: false }],
        );
        let err = exec_alter_table(&mut cx, &dup).expect_err("duplicate column must fail");
        assert_eq!(err.sqlstate, SqlState::DUPLICATE_COLUMN);
    }

    #[test]
    fn alter_add_column_constant_default_is_stored_as_missing_value() {
        let (mut txn, env) = setup();
        let rng = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);

        let spec = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], false);
        exec_create_table(&mut cx, &spec).expect("create table");
        let t = table_oid(&cx.se.env, "t");

        let mut d = ColumnSpec::new("d", TypeId::INT4);
        d.default = Some(StoredExpr { sql: "7".into(), bound: None, depends_on: Vec::new() });
        d.fast_default = Some("7".into());
        let stmt = alter(t, vec![AlterTableCmd::AddColumn { spec: d, if_not_exists: false }]);
        exec_alter_table(&mut cx, &stmt).expect("add column with constant default");
        let rel = cx.se.env.snapshot().relation(t).expect("relation");
        assert!(rel.column_by_name("d").expect("d").missing_value.is_some());
    }

    #[test]
    fn alter_add_not_valid_check_constraint() {
        let (mut txn, env) = setup();
        let rng = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);

        let spec = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], false);
        exec_create_table(&mut cx, &spec).expect("create table");
        let t = table_oid(&cx.se.env, "t");

        let check = ConstraintSpecKind::Check {
            expr: StoredExpr { sql: "(a > 0)".into(), bound: None, depends_on: Vec::new() },
            columns: vec!["a".into()],
            no_inherit: false,
        };
        let mut cs = ConstraintSpec::new(check);
        cs.not_valid = true;
        let stmt = alter(t, vec![AlterTableCmd::AddConstraint(cs)]);
        let res = exec_alter_table(&mut cx, &stmt).expect("add not valid check");
        assert_eq!(res.tag(), "ALTER TABLE");
    }

    #[test]
    #[ignore = "needs EXE-34"]
    fn alter_column_type_using_rewrites_rows() {
        let (mut txn, env) = setup();
        let rng = Cell::new(1u64);
        let guc = NoGuc;
        let services = NoServices;
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);

        let spec = table_spec(&cx.se.env, "t", vec![ColumnSpec::new("a", TypeId::INT4)], false);
        exec_create_table(&mut cx, &spec).expect("create table");
        let t = table_oid(&cx.se.env, "t");
        // The binder supplies the USING expression; a real test needs bound expressions and rows.
        let stmt = alter(
            t,
            vec![AlterTableCmd::AlterColumnType {
                column: "a".into(),
                new_type: TypeId::INT8,
                new_collation: None,
                new_ndims: 0,
                using: None,
                new_default: None,
                needs_rewrite: false,
            }],
        );
        let res = exec_alter_table(&mut cx, &stmt).expect("alter type");
        assert_eq!(res.tag(), "ALTER TABLE");
    }
}

fn reject_deferred(spec: &pgarbor_catalog::ddl::spec::ConstraintSpec) -> Result<()> {
    if spec.deferrable || spec.initially_deferred {
        return Err(PgError::unimplemented("EXE-33", "deferred constraint enforcement"));
    }
    Ok(())
}
