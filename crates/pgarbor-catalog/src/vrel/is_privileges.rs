//! information_schema, part 5 (privileges): `table_privileges`, `column_privileges`,
//! `role_table_grants`, `role_column_grants`, `usage_privileges`, `role_usage_grants`,
//! `routine_privileges`, `role_routine_grants`, `udt_privileges`, `role_udt_grants`,
//! `data_type_privileges`. Lease CAT-44. Authoritative semantics: the CREATE VIEW of each name in PG
//! 18.6 `information_schema.sql` (they all expand `aclexplode(relacl)` / `acldefault` per grantee);
//! conventions: `is_core.rs`.
//!
//! Core operation: for an object with owner `o` and ACL `acl` (`None` = `acl::acldefault`), each
//! `AclItem` is exploded into one row per privilege bit: `grantor` (role name), `grantee` (role name,
//! or the literal `PUBLIC` for grantee 0), privilege_type (`SELECT`, `INSERT`, `UPDATE`, `DELETE`,
//! `TRUNCATE`, `REFERENCES`, `TRIGGER` for tables (oracle: MAINTAIN is NOT listed; the default-ACL table yields exactly 7 rows INSERT, SELECT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER in that order); `USAGE` for sequences/types/schemas
//! ... per view), `is_grantable` (`YES` if the bit is in `grant_options`), and for table_privileges
//! `with_hierarchy` (`YES` only for SELECT, `NO` otherwise - oracle). Rows are visible to a user who is grantor or grantee (or inherits them:
//! `pg_has_role(grantor, 'USAGE') OR pg_has_role(grantee, 'USAGE') OR grantee = PUBLIC`).
//! `role_*_grants` restrict to grants where the grantor or grantee is an ENABLED role of the current
//! user. `column_privileges` / `role_column_grants` combine table-level ACL bits for SELECT/INSERT/
//! UPDATE/REFERENCES with `Column::acl` entries. `usage_privileges` lists USAGE on sequences, domains
//! (types), schemas? (PG: collations, domains, sequences, user-defined types; FDWs/servers
//! unsupported). `routine_privileges` lists EXECUTE (PUBLIC has it by default on every function).
//! `data_type_privileges` has one row per column/domain/parameter-type usage the user can see with
//! `object_type` (`TABLE`, `DOMAIN`, `ROUTINE`, `USER-DEFINED TYPE`), `dtd_identifier`.
//! Default ACLs matter: a table with `relacl = None` still yields owner rows because the views use
//! `aclexplode(coalesce(relacl, acldefault('r', relowner)))`; the owner's implicit rows are
//! `grantor = owner`, `grantee = owner`, `is_grantable = YES` (the views OR in the ownership right
//! to grant: port the CASE in `table_privileges`).
//!
//! ## Implementation notes (CAT-44)
//! - One [`PrivEnv`] per scan: the current user's [`RoleGraph`] (built once), the current database
//!   name, and its encoding. `pg_has_role(r, 'USAGE')` = `graph.has_privs_of_role(current_user, r)`;
//!   `pg_has_role(m, r, 'USAGE')` = `graph.has_privs_of_role(m, r)` (never called with PUBLIC = 0).
//! - Relations are enumerated as user relations plus `builtin::builtin_relations()` (owner 10,
//!   `acl` None), sorted by OID. Routines are the builtin `PROCS` (owner 10, `proacl` NULL) plus user
//!   functions. Types are the builtin `TYPES` (typacl NULL) plus user domains/enums and the rowtypes of
//!   user relations.
//! - Every view is a list of exploded grants passed through the same resolution: an inner join to the
//!   grantor and grantee role names (a missing role drops the row, as in PG), the visibility test,
//!   and `is_grantable = grant option OR grantee has the owner's rights`. `role_*` views filter
//!   that list by enabled roles before the rows are built.
//! - `nameconcatoid` (`specific_name`) is `name_<oid>` with the name truncated, never the suffix.
//! - `usage_privileges` for sequences follows the view text literally: its ACL default is
//!   `acldefault('r', owner)`, which has no USAGE bit, so a sequence with a NULL ACL shows no rows.
//!   Foreign data wrappers and servers are not supported by PGArbor and produce no rows.
//! - `data_type_privileges` is defined on the `attributes`, `columns`, `domains`, `parameters` and
//!   `routines` views, so it reuses those producers' rows (and their visibility) and keeps their
//!   `dtd_identifier`. It therefore needs CAT-40, CAT-41 and CAT-43 to be implemented.
//! - Ignoring `ScanRequest` is always correct (the executor re-applies WHERE); no fast paths are used.
use super::is_columns::{IsColumns, IsDomains};
use super::is_core::IsAttributes;
use super::is_routines::{IsParameters, IsRoutines};
use super::rowbuild::RowBuilder;
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin::{self, relid};
use crate::env::CatalogEnv;
use crate::model::{acl_mode, Acl, RelKind, Relation};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::catalog_data::{PROCS, TYPES};
use pgarbor_types::Datum;
use std::collections::HashSet;

/// Grantee OID of the PUBLIC pseudo-role (`aclitem` grantee 0).
const PUBLIC: Oid = 0;
/// `NAMEDATALEN - 1`: the longest identifier in bytes.
const MAX_NAME_BYTES: usize = 63;

/// Table privileges in `aclexplode` order (ascending acl bit): the view's `prtype` list without MAINTAIN.
const TABLE_PRIVS: &[(u32, &str)] = &[
    (acl_mode::INSERT, "INSERT"),
    (acl_mode::SELECT, "SELECT"),
    (acl_mode::UPDATE, "UPDATE"),
    (acl_mode::DELETE, "DELETE"),
    (acl_mode::TRUNCATE, "TRUNCATE"),
    (acl_mode::REFERENCES, "REFERENCES"),
    (acl_mode::TRIGGER, "TRIGGER"),
];
/// Column privileges (`column_privileges.prtype IN (INSERT, SELECT, UPDATE, REFERENCES)`).
const COLUMN_PRIVS: &[(u32, &str)] = &[
    (acl_mode::INSERT, "INSERT"),
    (acl_mode::SELECT, "SELECT"),
    (acl_mode::UPDATE, "UPDATE"),
    (acl_mode::REFERENCES, "REFERENCES"),
];
const ROUTINE_PRIVS: &[(u32, &str)] = &[(acl_mode::EXECUTE, "EXECUTE")];
const USAGE_PRIVS: &[(u32, &str)] = &[(acl_mode::USAGE, "USAGE")];
/// `udt_privileges` reports USAGE on types as `TYPE USAGE` (sic, as in PG).
const UDT_PRIVS: &[(u32, &str)] = &[(acl_mode::USAGE, "TYPE USAGE")];

// ------------------------------------------------------------------ producers

/// Producer for `information_schema.table_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsTablePrivileges;

impl VirtualRelation for IsTablePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_TABLE_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, false, table_privileges_rows)
    }
}

/// Producer for `information_schema.column_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsColumnPrivileges;

impl VirtualRelation for IsColumnPrivileges {
    fn oid(&self) -> Oid {
        relid::IS_COLUMN_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, false, column_privileges_rows)
    }
}

/// Producer for `information_schema.role_table_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleTableGrants;

impl VirtualRelation for IsRoleTableGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_TABLE_GRANTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, true, table_privileges_rows)
    }
}

/// Producer for `information_schema.role_column_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleColumnGrants;

impl VirtualRelation for IsRoleColumnGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_COLUMN_GRANTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, true, column_privileges_rows)
    }
}

/// Producer for `information_schema.usage_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsUsagePrivileges;

impl VirtualRelation for IsUsagePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_USAGE_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, false, usage_privileges_rows)
    }
}

/// Producer for `information_schema.role_usage_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleUsageGrants;

impl VirtualRelation for IsRoleUsageGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_USAGE_GRANTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, true, usage_privileges_rows)
    }
}

/// Producer for `information_schema.routine_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoutinePrivileges;

impl VirtualRelation for IsRoutinePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_ROUTINE_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, false, routine_privileges_rows)
    }
}

/// Producer for `information_schema.role_routine_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleRoutineGrants;

impl VirtualRelation for IsRoleRoutineGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_ROUTINE_GRANTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, true, routine_privileges_rows)
    }
}

/// Producer for `information_schema.udt_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsUdtPrivileges;

impl VirtualRelation for IsUdtPrivileges {
    fn oid(&self) -> Oid {
        relid::IS_UDT_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, false, udt_privileges_rows)
    }
}

/// Producer for `information_schema.role_udt_grants`.
#[derive(Clone, Copy, Debug)]
pub struct IsRoleUdtGrants;

impl VirtualRelation for IsRoleUdtGrants {
    fn oid(&self) -> Oid {
        relid::IS_ROLE_UDT_GRANTS
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        scan_view(ctx, true, udt_privileges_rows)
    }
}

/// Producer for `information_schema.data_type_privileges`.
#[derive(Clone, Copy, Debug)]
pub struct IsDataTypePrivileges;

impl VirtualRelation for IsDataTypePrivileges {
    fn oid(&self) -> Oid {
        relid::IS_DATA_TYPE_PRIVILEGES
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let _ = req;
        let env = PrivEnv::new(ctx)?;
        data_type_privileges_rows(ctx, &env)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[
    &IsTablePrivileges,
    &IsColumnPrivileges,
    &IsRoleTableGrants,
    &IsRoleColumnGrants,
    &IsUsagePrivileges,
    &IsRoleUsageGrants,
    &IsRoutinePrivileges,
    &IsRoleRoutineGrants,
    &IsUdtPrivileges,
    &IsRoleUdtGrants,
    &IsDataTypePrivileges,
];

// ------------------------------------------------------------------ per-scan state

/// Per-scan context shared by the privilege views.
struct PrivEnv<'a> {
    snap: &'a CatalogSnapshot,
    /// Role memberships for the privilege tests (built once per scan).
    graph: RoleGraph,
    current_user: Oid,
    database_name: String,
    database_encoding: i32,
}

/// One exploded `aclitem` privilege: a grantor, a grantee (0 = PUBLIC), the privilege name and its
/// grant option.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Grant {
    grantor: Oid,
    grantee: Oid,
    privilege: &'static str,
    grantable: bool,
}

/// A grant after the view's joins and visibility rules, with the role names resolved.
struct Resolved {
    grantor_name: String,
    grantee_name: String,
    privilege: &'static str,
    is_grantable: bool,
}

/// A schema-qualified object whose ACL a usage or udt view explodes.
struct Obj<'b> {
    schema: &'b str,
    name: &'b str,
    owner: Oid,
    acl: Option<&'b Acl>,
    kind: AclObject,
}

impl<'a> PrivEnv<'a> {
    fn new(ctx: &ScanContext<'a>) -> Result<PrivEnv<'a>> {
        let env: &'a CatalogEnv = ctx.env;
        let snap: &'a CatalogSnapshot = env.snapshot();
        let database = env.database();
        let db = snap.database(database).ok_or_else(|| {
            PgError::internal(format!("current database {database} is not in the catalog"))
        })?;
        Ok(PrivEnv {
            snap,
            graph: RoleGraph::build(snap, database)?,
            current_user: env.current_user(),
            database_name: db.name.clone(),
            database_encoding: db.encoding,
        })
    }

    /// Schema name of a namespace OID (`None` = the join to pg_namespace drops the row).
    fn namespace_name(&self, oid: Oid) -> Option<&'a str> {
        let snap: &'a CatalogSnapshot = self.snap;
        snap.namespace(oid).map(|n| n.name.as_str())
    }

    /// `pg_has_role(role, 'USAGE')` for the current user.
    fn current_has_usage(&self, role: Oid) -> Result<bool> {
        self.graph.has_privs_of_role(self.current_user, role)
    }

    /// `pg_has_role(member, role, 'USAGE')`; PUBLIC is never a member.
    fn member_has_usage(&self, member: Oid, role: Oid) -> Result<bool> {
        if member == PUBLIC {
            return Ok(false);
        }
        self.graph.has_privs_of_role(member, role)
    }

    /// `enabled_roles`: every role `r` with `pg_has_role(current_user, r, 'USAGE')`.
    fn enabled_roles(&self) -> Result<HashSet<Oid>> {
        let mut set = HashSet::new();
        for oid in self.snap.indexes().roles.keys() {
            if self.current_has_usage(*oid)? {
                set.insert(*oid);
            }
        }
        Ok(set)
    }

    /// The join and visibility rules shared by every privilege view. `owner` is the object's owner
    /// (for the `is_grantable` ownership test). `Ok(None)` means the row does not appear.
    fn resolve(&self, owner: Oid, g: &Grant) -> Result<Option<Resolved>> {
        let snap = self.snap;
        let Some(grantor_name) = snap.role_name(g.grantor) else {
            return Ok(None);
        };
        let grantee_name = if g.grantee == PUBLIC {
            "PUBLIC"
        } else {
            match snap.role_name(g.grantee) {
                Some(name) => name,
                None => return Ok(None),
            }
        };
        // pg_has_role(grantor, 'USAGE') OR pg_has_role(grantee, 'USAGE') OR grantee = 'PUBLIC'
        let visible = g.grantee == PUBLIC
            || self.current_has_usage(g.grantor)?
            || self.current_has_usage(g.grantee)?;
        if !visible {
            return Ok(None);
        }
        // object owner always has grant options
        let is_grantable = g.grantable || self.member_has_usage(g.grantee, owner)?;
        Ok(Some(Resolved {
            grantor_name: grantor_name.to_string(),
            grantee_name: grantee_name.to_string(),
            privilege: g.privilege,
            is_grantable,
        }))
    }
}

// ------------------------------------------------------------------ helpers

fn txt(s: &str) -> Datum {
    Datum::Text(s.to_string())
}

fn yes_no(b: bool) -> Datum {
    txt(if b { "YES" } else { "NO" })
}

/// Builds one row of `rel_oid` from (column name, value) pairs; unset columns are NULL.
fn build_row(rel_oid: Oid, cells: Vec<(&str, Datum)>) -> Result<Row> {
    let schema = builtin::system_rel(rel_oid)
        .ok_or_else(|| PgError::internal(format!("no system relation with OID {rel_oid}")))?;
    let mut b = RowBuilder::new(schema);
    for (name, value) in cells {
        b.set(name, value);
    }
    b.finish()
}

/// Column index of `name` in the system relation `rel_oid`.
fn col_index(rel_oid: Oid, name: &str) -> Result<usize> {
    let rel = builtin::system_rel(rel_oid)
        .ok_or_else(|| PgError::internal(format!("no system relation with OID {rel_oid}")))?;
    rel.columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| PgError::internal(format!("{} has no column \"{name}\"", rel.name)))
}

/// A text cell of a sibling producer's row (NULL stays NULL).
fn cell(row: &Row, idx: usize) -> Result<Datum> {
    match row.get(idx) {
        Some(Datum::Text(s)) => Ok(txt(s)),
        Some(Datum::Null) => Ok(Datum::Null),
        Some(other) => Err(PgError::internal(format!("information_schema column {idx} is not text: {other:?}"))),
        None => Err(PgError::internal(format!("information_schema row has no column {idx}"))),
    }
}

/// PG `nameconcatoid(name, oid)`: `name_<oid>`; oversize input is truncated in the name part only.
fn name_concat_oid(name: &str, oid: Oid) -> String {
    let suffix = format!("_{oid}");
    let max_name = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut end = name.len().min(max_name);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &name[..end], suffix)
}

/// `aclexplode`: one grant per (item, privilege bit present), items in ACL order and bits in the
/// order of `privs` (ascending acl bit).
fn explode(acl: &Acl, privs: &[(u32, &'static str)]) -> Vec<Grant> {
    let mut out = Vec::new();
    for item in acl {
        for &(bit, name) in privs {
            if item.privileges & bit != 0 {
                out.push(Grant {
                    grantor: item.grantor,
                    grantee: item.grantee,
                    privilege: name,
                    grantable: item.grant_options & bit != 0,
                });
            }
        }
    }
    out
}

/// Role-view filter: the grantor or the grantee is an enabled role (`None` = no filter).
fn keep(enabled: Option<&HashSet<Oid>>, grantor: Oid, grantee: Oid) -> bool {
    match enabled {
        None => true,
        Some(set) => set.contains(&grantor) || (grantee != PUBLIC && set.contains(&grantee)),
    }
}

/// Relations whose ACL the table and column views read: relkind r, v, f, p.
fn is_table_like(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::View | RelKind::ForeignTable | RelKind::PartitionedTable)
}

/// Every pg_class relation the views read: user relations plus builtin system tables/views, by OID.
fn all_relations(snap: &CatalogSnapshot) -> Vec<&Relation> {
    let mut rels: Vec<&Relation> = Vec::new();
    rels.extend(snap.user_relations());
    rels.extend(builtin::builtin_relations().iter());
    rels.sort_by_key(|r| r.oid);
    rels
}

/// Runs one privilege view: all grants, or only those touching an enabled role (`role_*` views).
fn scan_view(
    ctx: &ScanContext<'_>,
    enabled_only: bool,
    rows_of: fn(&PrivEnv<'_>, Option<&HashSet<Oid>>) -> Result<Vec<Row>>,
) -> Result<Vec<Row>> {
    let env = PrivEnv::new(ctx)?;
    if enabled_only {
        let enabled = env.enabled_roles()?;
        rows_of(&env, Some(&enabled))
    } else {
        rows_of(&env, None)
    }
}

// ------------------------------------------------------------------ table_privileges

fn table_privileges_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for rel in all_relations(env.snap) {
        if !is_table_like(rel.kind) {
            continue;
        }
        let Some(schema) = env.namespace_name(rel.namespace) else {
            continue;
        };
        let acl = acl::effective_acl(AclObject::Table, rel.owner, rel.acl.as_ref())?;
        for g in explode(&acl, TABLE_PRIVS) {
            if !keep(enabled, g.grantor, g.grantee) {
                continue;
            }
            let Some(r) = env.resolve(rel.owner, &g)? else {
                continue;
            };
            rows.push(build_row(
                relid::IS_TABLE_PRIVILEGES,
                vec![
                    ("grantor", txt(&r.grantor_name)),
                    ("grantee", txt(&r.grantee_name)),
                    ("table_catalog", txt(&env.database_name)),
                    ("table_schema", txt(schema)),
                    ("table_name", txt(rel.name.as_str())),
                    ("privilege_type", txt(r.privilege)),
                    ("is_grantable", yes_no(r.is_grantable)),
                    ("with_hierarchy", yes_no(r.privilege == "SELECT")),
                ],
            )?);
        }
    }
    Ok(rows)
}

// ------------------------------------------------------------------ column_privileges

fn column_privileges_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for rel in all_relations(env.snap) {
        if !is_table_like(rel.kind) {
            continue;
        }
        let Some(schema) = env.namespace_name(rel.namespace) else {
            continue;
        };
        // table-level grants apply to every column (acldefault('r') when relacl is NULL)
        let table_acl = acl::effective_acl(AclObject::Table, rel.owner, rel.acl.as_ref())?;
        let table_grants = explode(&table_acl, COLUMN_PRIVS);
        for col in rel.visible_columns() {
            // column ACLs start empty (acldefault('c') = {})
            let column_grants = match &col.acl {
                Some(a) => explode(a, COLUMN_PRIVS),
                None => Vec::new(),
            };
            // the view combines the two sources with UNION: an identical tuple appears once
            let mut seen: Vec<Grant> = Vec::new();
            for g in table_grants.iter().chain(column_grants.iter()) {
                if seen.contains(g) {
                    continue;
                }
                seen.push(*g);
                if !keep(enabled, g.grantor, g.grantee) {
                    continue;
                }
                let Some(r) = env.resolve(rel.owner, g)? else {
                    continue;
                };
                rows.push(build_row(
                    relid::IS_COLUMN_PRIVILEGES,
                    vec![
                        ("grantor", txt(&r.grantor_name)),
                        ("grantee", txt(&r.grantee_name)),
                        ("table_catalog", txt(&env.database_name)),
                        ("table_schema", txt(schema)),
                        ("table_name", txt(rel.name.as_str())),
                        ("column_name", txt(col.name.as_str())),
                        ("privilege_type", txt(r.privilege)),
                        ("is_grantable", yes_no(r.is_grantable)),
                    ],
                )?);
            }
        }
    }
    Ok(rows)
}

// ------------------------------------------------------------------ usage_privileges

fn usage_privileges_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    // collations: implicit USAGE for PUBLIC, granted by the owner; the view has no visibility filter
    for c in builtin::COLLATIONS.iter() {
        if !(c.encoding == -1 || c.encoding == env.database_encoding) {
            continue;
        }
        let Some(grantor) = env.snap.role_name(c.owner) else {
            continue;
        };
        let Some(schema) = env.namespace_name(c.namespace) else {
            continue;
        };
        if !keep(enabled, c.owner, PUBLIC) {
            continue;
        }
        rows.push(build_row(
            relid::IS_USAGE_PRIVILEGES,
            vec![
                ("grantor", txt(grantor)),
                ("grantee", txt("PUBLIC")),
                ("object_catalog", txt(&env.database_name)),
                ("object_schema", txt(schema)),
                ("object_name", txt(c.name)),
                ("object_type", txt("COLLATION")),
                ("privilege_type", txt("USAGE")),
                ("is_grantable", txt("NO")),
            ],
        )?);
    }

    // domains: builtin (typacl NULL) first, then user domains
    for t in TYPES.iter().filter(|t| t.typtype == b'd') {
        let Some(schema) = env.namespace_name(t.namespace) else {
            continue;
        };
        let obj = Obj {
            schema,
            name: t.name,
            owner: builtin::BOOTSTRAP_SUPERUSER,
            acl: None,
            kind: AclObject::Type,
        };
        usage_rows(env, enabled, &obj, "DOMAIN", &mut rows)?;
    }
    for t in env.snap.data().types.values().filter(|t| t.is_domain()) {
        let Some(schema) = env.namespace_name(t.namespace) else {
            continue;
        };
        let obj = Obj {
            schema,
            name: t.name.as_str(),
            owner: t.owner,
            acl: t.acl.as_ref(),
            kind: AclObject::Type,
        };
        usage_rows(env, enabled, &obj, "DOMAIN", &mut rows)?;
    }

    // sequences: the view's default is acldefault('r', relowner), which has no USAGE bit
    for s in env.snap.data().sequences.values() {
        let Some(schema) = env.namespace_name(s.namespace) else {
            continue;
        };
        let obj = Obj {
            schema,
            name: s.name.as_str(),
            owner: s.owner,
            acl: s.acl.as_ref(),
            kind: AclObject::Table,
        };
        usage_rows(env, enabled, &obj, "SEQUENCE", &mut rows)?;
    }
    Ok(rows)
}

/// USAGE rows of one domain or sequence.
fn usage_rows(
    env: &PrivEnv<'_>,
    enabled: Option<&HashSet<Oid>>,
    obj: &Obj<'_>,
    object_type: &str,
    out: &mut Vec<Row>,
) -> Result<()> {
    let acl = acl::effective_acl(obj.kind, obj.owner, obj.acl)?;
    for g in explode(&acl, USAGE_PRIVS) {
        if !keep(enabled, g.grantor, g.grantee) {
            continue;
        }
        let Some(r) = env.resolve(obj.owner, &g)? else {
            continue;
        };
        out.push(build_row(
            relid::IS_USAGE_PRIVILEGES,
            vec![
                ("grantor", txt(&r.grantor_name)),
                ("grantee", txt(&r.grantee_name)),
                ("object_catalog", txt(&env.database_name)),
                ("object_schema", txt(obj.schema)),
                ("object_name", txt(obj.name)),
                ("object_type", txt(object_type)),
                ("privilege_type", txt(r.privilege)),
                ("is_grantable", yes_no(r.is_grantable)),
            ],
        )?);
    }
    Ok(())
}

// ------------------------------------------------------------------ routine_privileges

fn routine_privileges_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    // builtin procs: proacl NULL, so every row uses acldefault('f', bootstrap superuser)
    let builtin_acl = acl::acldefault(AclObject::Function, builtin::BOOTSTRAP_SUPERUSER)?;
    let builtin_grants = explode(&builtin_acl, ROUTINE_PRIVS);
    for p in PROCS.iter() {
        let Some(schema) = env.namespace_name(p.namespace) else {
            continue;
        };
        for g in &builtin_grants {
            if !keep(enabled, g.grantor, g.grantee) {
                continue;
            }
            let Some(r) = env.resolve(builtin::BOOTSTRAP_SUPERUSER, g)? else {
                continue;
            };
            rows.push(routine_row(env, &r, schema, p.name, p.oid)?);
        }
    }

    // user functions
    for f in env.snap.data().functions.values() {
        let Some(schema) = env.namespace_name(f.namespace) else {
            continue;
        };
        let acl = acl::effective_acl(AclObject::Function, f.owner, f.acl.as_ref())?;
        for g in explode(&acl, ROUTINE_PRIVS) {
            if !keep(enabled, g.grantor, g.grantee) {
                continue;
            }
            let Some(r) = env.resolve(f.owner, &g)? else {
                continue;
            };
            rows.push(routine_row(env, &r, schema, f.name.as_str(), f.oid)?);
        }
    }
    Ok(rows)
}

fn routine_row(env: &PrivEnv<'_>, r: &Resolved, schema: &str, name: &str, oid: Oid) -> Result<Row> {
    let specific = name_concat_oid(name, oid);
    build_row(
        relid::IS_ROUTINE_PRIVILEGES,
        vec![
            ("grantor", txt(&r.grantor_name)),
            ("grantee", txt(&r.grantee_name)),
            ("specific_catalog", txt(&env.database_name)),
            ("specific_schema", txt(schema)),
            ("specific_name", txt(&specific)),
            ("routine_catalog", txt(&env.database_name)),
            ("routine_schema", txt(schema)),
            ("routine_name", txt(name)),
            ("privilege_type", txt(r.privilege)),
            ("is_grantable", yes_no(r.is_grantable)),
        ],
    )
}

// ------------------------------------------------------------------ udt_privileges

fn udt_privileges_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    // builtin composite types (catalog rowtypes, typacl NULL)
    for t in TYPES.iter().filter(|t| t.typtype == b'c') {
        let Some(schema) = env.namespace_name(t.namespace) else {
            continue;
        };
        let obj = Obj {
            schema,
            name: t.name,
            owner: builtin::BOOTSTRAP_SUPERUSER,
            acl: None,
            kind: AclObject::Type,
        };
        udt_rows(env, enabled, &obj, &mut rows)?;
    }

    // rowtypes of user relations: typtype 'c', no ACL of their own (acldefault('T'))
    for rel in env.snap.user_relations() {
        if !rel.kind.has_rowtype() || rel.rowtype == 0 {
            continue;
        }
        let Some(schema) = env.namespace_name(rel.namespace) else {
            continue;
        };
        let obj = Obj {
            schema,
            name: rel.name.as_str(),
            owner: rel.owner,
            acl: None,
            kind: AclObject::Type,
        };
        udt_rows(env, enabled, &obj, &mut rows)?;
    }
    Ok(rows)
}

fn udt_rows(env: &PrivEnv<'_>, enabled: Option<&HashSet<Oid>>, obj: &Obj<'_>, out: &mut Vec<Row>) -> Result<()> {
    let acl = acl::effective_acl(obj.kind, obj.owner, obj.acl)?;
    for g in explode(&acl, UDT_PRIVS) {
        if !keep(enabled, g.grantor, g.grantee) {
            continue;
        }
        let Some(r) = env.resolve(obj.owner, &g)? else {
            continue;
        };
        out.push(build_row(
            relid::IS_UDT_PRIVILEGES,
            vec![
                ("grantor", txt(&r.grantor_name)),
                ("grantee", txt(&r.grantee_name)),
                ("udt_catalog", txt(&env.database_name)),
                ("udt_schema", txt(obj.schema)),
                ("udt_name", txt(obj.name)),
                ("privilege_type", txt(r.privilege)),
                ("is_grantable", yes_no(r.is_grantable)),
            ],
        )?);
    }
    Ok(())
}

// ------------------------------------------------------------------ data_type_privileges

/// `data_type_privileges` = the union of the `attributes`, `columns`, `domains`, `parameters` and
/// `routines` views (UNION ALL, in that order), keeping each row's `dtd_identifier`. Visibility is
/// the one those views apply, so the sibling producers are scanned and their rows projected.
fn data_type_privileges_rows(ctx: &ScanContext<'_>, env: &PrivEnv<'_>) -> Result<Vec<Row>> {
    let all = ScanRequest::all();
    let sources: [(Vec<Row>, Oid, &str, &str, &str); 5] = [
        (IsAttributes.scan(ctx, &all)?, relid::IS_ATTRIBUTES, "udt_schema", "udt_name", "USER-DEFINED TYPE"),
        (IsColumns.scan(ctx, &all)?, relid::IS_COLUMNS, "table_schema", "table_name", "TABLE"),
        (IsDomains.scan(ctx, &all)?, relid::IS_DOMAINS, "domain_schema", "domain_name", "DOMAIN"),
        (IsParameters.scan(ctx, &all)?, relid::IS_PARAMETERS, "specific_schema", "specific_name", "ROUTINE"),
        (IsRoutines.scan(ctx, &all)?, relid::IS_ROUTINES, "specific_schema", "specific_name", "ROUTINE"),
    ];
    let mut rows = Vec::new();
    for (source, source_oid, schema_col, name_col, object_type) in sources {
        let schema_idx = col_index(source_oid, schema_col)?;
        let name_idx = col_index(source_oid, name_col)?;
        let dtd_idx = col_index(source_oid, "dtd_identifier")?;
        for row in &source {
            rows.push(build_row(
                relid::IS_DATA_TYPE_PRIVILEGES,
                vec![
                    ("object_catalog", txt(&env.database_name)),
                    ("object_schema", cell(row, schema_idx)?),
                    ("object_name", cell(row, name_idx)?),
                    ("object_type", txt(object_type)),
                    ("dtd_identifier", cell(row, dtd_idx)?),
                ],
            )?);
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AclItem;
    use crate::session::NullSessionInfo;
    use pgarbor_types::IoConfig;

    /// Text of a named column of a row (non-text values are shown in Debug form).
    fn cell_text(rel: Oid, row: &Row, name: &str) -> String {
        let idx = col_index(rel, name).expect("column exists");
        match &row[idx] {
            Datum::Text(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn name_concat_oid_truncates_name_only() {
        assert_eq!(name_concat_oid("abc", 16384), "abc_16384");
        let long = "x".repeat(80);
        let s = name_concat_oid(&long, 16384);
        assert_eq!(s.len(), 63);
        assert!(s.ends_with("_16384"));
    }

    #[test]
    fn explode_orders_by_privilege_and_flags_grant_option() {
        let acl = vec![AclItem {
            grantee: 0,
            grantor: 10,
            privileges: acl_mode::SELECT | acl_mode::INSERT,
            grant_options: acl_mode::SELECT,
        }];
        let grants = explode(&acl, TABLE_PRIVS);
        let names: Vec<&str> = grants.iter().map(|g| g.privilege).collect();
        assert_eq!(names, vec!["INSERT", "SELECT"]);
        let flags: Vec<bool> = grants.iter().map(|g| g.grantable).collect();
        assert_eq!(flags, vec![false, true]);
    }

    #[test]
    fn role_filter_matches_grantor_or_grantee() {
        let set: HashSet<Oid> = [10].into_iter().collect();
        assert!(keep(None, 99, PUBLIC));
        assert!(keep(Some(&set), 10, PUBLIC));
        assert!(keep(Some(&set), 99, 10));
        assert!(!keep(Some(&set), 99, PUBLIC));
        assert!(!keep(Some(&set), 99, 98));
    }

    /// The default ACL of a builtin table is the owner's seven table privileges, all grantable,
    /// with WITH HIERARCHY only for SELECT.
    #[test]
    fn default_table_acl_yields_seven_owner_rows() {
        let (_kv, env) = crate::testing::bootstrapped().expect("bootstrap");
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = crate::vrel::scan(&ctx, relid::IS_TABLE_PRIVILEGES, &ScanRequest::all()).expect("scan");
        let mine: Vec<&Row> = rows
            .iter()
            .filter(|r| {
                cell_text(relid::IS_TABLE_PRIVILEGES, r, "table_schema") == "pg_catalog"
                    && cell_text(relid::IS_TABLE_PRIVILEGES, r, "table_name") == "pg_class"
            })
            .collect();
        let privs: Vec<String> = mine.iter().map(|r| cell_text(relid::IS_TABLE_PRIVILEGES, r, "privilege_type")).collect();
        assert_eq!(privs, ["INSERT", "SELECT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"]);
        for r in &mine {
            assert_eq!(cell_text(relid::IS_TABLE_PRIVILEGES, r, "grantor"), "postgres");
            assert_eq!(cell_text(relid::IS_TABLE_PRIVILEGES, r, "grantee"), "postgres");
            assert_eq!(cell_text(relid::IS_TABLE_PRIVILEGES, r, "is_grantable"), "YES");
            let hierarchy = cell_text(relid::IS_TABLE_PRIVILEGES, r, "with_hierarchy");
            let expected = if cell_text(relid::IS_TABLE_PRIVILEGES, r, "privilege_type") == "SELECT" { "YES" } else { "NO" };
            assert_eq!(hierarchy, expected);
        }
    }

    /// Every builtin procedure grants EXECUTE to PUBLIC, not grantable.
    #[test]
    fn routine_privileges_show_public_execute_on_every_builtin_function() {
        let (_kv, env) = crate::testing::bootstrapped().expect("bootstrap");
        let expected = PROCS.iter().filter(|p| env.snapshot().namespace(p.namespace).is_some()).count();
        let session = NullSessionInfo;
        let io = IoConfig::default();
        let ctx = ScanContext { env: &env, session: &session, io_config: &io };
        let rows = crate::vrel::scan(&ctx, relid::IS_ROUTINE_PRIVILEGES, &ScanRequest::all()).expect("scan");
        let public = rows
            .iter()
            .filter(|r| {
                cell_text(relid::IS_ROUTINE_PRIVILEGES, r, "grantee") == "PUBLIC"
                    && cell_text(relid::IS_ROUTINE_PRIVILEGES, r, "privilege_type") == "EXECUTE"
                    && cell_text(relid::IS_ROUTINE_PRIVILEGES, r, "is_grantable") == "NO"
            })
            .count();
        assert_eq!(public, expected);
    }
}
