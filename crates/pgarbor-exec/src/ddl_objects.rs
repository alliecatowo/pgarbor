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
use pgarbor_catalog::ddl::spec::{
    CreateCompositeSpec, CreateDomainSpec, CreateEnumSpec, CreateRoleSpec, CreateSequenceSpec, GrantSpec, IndexSpec,
    MembershipSpec,
};
use pgarbor_catalog::ObjectAddress;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_sql::ir::{BoundCreateDatabase, BoundCreateFunction, BoundCreateView};

use crate::context::ExecContext;
use crate::result::ExecResult;

#[allow(unused_variables)]
pub fn exec_create_schema(cx: &mut ExecContext, name: &str, owner: Oid, if_not_exists: bool) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE SCHEMA (ddl_objects::exec_create_schema)"))
}

#[allow(unused_variables)]
pub fn exec_create_index(cx: &mut ExecContext, spec: &IndexSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE INDEX (ddl_objects::exec_create_index)"))
}

#[allow(unused_variables)]
pub fn exec_create_sequence(cx: &mut ExecContext, spec: &CreateSequenceSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE SEQUENCE (ddl_objects::exec_create_sequence)"))
}

#[allow(unused_variables)]
pub fn exec_create_view(cx: &mut ExecContext, v: &BoundCreateView) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE VIEW (ddl_objects::exec_create_view)"))
}

#[allow(unused_variables)]
pub fn exec_create_enum(cx: &mut ExecContext, spec: &CreateEnumSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE TYPE AS ENUM (ddl_objects::exec_create_enum)"))
}

#[allow(unused_variables)]
pub fn exec_create_domain(cx: &mut ExecContext, spec: &CreateDomainSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE DOMAIN (ddl_objects::exec_create_domain)"))
}

#[allow(unused_variables)]
pub fn exec_create_composite(cx: &mut ExecContext, spec: &CreateCompositeSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE TYPE AS (ddl_objects::exec_create_composite)"))
}

#[allow(unused_variables)]
pub fn exec_create_function(cx: &mut ExecContext, f: &BoundCreateFunction) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE FUNCTION (ddl_objects::exec_create_function)"))
}

#[allow(unused_variables)]
pub fn exec_create_role(cx: &mut ExecContext, spec: &CreateRoleSpec) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE ROLE (ddl_objects::exec_create_role)"))
}

#[allow(unused_variables)]
pub fn exec_create_database(cx: &mut ExecContext, d: &BoundCreateDatabase) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "CREATE DATABASE (ddl_objects::exec_create_database)"))
}

#[allow(unused_variables)]
pub fn exec_comment(cx: &mut ExecContext, addr: &ObjectAddress, text: Option<&str>) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "COMMENT ON (ddl_objects::exec_comment)"))
}

#[allow(unused_variables)]
pub fn exec_grant(cx: &mut ExecContext, specs: &[GrantSpec], revoke: bool) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "GRANT/REVOKE (ddl_objects::exec_grant)"))
}

#[allow(unused_variables)]
pub fn exec_grant_role(cx: &mut ExecContext, specs: &[MembershipSpec]) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "GRANT role (ddl_objects::exec_grant_role)"))
}

#[allow(unused_variables)]
pub fn exec_revoke_role(cx: &mut ExecContext, specs: &[MembershipSpec], admin_option_only: bool, cascade: bool) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "REVOKE role (ddl_objects::exec_revoke_role)"))
}

#[allow(unused_variables)]
pub fn exec_refresh_matview(cx: &mut ExecContext, view: Oid, with_data: bool, concurrently: bool) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-35", "REFRESH MATERIALIZED VIEW (ddl_objects::exec_refresh_matview)"))
}
