//! CREATE statements other than tables (lease EXE-35): schema, index, sequence, view, enum, domain,
//! composite type, function, role, database; COMMENT; GRANT/REVOKE. Each function is ONE `run_ddl`
//! with the catalog method named below, then `ddl_followup::run_followups` for the outcome's
//! follow-ups, and returns `ExecResult::Command{tag}` with the statement tag (`BoundStatement::tag()`).
//! `IF NOT EXISTS` skips return the same tag; the NOTICE was queued by the catalog.
//!
//! | function | catalog call | data work |
//! |---|---|---|
//! | `exec_create_schema(name, owner, if_not_exists)` | `create_schema` | - |
//! | `exec_create_index(spec)` | `create_index(&spec)` (`Ok(None)` = IF NOT EXISTS skip) | `FollowUp::BuildIndex` -> `index_maint::build_index`; duplicate keys: 23505 `could not create unique index "i"` DETAIL `Key (a)=(1) is duplicated.`; `concurrently` is a plain build |
//! | `exec_create_sequence(spec)` | `create_sequence(&spec)` | `CreateSequenceState` -> `seq_create` |
//! | `exec_create_view(v)` | `create_view(&v.spec)`; materialized + `populated` -> `FollowUp::PopulateMatView` | `ctas::populate_matview` |
//! | `exec_create_enum/domain/composite` | `create_enum`, `create_domain`, `create_composite` | - |
//! | `exec_create_function(f)` | `create_function(&f.function, f.or_replace)` | stored, never executed in v1 |
//! | `exec_create_role(spec)` | `create_role(&spec)` | - |
//! | `exec_create_database(d)` | 0A000 `CREATE DATABASE` (core amendment 02: single database per branch) | - |
//! | `exec_comment(addr, text)` | `comment_on(&addr, text.as_deref())` (tag `COMMENT`) | - |
//! | `exec_grant(specs, revoke)` | `grant_object_privileges(spec, revoke)` per spec, ONE run_ddl (tag `GRANT`/`REVOKE`) | - |
//! | `exec_grant_role(specs)` / `exec_revoke_role(specs, admin_only, cascade)` | `grant_role` / `revoke_role` per spec (tags `GRANT ROLE` / `REVOKE ROLE`) | - |
//! | `exec_refresh_matview(view, with_data, concurrently)` | `refresh_matview(view, with_data)` (+ `check_refresh_concurrently` when `concurrently`) | `PopulateMatView` when `with_data`: delete all rows + indexes data, repopulate, rebuild indexes |
//! All privilege checks happen inside the catalog calls (42501 `permission denied ...`).
//!
//! Tags follow `BoundStatement::tag()` in `pgarbor-sql` (`ir/stmt.rs`): `CREATE VIEW` vs
//! `CREATE MATERIALIZED VIEW` by `ViewSpec::materialized`, `CREATE FUNCTION` vs `CREATE PROCEDURE`
//! by `FuncKind::Procedure`, `CREATE TYPE` for enums and composites.
//!
//! PG references: `commands/schemacmds.c`, `commands/indexcmds.c`, `commands/sequence.c`,
//! `commands/view.c`, `commands/matview.c` (`ExecRefreshMatView`), `commands/typecmds.c`,
//! `commands/functioncmds.c`, `commands/user.c`, `commands/comment.c`, `catalog/aclchk.c`.
use pgarbor_catalog::ddl::spec::{
    CreateCompositeSpec, CreateDomainSpec, CreateEnumSpec, CreateRoleSpec, CreateSequenceSpec, GrantSpec, IndexSpec,
    MembershipSpec,
};
use pgarbor_catalog::model::FuncKind;
use pgarbor_catalog::{CatalogTxn, ObjectAddress};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::ir::{BoundCreateDatabase, BoundCreateFunction, BoundCreateView};

use crate::context::ExecContext;
use crate::ddl_driver::run_ddl;
use crate::ddl_followup::run_followups;
use crate::result::ExecResult;

/// Run one catalog statement, execute its data follow-ups, and report the command tag.
fn ddl_command<T>(cx: &mut ExecContext, tag: &str, f: impl FnOnce(&mut CatalogTxn) -> Result<T>) -> Result<ExecResult> {
    let (_, outcome) = run_ddl(cx, f)?;
    run_followups(cx, &outcome.follow_ups, None, &[])?;
    Ok(ExecResult::command(tag))
}

/// Statement tag of CREATE [MATERIALIZED] VIEW (`BoundStatement::tag`).
fn view_tag(materialized: bool) -> &'static str {
    if materialized {
        "CREATE MATERIALIZED VIEW"
    } else {
        "CREATE VIEW"
    }
}

/// Statement tag of CREATE FUNCTION / CREATE PROCEDURE (`BoundStatement::tag`).
fn function_tag(kind: &FuncKind) -> &'static str {
    match kind {
        FuncKind::Procedure => "CREATE PROCEDURE",
        _ => "CREATE FUNCTION",
    }
}

pub fn exec_create_schema(cx: &mut ExecContext, name: &str, owner: Oid, if_not_exists: bool) -> Result<ExecResult> {
    ddl_command(cx, "CREATE SCHEMA", |ddl| ddl.create_schema(name, owner, if_not_exists))
}

pub fn exec_create_index(cx: &mut ExecContext, spec: &IndexSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE INDEX", |ddl| ddl.create_index(spec))
}

pub fn exec_create_sequence(cx: &mut ExecContext, spec: &CreateSequenceSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE SEQUENCE", |ddl| ddl.create_sequence(spec))
}

pub fn exec_create_view(cx: &mut ExecContext, v: &BoundCreateView) -> Result<ExecResult> {
    let (_, outcome) = run_ddl(cx, |ddl| ddl.create_view(&v.spec))?;
    let mut populated = None;
    for follow_up in &outcome.follow_ups {
        match follow_up {
            pgarbor_catalog::FollowUp::PopulateMatView { relation } => {
                populated = Some(crate::ctas::populate_matview(cx, *relation)?);
            }
            other => run_followups(cx, std::slice::from_ref(other), None, &[])?,
        }
    }
    Ok(ExecResult::command(match populated {
        Some(n) => format!("SELECT {n}"),
        None => view_tag(v.spec.materialized).to_string(),
    }))
}

pub fn exec_create_enum(cx: &mut ExecContext, spec: &CreateEnumSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE TYPE", |ddl| ddl.create_enum(spec))
}

pub fn exec_create_domain(cx: &mut ExecContext, spec: &CreateDomainSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE DOMAIN", |ddl| ddl.create_domain(spec))
}

pub fn exec_create_composite(cx: &mut ExecContext, spec: &CreateCompositeSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE TYPE", |ddl| ddl.create_composite(spec))
}

pub fn exec_create_function(cx: &mut ExecContext, f: &BoundCreateFunction) -> Result<ExecResult> {
    ddl_command(cx, function_tag(&f.function.kind), |ddl| ddl.create_function(&f.function, f.or_replace))
}

pub fn exec_create_role(cx: &mut ExecContext, spec: &CreateRoleSpec) -> Result<ExecResult> {
    ddl_command(cx, "CREATE ROLE", |ddl| ddl.create_role(spec))
}

#[allow(unused_variables)]
pub fn exec_create_database(cx: &mut ExecContext, d: &BoundCreateDatabase) -> Result<ExecResult> {
    // Core amendment 02: one logical database per branch; CREATE DATABASE stays unimplemented (0A000).
    Err(PgError::unimplemented("EXE-35", "CREATE DATABASE"))
}

pub fn exec_comment(cx: &mut ExecContext, addr: &ObjectAddress, text: Option<&str>) -> Result<ExecResult> {
    ddl_command(cx, "COMMENT", |ddl| ddl.comment_on(addr, text))
}

pub fn exec_grant(cx: &mut ExecContext, specs: &[GrantSpec], revoke: bool) -> Result<ExecResult> {
    let tag = if revoke { "REVOKE" } else { "GRANT" };
    ddl_command(cx, tag, |ddl| {
        for spec in specs {
            ddl.grant_object_privileges(spec, revoke)?;
        }
        Ok(())
    })
}

pub fn exec_grant_role(cx: &mut ExecContext, specs: &[MembershipSpec]) -> Result<ExecResult> {
    ddl_command(cx, "GRANT ROLE", |ddl| {
        for spec in specs {
            ddl.grant_role(spec)?;
        }
        Ok(())
    })
}

pub fn exec_revoke_role(cx: &mut ExecContext, specs: &[MembershipSpec], admin_option_only: bool, cascade: bool) -> Result<ExecResult> {
    ddl_command(cx, "REVOKE ROLE", |ddl| {
        for spec in specs {
            ddl.revoke_role(spec, admin_option_only, cascade)?;
        }
        Ok(())
    })
}

pub fn exec_refresh_matview(cx: &mut ExecContext, view: Oid, with_data: bool, concurrently: bool) -> Result<ExecResult> {
    ddl_command(cx, "REFRESH MATERIALIZED VIEW", |ddl| {
        if concurrently {
            // PG (`ExecRefreshMatView`): not-populated is checked first, then the option conflict (42601),
            // then the unique-index requirement (inside `check_refresh_concurrently`).
            if !with_data {
                let snap = ddl.snapshot()?;
                let populated = snap.relation(view).and_then(|r| r.view.as_ref()).map(|v| v.populated).unwrap_or(false);
                if populated {
                    return Err(PgError::new(
                        SqlState::SYNTAX_ERROR,
                        "CONCURRENTLY and WITH NO DATA options cannot be used together",
                    ));
                }
            }
            ddl.check_refresh_concurrently(view)?;
        }
        ddl.refresh_matview(view, with_data)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use pgarbor_catalog::CatalogEnv;
    use pgarbor_types::context::IoConfig;

    use crate::context::{NoGuc, NoServices, SessionEnv};
    use crate::memtxn::MemTxn;

    /// A transaction whose kv holds a bootstrapped catalog, plus the matching env.
    fn setup() -> (MemTxn, CatalogEnv) {
        let (kv, env) = pgarbor_catalog::testing::bootstrapped().expect("bootstrap catalog");
        let mut txn = MemTxn::new();
        txn.kv = kv;
        (txn, env)
    }

    #[test]
    fn view_and_function_tags() {
        assert_eq!(view_tag(false), "CREATE VIEW");
        assert_eq!(view_tag(true), "CREATE MATERIALIZED VIEW");
        assert_eq!(function_tag(&FuncKind::Procedure), "CREATE PROCEDURE");
        assert_eq!(function_tag(&FuncKind::Function), "CREATE FUNCTION");
        assert_eq!(function_tag(&FuncKind::Window), "CREATE FUNCTION");
    }

    #[test]
    fn create_database_is_feature_not_supported() {
        let (mut txn, env) = setup();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        let d = BoundCreateDatabase {
            name: "other".to_string(),
            owner: 10,
            template: None,
            encoding: None,
            locale: None,
            connection_limit: -1,
            is_template: false,
            allow_connections: true,
            if_not_exists: false,
        };
        match exec_create_database(&mut cx, &d) {
            Ok(_) => panic!("CREATE DATABASE must not succeed"),
            Err(e) => assert!(e.sqlstate == SqlState::FEATURE_NOT_SUPPORTED),
        }
    }

    #[test]
    #[ignore = "needs EXE-34"]
    fn create_schema_returns_tag() {
        let (mut txn, env) = setup();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        match exec_create_schema(&mut cx, "s1", 10, false) {
            Ok(r) => assert_eq!(r.tag(), "CREATE SCHEMA"),
            Err(e) => panic!("unexpected error: {}", e.message),
        }
    }

    #[test]
    #[ignore = "needs EXE-34"]
    fn create_enum_returns_tag() {
        let (mut txn, env) = setup();
        let guc = NoGuc;
        let services = NoServices;
        let rng = Cell::new(1u64);
        let se = SessionEnv::new(env, IoConfig::default(), &guc, &services, &rng, 0, 0);
        let mut cx = ExecContext::new(&mut txn, se);
        // Oid 2200 is the `public` schema of a bootstrapped catalog; 10 is the bootstrap superuser.
        let spec = CreateEnumSpec {
            namespace: 2200,
            name: "mood".to_string(),
            owner: 10,
            labels: vec!["sad".to_string(), "ok".to_string()],
        };
        match exec_create_enum(&mut cx, &spec) {
            Ok(r) => assert_eq!(r.tag(), "CREATE TYPE"),
            Err(e) => panic!("unexpected error: {}", e.message),
        }
    }
}
