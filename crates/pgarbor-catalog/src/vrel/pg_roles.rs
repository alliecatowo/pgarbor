//! Roles, memberships, databases, tablespaces and their views. Lease CAT-34. PG sources:
//! `catalog/pg_authid.h`, `pg_auth_members.h`, `pg_database.h`, `pg_db_role_setting.h`,
//! `pg_tablespace.h`, `catalog/system_views.sql` (pg_roles, pg_shadow, pg_group, pg_user).
//! Roles = `snapshot.indexes().roles` (builtin predefined + persisted `postgres` + user roles),
//! all ordered by OID. Fast paths: eq on `oid`/`rolname`.
//!
//! - **pg_authid**: `oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
//!   rolreplication, rolbypassrls, rolconnlimit, rolpassword` (the stored verifier or NULL),
//!   `rolvaliduntil` (`TimestampTz` or NULL).
//! - **pg_roles**: `rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
//!   rolreplication, rolconnlimit, rolpassword` = the literal text `********` ALWAYS (even when
//!   NULL in pg_authid: oracle), `rolvaliduntil, rolbypassrls, rolconfig` (= `Role::config` as
//!   `text[]`, NULL when empty), `oid`. Note the column ORDER of pg_roles differs from pg_authid:
//!   use `RowBuilder` by name.
//! - **pg_shadow**: roles with `can_login`: `usename, usesysid, usecreatedb, usesuper, userepl,
//!   usebypassrls, passwd` (verifier), `valuntil, useconfig`. **pg_user**: same rows, `passwd` =
//!   `********`.
//! - **pg_group**: roles WITHOUT `can_login`: `groname, grosysid, grolist` = `oid[]` of members
//!   (`RoleMembership::member` where `role` = this role), empty array when none.
//! - **pg_auth_members**: persisted `RoleMembership` rows (`oid, roleid, member, grantor,
//!   admin_option, inherit_option, set_option`) plus the builtin predefined-role grants of
//!   `builtin::AUTH_MEMBERS` (`TextRows`: convert cells with `io::input`). Ordered by `oid`.
//! - **pg_database**: `snapshot.indexes().databases` (template1 1, template0 4, postgres 5):
//!   `oid, datname, datdba, encoding, datlocprovider` (`Char`), `datistemplate, datallowconn,
//!   dathasloginevt` (false), `datconnlimit, datfrozenxid` (`Oid(3)` for persisted rows; builtin
//!   `frozen_xid`), `datminmxid` (1 / builtin `min_mxid`), `dattablespace` 1663 (builtin
//!   `tablespace`), `datcollate, datctype, datlocale, daticurules` NULL, `datcollversion` NULL,
//!   `datacl`.
//! - **pg_db_role_setting**: one row per non-empty `Database::config` (`setdatabase` = db OID,
//!   `setrole` 0) and per non-empty `Role::config` (`setdatabase` 0, `setrole` = role OID);
//!   `setconfig` = `text[]` of the `name=value` strings.
//! - **pg_tablespace**: `builtin::TABLESPACES` (pg_default 1663, pg_global 1664): `oid, spcname,
//!   spcowner, spcacl` NULL, `spcoptions` NULL.
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::model::{Database, Role};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::{io, Datum, TypeId};
use std::collections::HashMap;

/// The password mask shown by `pg_roles.rolpassword` and `pg_user.passwd` (PG view literal).
const PASSWORD_MASK: &str = "********";

/// `pg_database.datfrozenxid` / `datminmxid` for databases written by the bootstrap (not builtin).
const PERSISTED_FROZEN_XID: Oid = 3;
const PERSISTED_MIN_MXID: Oid = 1;

const NO_MEMBERS: &[Oid] = &[];

/// Column type-independent `oid` read of a datum (0 when absent or not an OID).
fn oid_of(datum: Option<&Datum>) -> Oid {
    match datum {
        Some(Datum::Oid(o)) => *o,
        _ => 0,
    }
}

/// The static system relation for a catalog OID.
fn schema_of(rel: Oid) -> Result<&'static SystemRel> {
    builtin::system_rel(rel).ok_or_else(|| PgError::internal(format!("no system relation entry for OID {rel}")))
}

/// `text[]` of a role/database GUC list, NULL when empty (pg_db_role_setting has no row otherwise).
fn config_datum(config: &[String]) -> Datum {
    if config.is_empty() {
        Datum::Null
    } else {
        d::text_array(config)
    }
}

/// Roles in OID order, narrowed by an equality filter on the OID column or the name column when the
/// request carries one (the executor re-applies WHERE, so the narrowing is only a shortcut).
fn roles_sorted<'a>(snap: &'a CatalogSnapshot, req: &ScanRequest, oid_col: usize, name_col: usize) -> Vec<&'a Role> {
    if let Some(oid) = req.oid_eq_on(oid_col) {
        return snap.role(oid).into_iter().collect();
    }
    if let Some(Datum::Text(name)) = req.eq_on(name_col) {
        return snap.role_by_name(name).into_iter().collect();
    }
    let mut roles: Vec<&Role> = snap.indexes().roles.values().collect();
    roles.sort_by_key(|r| r.oid);
    roles
}

/// Databases in OID order.
fn databases_sorted(snap: &CatalogSnapshot) -> Vec<&Database> {
    let mut dbs: Vec<&Database> = snap.indexes().databases.values().collect();
    dbs.sort_by_key(|db| db.oid);
    dbs
}

/// One `pg_authid` row.
fn authid_row(schema: &'static SystemRel, r: &Role) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("oid", d::oid(r.oid))
        .set("rolname", d::name(&r.name))
        .set("rolsuper", d::bool_(r.superuser))
        .set("rolinherit", d::bool_(r.inherit))
        .set("rolcreaterole", d::bool_(r.create_role))
        .set("rolcreatedb", d::bool_(r.create_db))
        .set("rolcanlogin", d::bool_(r.can_login))
        .set("rolreplication", d::bool_(r.replication))
        .set("rolbypassrls", d::bool_(r.bypass_rls))
        .set("rolconnlimit", d::int4(r.conn_limit))
        .set("rolpassword", d::opt_text(r.password.as_deref()))
        .set("rolvaliduntil", d::opt_tstz(r.valid_until));
    b.finish()
}

/// One `pg_roles` row (column order differs from pg_authid; set by name).
fn roles_row(schema: &'static SystemRel, r: &Role) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("rolname", d::name(&r.name))
        .set("rolsuper", d::bool_(r.superuser))
        .set("rolinherit", d::bool_(r.inherit))
        .set("rolcreaterole", d::bool_(r.create_role))
        .set("rolcreatedb", d::bool_(r.create_db))
        .set("rolcanlogin", d::bool_(r.can_login))
        .set("rolreplication", d::bool_(r.replication))
        .set("rolconnlimit", d::int4(r.conn_limit))
        .set("rolpassword", d::text(PASSWORD_MASK))
        .set("rolvaliduntil", d::opt_tstz(r.valid_until))
        .set("rolbypassrls", d::bool_(r.bypass_rls))
        .set("rolconfig", config_datum(&r.config))
        .set("oid", d::oid(r.oid));
    b.finish()
}

/// One `pg_shadow` / `pg_user` row. `mask_password` selects `pg_user` (always `********`).
fn shadow_row(schema: &'static SystemRel, r: &Role, mask_password: bool) -> Result<Row> {
    let passwd = if mask_password {
        d::text(PASSWORD_MASK)
    } else {
        d::opt_text(r.password.as_deref())
    };
    let mut b = RowBuilder::new(schema);
    b.set("usename", d::name(&r.name))
        .set("usesysid", d::oid(r.oid))
        .set("usecreatedb", d::bool_(r.create_db))
        .set("usesuper", d::bool_(r.superuser))
        .set("userepl", d::bool_(r.replication))
        .set("usebypassrls", d::bool_(r.bypass_rls))
        .set("passwd", passwd)
        .set("valuntil", d::opt_tstz(r.valid_until))
        .set("useconfig", config_datum(&r.config));
    b.finish()
}

/// Every `pg_auth_members` row (builtin grants first-class, then persisted), ordered by `oid`.
/// Column order: `oid, roleid, member, grantor, admin_option, inherit_option, set_option`.
fn membership_rows(ctx: &ScanContext) -> Result<Vec<Row>> {
    let schema = schema_of(relid::PG_AUTH_MEMBERS)?;
    let snap = ctx.env.snapshot();
    let ioctx = ctx.io_ctx();
    let mut rows: Vec<Row> = Vec::with_capacity(builtin::AUTH_MEMBERS.rows.len() + snap.data().memberships.len());
    for cells in builtin::AUTH_MEMBERS.rows {
        if cells.len() != schema.columns.len() {
            return Err(PgError::internal(format!(
                "builtin pg_auth_members row has {} cells, expected {}",
                cells.len(),
                schema.columns.len()
            )));
        }
        let mut row: Row = Vec::with_capacity(cells.len());
        for (col, cell) in schema.columns.iter().zip(cells.iter()) {
            row.push(match cell {
                None => Datum::Null,
                Some(text) => io::input(&ioctx, TypeId::of(col.type_oid), text)?,
            });
        }
        rows.push(row);
    }
    for m in snap.data().memberships.values() {
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(m.oid))
            .set("roleid", d::oid(m.role))
            .set("member", d::oid(m.member))
            .set("grantor", d::oid(m.grantor))
            .set("admin_option", d::bool_(m.admin_option))
            .set("inherit_option", d::bool_(m.inherit_option))
            .set("set_option", d::bool_(m.set_option));
        rows.push(b.finish()?);
    }
    rows.sort_by_key(|row| oid_of(row.first()));
    Ok(rows)
}

/// Producer for `pg_authid`.
#[derive(Clone, Copy, Debug)]
pub struct PgAuthid;

impl VirtualRelation for PgAuthid {
    fn oid(&self) -> Oid {
        relid::PG_AUTHID
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        roles_sorted(snap, req, 0, 1).into_iter().map(|r| authid_row(schema, r)).collect()
    }
}

/// Producer for `pg_auth_members`.
#[derive(Clone, Copy, Debug)]
pub struct PgAuthMembers;

impl VirtualRelation for PgAuthMembers {
    fn oid(&self) -> Oid {
        relid::PG_AUTH_MEMBERS
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        membership_rows(ctx)
    }
}

/// Producer for `pg_database`.
#[derive(Clone, Copy, Debug)]
pub struct PgDatabase;

impl VirtualRelation for PgDatabase {
    fn oid(&self) -> Oid {
        relid::PG_DATABASE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        let mut rows = Vec::new();
        for db in databases_sorted(snap) {
            // Persisted databases (the bootstrap `postgres` row) carry PGArbor's own xid values;
            // compiled-in templates carry the PG 18.6 values.
            let persisted = snap.data().databases.contains_key(&db.oid);
            let builtin_row = builtin::DATABASES.iter().find(|b| b.oid == db.oid);
            let (frozen_xid, min_mxid) = match builtin_row {
                Some(b) if !persisted => (b.frozen_xid, b.min_mxid),
                _ => (PERSISTED_FROZEN_XID, PERSISTED_MIN_MXID),
            };
            let tablespace = builtin_row.map(|b| b.tablespace).unwrap_or(builtin::DEFAULT_TABLESPACE);
            let coll_version = builtin_row.and_then(|b| b.coll_version);
            let icu_rules = builtin_row.and_then(|b| b.icu_rules);

            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(db.oid))
                .set("datname", d::name(&db.name))
                .set("datdba", d::oid(db.owner))
                .set("encoding", d::int4(db.encoding))
                .set("datlocprovider", d::char_(db.locale_provider))
                .set("datistemplate", d::bool_(db.is_template))
                .set("datallowconn", d::bool_(db.allow_conn))
                .set("dathasloginevt", d::bool_(false))
                .set("datconnlimit", d::int4(db.conn_limit))
                .set("datfrozenxid", d::oid(frozen_xid))
                .set("datminmxid", d::oid(min_mxid))
                .set("dattablespace", d::oid(tablespace))
                .set("datcollate", d::text(&db.collate))
                .set("datctype", d::text(&db.ctype))
                .set("datlocale", d::opt_text(db.locale.as_deref()))
                .set("daticurules", d::opt_text(icu_rules))
                .set("datcollversion", d::opt_text(coll_version))
                .set("datacl", d::acl_array(db.acl.as_ref(), snap));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_db_role_setting`.
#[derive(Clone, Copy, Debug)]
pub struct PgDbRoleSetting;

impl VirtualRelation for PgDbRoleSetting {
    fn oid(&self) -> Oid {
        relid::PG_DB_ROLE_SETTING
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        let mut rows = Vec::new();
        for db in databases_sorted(snap).into_iter().filter(|db| !db.config.is_empty()) {
            let mut b = RowBuilder::new(schema);
            b.set("setdatabase", d::oid(db.oid))
                .set("setrole", d::oid(0))
                .set("setconfig", d::text_array(&db.config));
            rows.push(b.finish()?);
        }
        for r in roles_sorted(snap, &ScanRequest::all(), 0, 1).into_iter().filter(|r| !r.config.is_empty()) {
            let mut b = RowBuilder::new(schema);
            b.set("setdatabase", d::oid(0))
                .set("setrole", d::oid(r.oid))
                .set("setconfig", d::text_array(&r.config));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_tablespace`.
#[derive(Clone, Copy, Debug)]
pub struct PgTablespace;

impl VirtualRelation for PgTablespace {
    fn oid(&self) -> Oid {
        relid::PG_TABLESPACE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let mut rows = Vec::with_capacity(builtin::TABLESPACES.len());
        for t in builtin::TABLESPACES {
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(t.oid))
                .set("spcname", d::name(t.name))
                .set("spcowner", d::oid(t.owner))
                .set("spcacl", Datum::Null)
                .set("spcoptions", Datum::Null);
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_roles`.
#[derive(Clone, Copy, Debug)]
pub struct PgRoles;

impl VirtualRelation for PgRoles {
    fn oid(&self) -> Oid {
        relid::PG_ROLES
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        // pg_roles: name is column 0, oid is column 12.
        roles_sorted(snap, req, 12, 0).into_iter().map(|r| roles_row(schema, r)).collect()
    }
}

/// Producer for `pg_shadow`.
#[derive(Clone, Copy, Debug)]
pub struct PgShadow;

impl VirtualRelation for PgShadow {
    fn oid(&self) -> Oid {
        relid::PG_SHADOW
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        roles_sorted(snap, req, 1, 0)
            .into_iter()
            .filter(|r| r.can_login)
            .map(|r| shadow_row(schema, r, false))
            .collect()
    }
}

/// Producer for `pg_group`.
#[derive(Clone, Copy, Debug)]
pub struct PgGroup;

impl VirtualRelation for PgGroup {
    fn oid(&self) -> Oid {
        relid::PG_GROUP
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        // Members per role, in pg_auth_members order (builtin grants included).
        let mut members: HashMap<Oid, Vec<Oid>> = HashMap::new();
        for m in membership_rows(ctx)? {
            members.entry(oid_of(m.get(1))).or_default().push(oid_of(m.get(2)));
        }
        let mut rows = Vec::new();
        for r in roles_sorted(snap, req, 1, 0).into_iter().filter(|r| !r.can_login) {
            let grolist = members.get(&r.oid).map(Vec::as_slice).unwrap_or(NO_MEMBERS);
            let mut b = RowBuilder::new(schema);
            b.set("groname", d::name(&r.name))
                .set("grosysid", d::oid(r.oid))
                .set("grolist", d::oid_array(grolist));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_user`.
#[derive(Clone, Copy, Debug)]
pub struct PgUser;

impl VirtualRelation for PgUser {
    fn oid(&self) -> Oid {
        relid::PG_USER
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let snap = ctx.env.snapshot();
        roles_sorted(snap, req, 1, 0)
            .into_iter()
            .filter(|r| r.can_login)
            .map(|r| shadow_row(schema, r, true))
            .collect()
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] =
    &[&PgAuthid, &PgAuthMembers, &PgDatabase, &PgDbRoleSetting, &PgTablespace, &PgRoles, &PgShadow, &PgGroup, &PgUser];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    fn col(rel: Oid, name: &str) -> usize {
        builtin::system_rel(rel).unwrap().columns.iter().position(|c| c.name == name).unwrap()
    }

    fn scan_rel(env: &CatalogEnv, oid: Oid, req: &ScanRequest) -> Vec<Row> {
        let session = NullSessionInfo;
        let cfg = IoConfig::default();
        let ctx = ScanContext { env, session: &session, io_config: &cfg };
        crate::vrel::scan(&ctx, oid, req).unwrap()
    }

    fn is_text(datum: &Datum, want: &str) -> bool {
        matches!(datum, Datum::Text(s) if s == want)
    }

    #[test]
    fn pg_roles_masks_password_and_hides_empty_config() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_ROLES, &ScanRequest::all());
        assert_eq!(rows.len(), env.snapshot().indexes().roles.len());
        let pw = col(relid::PG_ROLES, "rolpassword");
        let cfg = col(relid::PG_ROLES, "rolconfig");
        for r in &rows {
            assert!(is_text(&r[pw], PASSWORD_MASK));
            assert!(matches!(r[cfg], Datum::Null));
        }
    }

    #[test]
    fn pg_authid_fast_path_on_oid() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let req = ScanRequest { eq: vec![(col(relid::PG_AUTHID, "oid"), Datum::Oid(10))] };
        let rows = scan_rel(&env, relid::PG_AUTHID, &req);
        assert_eq!(rows.len(), 1);
        assert!(is_text(&rows[0][col(relid::PG_AUTHID, "rolname")], "postgres"));
        assert!(matches!(rows[0][col(relid::PG_AUTHID, "rolpassword")], Datum::Null));
    }

    #[test]
    fn pg_user_lists_only_login_roles() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_USER, &ScanRequest::all());
        assert_eq!(rows.len(), 1);
        assert!(is_text(&rows[0][col(relid::PG_USER, "usename")], "postgres"));
        assert!(is_text(&rows[0][col(relid::PG_USER, "passwd")], PASSWORD_MASK));
    }

    #[test]
    fn pg_group_lists_non_login_roles_with_members() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_GROUP, &ScanRequest::all());
        let non_login = env.snapshot().indexes().roles.values().filter(|r| !r.can_login).count();
        assert_eq!(rows.len(), non_login);
        let name = col(relid::PG_GROUP, "groname");
        let list = col(relid::PG_GROUP, "grolist");
        let settings = rows.iter().find(|r| is_text(&r[name], "pg_read_all_settings")).unwrap();
        // pg_monitor (3373) is granted pg_read_all_settings (3374) by the builtin pg_auth_members rows.
        assert!(matches!(&settings[list], Datum::Array(a) if a.items.iter().any(|x| matches!(x, Datum::Oid(3373)))));
    }

    #[test]
    fn pg_auth_members_includes_builtin_grants() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_AUTH_MEMBERS, &ScanRequest::all());
        assert_eq!(rows.len(), builtin::AUTH_MEMBERS.rows.len() + env.snapshot().data().memberships.len());
        assert!(matches!(rows[0][0], Datum::Oid(10308)));
        assert!(matches!(rows[0][1], Datum::Oid(3374)));
        assert!(matches!(rows[0][2], Datum::Oid(3373)));
        assert!(matches!(rows[0][4], Datum::Bool(false)));
    }

    #[test]
    fn pg_database_lists_three_databases_with_c_utf8() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_DATABASE, &ScanRequest::all());
        assert_eq!(rows.len(), 3);
        let collate = col(relid::PG_DATABASE, "datcollate");
        let provider = col(relid::PG_DATABASE, "datlocprovider");
        let oids: Vec<Oid> = rows.iter().map(|r| oid_of(r.first())).collect();
        assert_eq!(oids, vec![1, 4, 5]);
        for r in &rows {
            assert!(is_text(&r[collate], "C.UTF-8"));
            assert!(matches!(r[provider], Datum::Char(b'b')));
        }
    }

    #[test]
    fn pg_tablespace_lists_default_and_global() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let rows = scan_rel(&env, relid::PG_TABLESPACE, &ScanRequest::all());
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0][0], Datum::Oid(1663)));
        assert!(matches!(rows[1][0], Datum::Oid(1664)));
    }

    #[test]
    fn pg_db_role_setting_empty_at_bootstrap() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert!(scan_rel(&env, relid::PG_DB_ROLE_SETTING, &ScanRequest::all()).is_empty());
    }
}
