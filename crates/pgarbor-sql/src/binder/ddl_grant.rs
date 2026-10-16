//! GRANT / REVOKE of privileges and role membership (SQL-39). PG references: `aclchk.c`
//! (`ExecuteGrantStmt`, `objectNamesToOids`, `expand_all_col_privileges`), `user.c`
//! (`GrantRole`, `AddRoleMems`), `gram.y` (`GrantStmt`, `RevokeStmt`, `GrantRoleStmt`,
//! `privilege_target`, `grantee`).
//!
//! `bind_grant(b, stmt)` handles `Statement::Grant(Grant { privileges, objects, grantees,
//! with_grant_option, as_grantor, granted_by, current_grants })` and `Statement::Revoke(Revoke {
//! grant_option_for, privileges, objects, grantees, granted_by, cascade })`:
//! * Privileges: `ALL [PRIVILEGES]` -> `PrivilegeSet::All`; else the union of `Action`s as ACL bits via
//!   `pgarbor_catalog::model::acl_mode`. A privilege invalid for the object kind: 0LP01 `invalid
//!   privilege type TRUNCATE for sequence`/`... for schema` etc. (`ExecuteGrantStmt`). Column lists
//!   `SELECT (a, b)` -> `GrantSpec.columns` (attnums; table targets only: 0LP01 `column privileges are
//!   only valid for relations` otherwise).
//! * Objects (`GrantObjects`): `Tables(names)` (no `TABLE` keyword = tables, views, sequences allowed
//!   by name), `Sequences`, `Schemas`, `Databases`, `Function`/`Procedure` (with argument types),
//!   `AllTablesInSchema`/`AllSequencesInSchema`/`AllFunctionsInSchema`. Kinds that PostgreSQL rejects
//!   as syntax (`VIEW`, `ALL VIEWS IN SCHEMA`, Snowflake kinds) -> 42601.
//!   One `GrantSpec` per named object (`GrantTarget::Object(ObjectAddress)`) or per `ALL ... IN SCHEMA`
//!   target; every spec lists ALL grantees.
//! * Grantees: role names, `PUBLIC` (OID 0), `CURRENT_USER`/`CURRENT_ROLE`/`SESSION_USER`; unknown role
//!   42704 `role "x" does not exist`. `WITH GRANT OPTION` -> `grant_option`; `GRANTED BY r` -> `grantor`
//!   (must be the current user, else 0A000 `grantor must be current user`; default current user).
//!   REVOKE: `GRANT OPTION FOR` -> `grant_option = true`, `CASCADE` -> `cascade`.
//! * Order of checks follows `ExecuteGrantStmt`: grantor, objects, grantees, then privilege validity.
//!
//! `ALTER DEFAULT PRIVILEGES` is an `ExtraStatement::Unsupported`.
//!
//! `bind_grant_role`: `GRANT r1, r2 TO m1, m2 [WITH ADMIN OPTION | ...] [GRANTED BY g]` ->
//! `GrantRole(Vec<MembershipSpec>)`, `REVOKE [ADMIN OPTION FOR] r FROM m [CASCADE]` -> `RevokeRole`.
//! NOT REACHABLE with sqlparser 0.63: its `parse_grant_permission` rejects a bare role name in the
//! privilege list, so no `Statement` value represents the role form. See the function's body.
//! Self-membership and other membership rules are enforced by the catalog (`ddl::spec::MembershipSpec`).
use pgarbor_catalog::ddl::spec::{GrantSpec, GrantTarget, MembershipSpec, PrivilegeSet};
use pgarbor_catalog::model::acl_mode;
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::{errors as cat_errors, CatalogEnv, ObjClass, ObjectAddress, RelKind};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::catalog_data::ProcKind;
use pgarbor_types::TypeId;
use sqlparser::ast::{
    Action, CascadeOption, DataType, GrantObjects, Grantee, GranteeName, GranteesType, Ident, ObjectName,
    ObjectNamePart, Privileges, Statement,
};

use super::context::Binder;
use super::dispatch::GRANT_ROUTINE_KEY;
use super::types;
use crate::diag;
use crate::ir::BoundStatement;
use crate::names::QualName;

/// SQLSTATE 0LP01 `invalid_grant_operation` (also the SQLSTATE of the privilege WARNINGs).
const INVALID_GRANT_OPERATION: SqlState = SqlState(*b"0LP01");

/// Routine kinds a `ON FUNCTION|PROCEDURE|ROUTINE` object names: `Function` (not procedures),
/// `Procedure` (procedures only), `Any` (`ROUTINE`, PG `OBJECT_ROUTINE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoutineWant {
    Function,
    Procedure,
    Any,
}

/// The statement's `objtype` (PG `GrantStmt.objtype`): decides the top-level privilege check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjType {
    /// `TABLE` / no keyword: relations and sequences (`ALL_RELATION | ALL_SEQUENCE`).
    Table,
    Sequence,
    Database,
    Schema,
    Function,
    Procedure,
}

impl ObjType {
    /// Privileges valid for the statement kind (`ExecuteGrantStmt` `all_privileges`).
    fn all_privileges(self) -> u32 {
        match self {
            ObjType::Table => acl_mode::ALL_RELATION | acl_mode::ALL_SEQUENCE,
            ObjType::Sequence => acl_mode::ALL_SEQUENCE,
            ObjType::Database => acl_mode::ALL_DATABASE,
            ObjType::Schema => acl_mode::ALL_SCHEMA,
            ObjType::Function | ObjType::Procedure => acl_mode::ALL_FUNCTION,
        }
    }

    /// The word in `invalid privilege type X for <word>` (PG `errormsg` of `ExecuteGrantStmt`).
    fn word(self) -> &'static str {
        match self {
            ObjType::Table => "relation",
            ObjType::Sequence => "sequence",
            ObjType::Database => "database",
            ObjType::Schema => "schema",
            ObjType::Function => "function",
            ObjType::Procedure => "procedure",
        }
    }
}

/// One target of the statement resolved to OIDs (a single object, or an `ALL ... IN SCHEMA` group).
struct Resolved {
    target: GrantTarget,
    /// Unqualified name used in WARNING / ERROR texts (`NameStr(relname)` in PG).
    name: String,
    /// A sequence (`relkind = 'S'`) addressed through a table-kind target.
    is_sequence: bool,
    /// Relation OID for column privileges (`None` for sequences, schemas, databases, routines).
    relation: Option<Oid>,
}

/// Grant parameters shared by every spec of one statement.
struct SpecBase {
    grantees: Vec<Oid>,
    grant_option: bool,
    grantor: Oid,
    cascade: bool,
}

impl SpecBase {
    fn spec(&self, target: &GrantTarget, privileges: PrivilegeSet, columns: Vec<i16>) -> GrantSpec {
        GrantSpec {
            target: target.clone(),
            privileges,
            grantees: self.grantees.clone(),
            grant_option: self.grant_option,
            grantor: self.grantor,
            columns,
            cascade: self.cascade,
        }
    }
}

/// The GRANT or REVOKE fields the shared implementation needs.
struct GrantInput<'s> {
    revoke: bool,
    privileges: &'s Privileges,
    objects: Option<&'s GrantObjects>,
    grantees: &'s [Grantee],
    /// GRANT: `WITH GRANT OPTION`; REVOKE: `GRANT OPTION FOR`.
    grant_option: bool,
    granted_by: Option<&'s Ident>,
    cascade: bool,
}

/// `GRANT`/`REVOKE` on objects.
pub fn bind_grant(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::Grant(g) => {
            if g.as_grantor.is_some() {
                // PostgreSQL has no `GRANT ... AS grantor`.
                return Err(diag::syntax_error_near("AS", None));
            }
            if let Some(kind) = &g.current_grants {
                let token = match kind {
                    sqlparser::ast::CurrentGrantsKind::CopyCurrentGrants => "COPY",
                    sqlparser::ast::CurrentGrantsKind::RevokeCurrentGrants => "REVOKE",
                };
                return Err(diag::syntax_error_near(token, None));
            }
            bind_grant_input(
                b,
                GrantInput {
                    revoke: false,
                    privileges: &g.privileges,
                    objects: g.objects.as_ref(),
                    grantees: &g.grantees,
                    grant_option: g.with_grant_option,
                    granted_by: g.granted_by.as_ref(),
                    cascade: false,
                },
            )
        }
        Statement::Revoke(r) => bind_grant_input(
            b,
            GrantInput {
                revoke: true,
                privileges: &r.privileges,
                objects: r.objects.as_ref(),
                grantees: &r.grantees,
                grant_option: r.grant_option_for,
                granted_by: r.granted_by.as_ref(),
                cascade: matches!(r.cascade, Some(CascadeOption::Cascade)),
            },
        ),
        _ => Err(PgError::internal("binder: bind_grant called with a statement that is not GRANT or REVOKE")),
    }
}

/// `GRANT role TO member` / `REVOKE role FROM member` (called by `bind_grant` when the statement is the
/// role form).
///
/// Not implemented: sqlparser 0.63 has no `Statement` for the role form. `parse_grant_permission` only
/// accepts privilege keywords, so `GRANT r1 TO r2` never reaches the binder. The membership rules
/// (grantor, `WITH ADMIN | INHERIT | SET` options, `REVOKE ... CASCADE`) need a parser hook that
/// yields a role-membership statement (an `ExtraStatement` variant) before this function can do work.
pub fn bind_grant_role(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let _ = (b, stmt);
    Err(PgError::unimplemented(
        "SQL-39",
        "role membership GRANT/REVOKE (ddl_grant::bind_grant_role): sqlparser 0.63 has no AST for the role form",
    ))
}

/// `ExtraStatement::GrantRole`: `GRANT r1, r2 TO m1, m2 [WITH ADMIN|INHERIT|SET ..] [GRANTED BY g]`, one
/// [`MembershipSpec`] per (role, member) pair. Order of checks follows `GrantRole` in user.c: option
/// names, grantor, grantees, then the granted roles.
pub fn bind_grant_role_extra(
    b: &mut Binder,
    roles: &[Ident],
    grantees: &[Ident],
    options: &[(Ident, bool)],
    grantor: Option<&Ident>,
) -> Result<BoundStatement> {
    let (mut admin_option, mut inherit_option, mut set_option) = (false, None, true);
    for (name, value) in options {
        match name.value.to_ascii_lowercase().as_str() {
            "admin" => admin_option = *value,
            "inherit" => inherit_option = Some(*value),
            "set" => set_option = *value,
            _ => return Err(unrecognized_role_option(b, name)),
        }
    }
    let grantor = match grantor {
        Some(id) => role_from_ident(b, id)?,
        None => 0,
    };
    let mut members: Vec<Oid> = Vec::with_capacity(grantees.len());
    for g in grantees {
        members.push(role_from_ident(b, g)?);
    }
    let mut specs = Vec::with_capacity(roles.len() * members.len());
    for r in roles {
        let role = role_from_ident(b, r)?;
        for &member in &members {
            specs.push(MembershipSpec { role, member, grantor, admin_option, inherit_option, set_option });
        }
    }
    Ok(BoundStatement::GrantRole(specs))
}

/// `ExtraStatement::RevokeRole`: `REVOKE [ADMIN OPTION FOR] r FROM m [GRANTED BY g] [CASCADE]`. Only the
/// `ADMIN OPTION FOR` form is executed; `INHERIT` / `SET OPTION FOR` are 0A000.
pub fn bind_revoke_role_extra(
    b: &mut Binder,
    option_for: Option<&Ident>,
    roles: &[Ident],
    grantees: &[Ident],
    grantor: Option<&Ident>,
    cascade: bool,
) -> Result<BoundStatement> {
    let mut admin_option_only = false;
    if let Some(name) = option_for {
        match name.value.to_ascii_lowercase().as_str() {
            "admin" => admin_option_only = true,
            "inherit" | "set" => {
                return Err(diag::unsupported_construct("REVOKE INHERIT/SET OPTION FOR role membership", b.pos_span(name.span)))
            }
            _ => return Err(unrecognized_role_option(b, name)),
        }
    }
    let grantor = match grantor {
        Some(id) => role_from_ident(b, id)?,
        None => 0,
    };
    let mut members: Vec<Oid> = Vec::with_capacity(grantees.len());
    for g in grantees {
        members.push(role_from_ident(b, g)?);
    }
    let mut specs = Vec::with_capacity(roles.len() * members.len());
    for r in roles {
        let role = role_from_ident(b, r)?;
        for &member in &members {
            specs.push(MembershipSpec { role, member, grantor, admin_option: false, inherit_option: None, set_option: true });
        }
    }
    Ok(BoundStatement::RevokeRole { specs, admin_option_only, cascade })
}

/// 42601 `unrecognized role option "x"` at the option name.
fn unrecognized_role_option(b: &Binder, name: &Ident) -> PgError {
    let mut e = PgError::new(SqlState::SYNTAX_ERROR, format!("unrecognized role option \"{}\"", name.value));
    if let Some(p) = b.pos_span(name.span) {
        e = e.with_position(p);
    }
    e
}

/// Shared GRANT / REVOKE implementation over the sqlparser fields (PG `ExecuteGrantStmt`).
fn bind_grant_input(b: &mut Binder, input: GrantInput<'_>) -> Result<BoundStatement> {
    // Syntax stage: privilege names become acl bits (unknown names are parse errors in PG).
    let items: Option<Vec<(u32, Option<&[Ident]>)>> = match input.privileges {
        Privileges::All { .. } => None,
        Privileges::Actions(actions) => {
            let mut v = Vec::with_capacity(actions.len());
            for action in actions {
                v.push(privilege_bits(action)?);
            }
            Some(v)
        }
    };
    let objects = match input.objects {
        Some(objects) => objects,
        None => return Err(diag::syntax_error_near(if input.revoke { "FROM" } else { "TO" }, None)),
    };

    // 1. GRANTED BY (`ExecuteGrantStmt`: grantor must be the current user).
    let grantor = grantor_oid(b, input.granted_by)?;
    // 2. Objects.
    let (stmt_kind, resolved) = resolve_objects(b, objects)?;
    // 3. Grantees (PUBLIC = 0).
    let mut grantees: Vec<Oid> = Vec::with_capacity(input.grantees.len());
    for grantee in input.grantees {
        grantees.push(grantee_oid(b, grantee)?);
    }
    let base = SpecBase { grantees, grant_option: input.grant_option, grantor, cascade: input.cascade };

    // 4. Privilege validity for the statement kind. Column entries are checked per object below.
    let mut plain: u32 = 0;
    let mut col_items: Vec<(u32, Vec<String>)> = Vec::new();
    if let Some(list) = &items {
        for &(bits, columns) in list {
            match columns {
                Some(cols) => {
                    if stmt_kind != ObjType::Table {
                        return Err(PgError::new(
                            INVALID_GRANT_OPERATION,
                            "column privileges are only valid for relations",
                        ));
                    }
                    let mut names = Vec::with_capacity(cols.len());
                    for col in cols {
                        names.push(b.ident(col));
                    }
                    col_items.push((bits, names));
                }
                None => {
                    if bits & !stmt_kind.all_privileges() != 0 {
                        return Err(PgError::new(
                            INVALID_GRANT_OPERATION,
                            format!(
                                "invalid privilege type {} for {}",
                                privilege_name(bits),
                                stmt_kind.word()
                            ),
                        ));
                    }
                    plain |= bits;
                }
            }
        }
    }

    // 5. Per-object checks (`ExecGrant_Relation`) and spec emission.
    let env: &CatalogEnv = b.env;
    let mut specs: Vec<GrantSpec> = Vec::new();
    for obj in &resolved {
        if items.is_none() {
            specs.push(base.spec(&obj.target, PrivilegeSet::All, Vec::new()));
            continue;
        }
        let mut whole = plain;
        let mut cols_here = col_items.clone();
        if stmt_kind == ObjType::Table && obj.is_sequence {
            if whole & !acl_mode::ALL_SEQUENCE != 0 {
                b.notice(warning(format!(
                    "sequence \"{}\" only supports USAGE, SELECT, and UPDATE privileges",
                    obj.name
                )));
                whole &= acl_mode::ALL_SEQUENCE;
            }
            for item in cols_here.iter_mut() {
                if item.0 & !acl_mode::SELECT != 0 {
                    b.notice(warning(format!(
                        "sequence \"{}\" only supports SELECT column privileges",
                        obj.name
                    )));
                    item.0 &= acl_mode::SELECT;
                }
            }
        }
        if stmt_kind == ObjType::Table && !obj.is_sequence && whole & !acl_mode::ALL_RELATION != 0 {
            // USAGE is the only sequence privilege that a table does not have.
            return Err(PgError::new(INVALID_GRANT_OPERATION, "invalid privilege type USAGE for table"));
        }
        let mut col_specs: Vec<(u32, Vec<i16>)> = Vec::with_capacity(cols_here.len());
        for (bits, names) in &cols_here {
            if *bits == 0 {
                continue;
            }
            col_specs.push((*bits, column_attnums(env, obj, names)?));
        }
        if whole != 0 {
            specs.push(base.spec(&obj.target, PrivilegeSet::Bits(whole), Vec::new()));
        }
        for (bits, attnums) in col_specs {
            specs.push(base.spec(&obj.target, PrivilegeSet::Bits(bits), attnums));
        }
    }

    Ok(BoundStatement::Grant { specs, revoke: input.revoke })
}

/// Maps one sqlparser privilege to its acl bit and optional column list. Privileges that PostgreSQL
/// does not have (`MAINTAIN` and `SET` are not representable in sqlparser 0.63, and the Snowflake
/// forms) are syntax errors.
fn privilege_bits(action: &Action) -> Result<(u32, Option<&[Ident]>)> {
    let item = match action {
        Action::Select { columns } => (acl_mode::SELECT, columns.as_deref()),
        Action::Insert { columns } => (acl_mode::INSERT, columns.as_deref()),
        Action::Update { columns } => (acl_mode::UPDATE, columns.as_deref()),
        Action::References { columns } => (acl_mode::REFERENCES, columns.as_deref()),
        Action::Delete => (acl_mode::DELETE, None),
        Action::Truncate => (acl_mode::TRUNCATE, None),
        Action::Trigger => (acl_mode::TRIGGER, None),
        Action::Usage => (acl_mode::USAGE, None),
        Action::Temporary => (acl_mode::CREATE_TEMP, None),
        Action::Connect => (acl_mode::CONNECT, None),
        Action::Create { obj_type: None } => (acl_mode::CREATE, None),
        Action::Execute { obj_type: None } => (acl_mode::EXECUTE, None),
        Action::Role { role }
            if matches!(role.0.as_slice(), [ObjectNamePart::Identifier(id)] if id.value == crate::parser::MAINTAIN_PRIVILEGE_MARKER) =>
        {
            (acl_mode::MAINTAIN, None)
        }
        other => return Err(diag::syntax_error_near(first_word(&other.to_string()), None)),
    };
    Ok(item)
}

/// PG `privilege_to_string` for one acl bit (`TEMPORARY` for `CREATE TEMP`).
fn privilege_name(bit: u32) -> &'static str {
    // Called with single bits only (each sqlparser action maps to one bit).
    match bit {
        acl_mode::INSERT => "INSERT",
        acl_mode::SELECT => "SELECT",
        acl_mode::UPDATE => "UPDATE",
        acl_mode::DELETE => "DELETE",
        acl_mode::TRUNCATE => "TRUNCATE",
        acl_mode::REFERENCES => "REFERENCES",
        acl_mode::TRIGGER => "TRIGGER",
        acl_mode::EXECUTE => "EXECUTE",
        acl_mode::USAGE => "USAGE",
        acl_mode::CREATE => "CREATE",
        acl_mode::CREATE_TEMP => "TEMPORARY",
        acl_mode::CONNECT => "CONNECT",
        acl_mode::SET => "SET",
        acl_mode::ALTER_SYSTEM => "ALTER SYSTEM",
        acl_mode::MAINTAIN => "MAINTAIN",
        _ => "",
    }
}

/// `GRANTED BY role` (default: the current user). PG `ExecuteGrantStmt`: a grantor other than the
/// current user is 0A000 `grantor must be current user`.
fn grantor_oid(b: &mut Binder, granted_by: Option<&Ident>) -> Result<Oid> {
    let current = b.env.current_user();
    let id = match granted_by {
        Some(id) => id,
        None => return Ok(current),
    };
    let grantor = role_from_ident(b, id)?;
    if grantor != current {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "grantor must be current user"));
    }
    Ok(grantor)
}

/// A grantee (`GranteesType` + name). `PUBLIC` is OID 0.
fn grantee_oid(b: &mut Binder, grantee: &Grantee) -> Result<Oid> {
    match &grantee.grantee_type {
        GranteesType::Public => return Ok(0),
        GranteesType::Role | GranteesType::Group | GranteesType::None => {}
        other => return Err(diag::syntax_error_near(grantee_keyword(other), None)),
    }
    match &grantee.name {
        Some(GranteeName::ObjectName(name)) => match name.0.as_slice() {
            [ObjectNamePart::Identifier(id)] => {
                if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("public") {
                    return Ok(0);
                }
                role_from_ident(b, id)
            }
            _ => Err(diag::syntax_error_near(".", None)),
        },
        _ => Err(diag::syntax_error_near("@", None)),
    }
}

fn grantee_keyword(kind: &GranteesType) -> &'static str {
    match kind {
        GranteesType::Role => "ROLE",
        GranteesType::Share => "SHARE",
        GranteesType::User => "USER",
        GranteesType::Group => "GROUP",
        GranteesType::Public => "PUBLIC",
        GranteesType::DatabaseRole => "DATABASE",
        GranteesType::Application => "APPLICATION",
        GranteesType::ApplicationRole => "APPLICATION",
        GranteesType::None => "",
    }
}

/// A role written as an identifier. Unquoted `CURRENT_USER`, `CURRENT_ROLE` and `SESSION_USER` are the
/// keyword roles; everything else is looked up by name (42704 `role "x" does not exist`).
fn role_from_ident(b: &mut Binder, id: &Ident) -> Result<Oid> {
    if id.quote_style.is_none() {
        match id.value.to_ascii_lowercase().as_str() {
            "current_user" | "current_role" => return Ok(b.env.current_user()),
            "session_user" => return Ok(b.env.session_user()),
            _ => {}
        }
    }
    let name = b.ident(id);
    b.env.role_oid(&name)
}

/// Resolves the `ON` clause (PG `objectNamesToOids` and `objectsInSchemaToOids`) and returns the
/// statement kind used for the privilege check.
fn resolve_objects(b: &mut Binder, objects: &GrantObjects) -> Result<(ObjType, Vec<Resolved>)> {
    let env: &CatalogEnv = b.env;
    let mut out: Vec<Resolved> = Vec::new();
    // `ROUTINE` / `ALL ROUTINES IN SCHEMA` keyword of the statement text (dispatch::GRANT_ROUTINE_KEY).
    let routine_keyword: Option<String> = b.scratch.get(GRANT_ROUTINE_KEY).cloned();
    let kind = match objects {
        GrantObjects::Tables(names) => {
            for name in names {
                out.push(resolve_relation_target(b, name, false)?);
            }
            ObjType::Table
        }
        GrantObjects::Sequences(names) => {
            for name in names {
                out.push(resolve_relation_target(b, name, true)?);
            }
            ObjType::Sequence
        }
        GrantObjects::Schemas(names) => {
            for name in names {
                let schema = single_name(b, name)?;
                let oid = env.namespace_oid(&schema)?;
                out.push(Resolved {
                    target: GrantTarget::Object(ObjectAddress::namespace(oid)),
                    name: schema,
                    is_sequence: false,
                    relation: None,
                });
            }
            ObjType::Schema
        }
        GrantObjects::Databases(names) => {
            for name in names {
                let database = single_name(b, name)?;
                let oid = match env.snapshot().database_by_name(&database) {
                    Some(db) => db.oid,
                    None => return Err(cat_errors::database_does_not_exist(&database)),
                };
                out.push(Resolved {
                    target: GrantTarget::Object(ObjectAddress::new(ObjClass::Database, oid)),
                    name: database,
                    is_sequence: false,
                    relation: None,
                });
            }
            ObjType::Database
        }
        GrantObjects::Function { name, arg_types } => {
            let want = if routine_keyword.as_deref() == Some("routine") { RoutineWant::Any } else { RoutineWant::Function };
            out.push(resolve_routine(b, name, arg_types, want)?);
            ObjType::Function
        }
        GrantObjects::Procedure { name, arg_types } => {
            out.push(resolve_routine(b, name, arg_types, RoutineWant::Procedure)?);
            ObjType::Procedure
        }
        GrantObjects::AllTablesInSchema { schemas } => {
            for schema in schemas {
                let (name, oid) = schema_target(b, env, schema)?;
                out.push(Resolved {
                    target: GrantTarget::AllTablesInSchema(oid),
                    name,
                    is_sequence: false,
                    relation: None,
                });
            }
            ObjType::Table
        }
        GrantObjects::AllSequencesInSchema { schemas } => {
            for schema in schemas {
                let (name, oid) = schema_target(b, env, schema)?;
                out.push(Resolved {
                    target: GrantTarget::AllSequencesInSchema(oid),
                    name,
                    is_sequence: true,
                    relation: None,
                });
            }
            ObjType::Sequence
        }
        GrantObjects::AllFunctionsInSchema { schemas } => {
            for schema in schemas {
                if routine_keyword.as_deref() == Some("all routines") {
                    all_routines_in_schema(b, env, schema, &mut out)?;
                    continue;
                }
                let (name, oid) = schema_target(b, env, schema)?;
                out.push(Resolved {
                    target: GrantTarget::AllFunctionsInSchema(oid),
                    name,
                    is_sequence: false,
                    relation: None,
                });
            }
            ObjType::Function
        }
        other => return Err(diag::syntax_error_near(&objects_token(other), None)),
    };
    Ok((kind, out))
}

/// A table, view, matview or sequence named in `ON [TABLE|SEQUENCE]`. Index and composite type names
/// are rejected like PG's `ExecGrant_Relation`; a non-sequence under `SEQUENCE` is 42809.
fn resolve_relation_target(b: &mut Binder, name: &ObjectName, want_sequence: bool) -> Result<Resolved> {
    let env: &CatalogEnv = b.env;
    let qn = qualify(b, name)?;
    let schema = qn.schema.as_deref();
    let class = env.lookup_class(schema, &qn.name)?;
    let resolved = match class {
        None => return Err(cat_errors::relation_does_not_exist(schema, &qn.name)),
        Some(ClassRef::Index(_)) | Some(ClassRef::SystemIndex(_)) => {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is an index", qn.name)));
        }
        Some(ClassRef::Sequence(oid)) => Resolved {
            target: GrantTarget::Object(ObjectAddress::relation(oid)),
            name: qn.name.clone(),
            is_sequence: true,
            relation: None,
        },
        Some(ClassRef::Relation(oid)) => {
            let rel = env.snapshot().relation(oid).ok_or_else(|| {
                PgError::internal(format!("catalog: relation {oid} missing from the snapshot"))
            })?;
            if matches!(rel.kind, RelKind::CompositeType) {
                return Err(PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is a composite type", qn.name),
                ));
            }
            if want_sequence {
                return Err(cat_errors::not_a(&qn.name, "sequence"));
            }
            Resolved {
                target: GrantTarget::Object(ObjectAddress::relation(oid)),
                name: qn.name.clone(),
                is_sequence: false,
                relation: Some(oid),
            }
        }
    };
    Ok(resolved)
}

/// A function or procedure with optional argument types (PG `LookupFuncWithArgs`).
/// No argument list: the name must be unique (42725). With arguments: an exact match on the IN
/// argument types (42883 when none). sqlparser cannot tell `f()` from `f`, so an empty list means
/// "no argument list".
fn resolve_routine(b: &mut Binder, name: &ObjectName, arg_types: &[DataType], want: RoutineWant) -> Result<Resolved> {
    let env: &CatalogEnv = b.env;
    // PG reports a missing ROUTINE as "function ... does not exist" (parse_func.c).
    let word = if want == RoutineWant::Procedure { "procedure" } else { "function" };
    let qn = qualify(b, name)?;
    let schema = qn.schema.as_deref();
    let candidates = env.function_candidates(schema, &qn.name)?;
    let (oid, kind, arg_oids): (Oid, ProcKind, Vec<Oid>) = if arg_types.is_empty() {
        match candidates.as_slice() {
            [] => return Err(cat_errors::no_function_named(word, &qn.display())),
            [only] => (only.oid, only.kind, only.arg_types.clone()),
            _ => return Err(cat_errors::function_name_not_unique(&qn.display())),
        }
    } else {
        let mut wanted: Vec<Oid> = Vec::with_capacity(arg_types.len());
        for dt in arg_types {
            wanted.push(types::resolve_data_type(b, dt, None)?.id.oid);
        }
        let found = candidates.iter().find(|c| c.arg_types == wanted).map(|c| (c.oid, c.kind));
        match found {
            Some((oid, kind)) => (oid, kind, wanted),
            None => {
                let sig = routine_signature(b, &qn, &wanted);
                return Err(PgError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("{word} {sig} does not exist"),
                ));
            }
        }
    };
    if want == RoutineWant::Function && kind == ProcKind::Procedure {
        let sig = routine_signature(b, &qn, &arg_oids);
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is not a function")));
    }
    if want == RoutineWant::Procedure && kind != ProcKind::Procedure {
        let sig = routine_signature(b, &qn, &arg_oids);
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{sig} is not a procedure")));
    }
    Ok(Resolved {
        target: GrantTarget::Object(ObjectAddress::proc(oid)),
        name: qn.name.clone(),
        is_sequence: false,
        relation: None,
    })
}

/// `name(type, type)` as PG `func_signature_string` prints it.
fn routine_signature(b: &Binder, qn: &QualName, arg_oids: &[Oid]) -> String {
    let names: Vec<String> = arg_oids.iter().map(|oid| b.type_name(TypeId { oid: *oid, typmod: -1 })).collect();
    format!("{}({})", qn.display(), names.join(", "))
}

/// `ALL ROUTINES IN SCHEMA s` (PG `objectsInSchemaToOids`, OBJECT_ROUTINE): one object target per
/// function, aggregate and procedure of the schema, in OID order. Builtin schemas are system catalogs
/// (42501 like `ALL FUNCTIONS`), checked here because the catalog's schema-wide targets exclude
/// procedures.
fn all_routines_in_schema(b: &mut Binder, env: &CatalogEnv, schema: &ObjectName, out: &mut Vec<Resolved>) -> Result<()> {
    let (name, oid) = schema_target(b, env, schema)?;
    let data = env.snapshot().data();
    if !data.namespaces.contains_key(&oid) {
        return Err(cat_errors::system_catalog_permission(&name));
    }
    for f in data.functions.values().filter(|f| f.namespace == oid) {
        out.push(Resolved {
            target: GrantTarget::Object(ObjectAddress::proc(f.oid)),
            name: f.name.clone(),
            is_sequence: false,
            relation: None,
        });
    }
    Ok(())
}

/// Schema for an `ALL ... IN SCHEMA` target: (name, OID), 3F000 when missing.
fn schema_target(b: &mut Binder, env: &CatalogEnv, name: &ObjectName) -> Result<(String, Oid)> {
    let schema = single_name(b, name)?;
    let oid = env.namespace_oid(&schema)?;
    Ok((schema, oid))
}

/// Column attnums for `GRANT priv (cols) ON obj` (PG `expand_col_privileges`). Sequences have no
/// columns, so their column lists fail with 42703 like PG.
fn column_attnums(env: &CatalogEnv, obj: &Resolved, names: &[String]) -> Result<Vec<i16>> {
    if matches!(obj.target, GrantTarget::AllTablesInSchema(_)) {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "column privileges in GRANT ... ON ALL TABLES IN SCHEMA are not supported",
        ));
    }
    let rel = match obj.relation {
        Some(oid) => env.snapshot().relation(oid),
        None => None,
    };
    let mut out: Vec<i16> = Vec::with_capacity(names.len());
    for name in names {
        let attnum = rel.and_then(|r| r.column_by_name(name)).map(|c| c.attnum);
        match attnum {
            Some(attnum) => out.push(attnum),
            None => return Err(cat_errors::column_of_relation_does_not_exist(name, &obj.name)),
        }
    }
    Ok(out)
}

/// A possibly qualified name, rejecting another database (0A000 `cross-database references`).
fn qualify(b: &mut Binder, name: &ObjectName) -> Result<QualName> {
    let qn = b.qual(name)?;
    if let Some(catalog) = &qn.catalog {
        if *catalog != b.ctx.session.database {
            return Err(diag::cross_database(&format!("{catalog}.{}", qn.display()), qn.pos));
        }
    }
    Ok(qn)
}

/// A single unqualified identifier (databases, schemas); anything dotted is a syntax error.
fn single_name(b: &mut Binder, name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => Ok(b.ident(id)),
        _ => Err(diag::syntax_error_near(".", None)),
    }
}

/// WARNING with the GRANT SQLSTATE (PG `ereport(WARNING, ERRCODE_INVALID_GRANT_OPERATION)`).
fn warning(message: String) -> PgError {
    PgError::new(INVALID_GRANT_OPERATION, message).with_severity(Severity::Warning)
}

/// The first whitespace-separated word of a keyword rendering (syntax error token).
fn first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or(text)
}

/// The token PostgreSQL reports for an `ON` object kind it does not accept.
fn objects_token(objects: &GrantObjects) -> String {
    let text = objects.to_string();
    let mut words = text.split_whitespace();
    match objects {
        GrantObjects::AllViewsInSchema { .. }
        | GrantObjects::AllMaterializedViewsInSchema { .. }
        | GrantObjects::AllExternalTablesInSchema { .. } => {
            words.next();
            words.next().unwrap_or("").to_string()
        }
        _ => words.next().unwrap_or("").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// Parses and binds one GRANT/REVOKE against `env` (the builtin catalog; no DDL needed).
    fn bind_sql(env: &CatalogEnv, sql: &str) -> Result<BoundStatement> {
        let parsed = Parser::parse_sql(&PostgreSqlDialect {}, sql)
            .map_err(|e| PgError::syntax(e.to_string(), None))?;
        let stmt = parsed.into_iter().next().ok_or_else(|| PgError::internal("no statement parsed"))?;
        let ctx = BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        bind_grant(&mut b, &stmt)
    }

    fn specs_of(stmt: BoundStatement) -> Vec<GrantSpec> {
        match stmt {
            BoundStatement::Grant { specs, .. } => specs,
            _ => Vec::new(),
        }
    }

    fn error_of(env: &CatalogEnv, sql: &str) -> PgError {
        match bind_sql(env, sql) {
            Ok(_) => PgError::internal("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn grant_select_insert_on_two_objects_to_role_and_public() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let pg = env.role_oid("postgres")?;
        let stmt = bind_sql(&env, "grant select, insert on pg_class, pg_namespace to postgres, public")?;
        let specs = specs_of(stmt);
        assert_eq!(specs.len(), 2);
        for spec in &specs {
            assert_eq!(spec.grantees, vec![pg, 0]);
            assert_eq!(spec.privileges, PrivilegeSet::Bits(acl_mode::SELECT | acl_mode::INSERT));
            assert!(!spec.grant_option);
            assert_eq!(spec.grantor, pg);
            assert!(spec.columns.is_empty());
        }
        Ok(())
    }

    #[test]
    fn grant_all_on_schema_with_grant_option() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ns = env.namespace_oid("public")?;
        let specs = specs_of(bind_sql(&env, "grant all on schema public to postgres with grant option")?);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].privileges, PrivilegeSet::All);
        assert!(specs[0].grant_option);
        assert_eq!(specs[0].target, GrantTarget::Object(ObjectAddress::namespace(ns)));
        Ok(())
    }

    #[test]
    fn grant_select_column_list_resolves_attnums() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let rel_oid = match env.lookup_class(None, "pg_class")? {
            Some(ClassRef::Relation(oid)) => oid,
            _ => return Err(PgError::internal("pg_class is not a relation")),
        };
        let rel = env.snapshot().relation(rel_oid).ok_or_else(|| PgError::internal("pg_class missing"))?;
        let relname = rel.column_by_name("relname").map(|c| c.attnum);
        let relnamespace = rel.column_by_name("relnamespace").map(|c| c.attnum);
        let (Some(relname), Some(relnamespace)) = (relname, relnamespace) else {
            return Err(PgError::internal("pg_class columns missing"));
        };
        let specs = specs_of(bind_sql(&env, "grant select (relname, relnamespace) on pg_class to postgres")?);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].columns, vec![relname, relnamespace]);
        assert_eq!(specs[0].privileges, PrivilegeSet::Bits(acl_mode::SELECT));
        Ok(())
    }

    #[test]
    fn truncate_on_database_is_invalid_for_database() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant truncate on database postgres to postgres");
        assert_eq!(e.sqlstate.code(), "0LP01");
        assert_eq!(e.message, "invalid privilege type TRUNCATE for database");
        Ok(())
    }

    #[test]
    fn truncate_on_schema_is_invalid_for_schema() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant truncate on schema public to postgres");
        assert_eq!(e.sqlstate.code(), "0LP01");
        assert_eq!(e.message, "invalid privilege type TRUNCATE for schema");
        Ok(())
    }

    #[test]
    fn usage_on_table_is_invalid_for_table() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant usage on pg_class to postgres");
        assert_eq!(e.sqlstate.code(), "0LP01");
        assert_eq!(e.message, "invalid privilege type USAGE for table");
        Ok(())
    }

    #[test]
    fn revoke_grant_option_for_select_cascade() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let stmt = bind_sql(&env, "revoke grant option for select on pg_class from postgres cascade")?;
        match &stmt {
            BoundStatement::Grant { revoke, .. } => assert!(*revoke),
            _ => return Err(PgError::internal("expected a REVOKE statement")),
        }
        let specs = specs_of(stmt);
        assert_eq!(specs.len(), 1);
        assert!(specs[0].grant_option);
        assert!(specs[0].cascade);
        assert_eq!(specs[0].privileges, PrivilegeSet::Bits(acl_mode::SELECT));
        Ok(())
    }

    #[test]
    fn unknown_grantee_is_42704() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant select on pg_class to no_such_role_for_test");
        assert_eq!(e.sqlstate.code(), "42704");
        assert_eq!(e.message, "role \"no_such_role_for_test\" does not exist");
        Ok(())
    }

    #[test]
    fn unknown_relation_is_42p01() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant select on no_such_table_for_test to postgres");
        assert_eq!(e.sqlstate.code(), "42P01");
        Ok(())
    }

    #[test]
    fn current_user_keyword_grantee_is_the_current_user() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let specs = specs_of(bind_sql(&env, "grant select on pg_class to current_user")?);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].grantees, vec![env.current_user()]);
        Ok(())
    }

    #[test]
    fn granted_by_unknown_role_is_42704() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = error_of(&env, "grant select on pg_class to postgres granted by no_such_grantor_for_test");
        assert_eq!(e.sqlstate.code(), "42704");
        Ok(())
    }
}
