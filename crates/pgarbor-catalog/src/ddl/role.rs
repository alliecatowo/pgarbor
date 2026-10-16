//! Roles and role membership: CREATE/ALTER/DROP ROLE, ALTER ROLE ... SET, GRANT/REVOKE role.
//! Lease CAT-10. PG sources: `commands/user.c` (`CreateRole`, `AlterRole`, `DropRole`,
//! `RenameRole`, `GrantRole`, `AddRoleMems`, `DelRoleMems`, `check_role_membership_authorization`,
//! `roleSpecsToIds`), `utils/adt/acl.c` (`is_admin_of_role`, `roles_is_member_of`).
//!
//! Roles are CLUSTER-wide in PG; in PGArbor they live in the branch catalog like everything else (a
//! branch forks its roles). Predefined roles (`pg_monitor`, `pg_read_all_data`, ...) are builtin (not
//! persisted): they cannot be created, renamed, altered or dropped (CREATE/RENAME to a `pg_` name:
//! 42939 `errors::reserved_role_name`; ALTER/DROP of a builtin role: 42501
//! `errors::permission_denied_to("alter role")` / `("drop role")` with DETAIL `The role "pg_x" is reserved.`
//! style text from PG 18 `user.c`).
//!
//! Oracle-verified (PG 18.6):
//! - `create role postgres` -> 42710 `role "postgres" already exists` (`errors::role_already_exists`)
//! - `create role pg_x` -> 42939 `role name "pg_x" is reserved` DETAIL `Role names starting with "pg_" are reserved.`
//! - `drop role nosuch` -> 42704 `role "nosuch" does not exist`; IF EXISTS -> NOTICE `role "nosuch" does not exist, skipping`
//! - `drop role` of the current user -> 55006 `current user cannot be dropped`
//! - dropping a role that owns objects -> 2BP01 `role "r" cannot be dropped because some objects
//!   depend on it` DETAIL `owner of table zz.t` lines (`errors::role_cannot_be_dropped`; every object
//!   whose `owner` is the role, in OID order, described by `describe`; plus `privileges for ...` for
//!   ACL grants to the role)
//! - GRANT creating a cycle -> 0LP01 `role "a" is a member of role "b"` (`errors::circular_membership`)
//!
//! Implementation notes (CAT-10):
//! - `MembershipSpec` carries plain booleans, so for an EXISTING grant `admin_option = true` counts as
//!   "ADMIN specified" and `set_option = false` as "SET specified" (their non-default values);
//!   `inherit_option` is explicit when `Some`. A new row uses the spec values (PG defaults: admin false,
//!   set true, inherit = member's `inherit`).
//! - `MembershipSpec::grantor` 0, or equal to the current user, means "not specified" (PG infers the
//!   grantor: the bootstrap superuser for superusers, else the best admin of the role).
//! - The three builtin `pg_auth_members` rows are read-only edges in the membership closure.
//! - DROP ROLE of a pinned role (OID below 16384) after the permission checks: 2BP01 `cannot drop role
//!   pg_x because it is required by the database system` (PG `checkSharedDependencies`).
use super::spec::*;
use crate::builtin::{BOOTSTRAP_SUPERUSER, PG_DATABASE_OWNER};
use crate::data::CatalogData;
use crate::errors;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState, FIRST_NORMAL_OID};

// ------------------------------------------------------------------ private helpers (free functions:
// inherent method names would collide with other leases' private methods on `CatalogTxn`).

/// The attributes of a role the checks below need.
#[derive(Clone, Debug)]
struct Attrs {
    name: String,
    superuser: bool,
    inherit: bool,
    create_role: bool,
    create_db: bool,
    replication: bool,
    bypass_rls: bool,
    password: Option<String>,
}

fn attrs(d: &CatalogData, oid: Oid) -> Option<Attrs> {
    if let Some(r) = d.roles.get(&oid) {
        return Some(Attrs {
            name: r.name.clone(),
            superuser: r.superuser,
            inherit: r.inherit,
            create_role: r.create_role,
            create_db: r.create_db,
            replication: r.replication,
            bypass_rls: r.bypass_rls,
            password: r.password.clone(),
        });
    }
    crate::builtin::ROLES.iter().find(|b| b.oid == oid).map(|b| Attrs {
        name: b.name.to_string(),
        superuser: b.superuser,
        inherit: b.inherit,
        create_role: b.create_role,
        create_db: b.create_db,
        replication: b.replication,
        bypass_rls: b.bypass_rls,
        password: None,
    })
}

fn name_of(d: &CatalogData, oid: Oid) -> String {
    attrs(d, oid).map(|a| a.name).unwrap_or_else(|| format!("unknown (OID={oid})"))
}

/// Superuser check (`superuser_arg`): the bootstrap superuser always is.
fn is_super(d: &CatalogData, oid: Oid) -> bool {
    oid == BOOTSTRAP_SUPERUSER || attrs(d, oid).is_some_and(|a| a.superuser)
}

fn has_createrole(d: &CatalogData, oid: Oid) -> bool {
    is_super(d, oid) || attrs(d, oid).is_some_and(|a| a.create_role)
}

fn has_createdb(d: &CatalogData, oid: Oid) -> bool {
    is_super(d, oid) || attrs(d, oid).is_some_and(|a| a.create_db)
}

fn has_replication(d: &CatalogData, oid: Oid) -> bool {
    is_super(d, oid) || attrs(d, oid).is_some_and(|a| a.replication)
}

fn has_bypassrls(d: &CatalogData, oid: Oid) -> bool {
    is_super(d, oid) || attrs(d, oid).is_some_and(|a| a.bypass_rls)
}

fn role_name_taken(d: &CatalogData, name: &str) -> bool {
    d.roles.values().any(|r| r.name == name) || crate::builtin::ROLES.iter().any(|b| b.name == name)
}

fn notice_msg(state: SqlState, msg: String) -> PgError {
    PgError::new(state, msg).with_severity(Severity::Notice)
}

fn warning_msg(msg: String) -> PgError {
    PgError::new(SqlState::WARNING, msg).with_severity(Severity::Warning)
}

fn invalid_grant_operation(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState(*b"0LP01"), msg)
}

fn reserved_name(name: &str) -> bool {
    name.starts_with("pg_")
}

/// PG `get_password_type` classification: only verifiers are storable.
fn is_hashed_password(p: &str) -> bool {
    if p.starts_with("SCRAM-SHA-256$") {
        return true;
    }
    p.len() == 35
        && p.starts_with("md5")
        && p.as_bytes()[3..].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Validate a password for storage. `Ok(None)` = cleared (empty string; the caller queues the NOTICE).
fn check_password(p: &str) -> Result<Option<String>> {
    if p.is_empty() {
        return Ok(None);
    }
    if !is_hashed_password(p) {
        return Err(errors::password_not_hashed());
    }
    Ok(Some(p.to_string()))
}

fn empty_password_notice() -> PgError {
    notice_msg(SqlState::SUCCESSFUL_COMPLETION, "empty string is not a valid password, clearing password".to_string())
}

fn check_conn_limit(limit: i32) -> Result<()> {
    if limit < -1 {
        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid connection limit: {limit}")));
    }
    Ok(())
}

/// Builtin `pg_auth_members` rows (read-only edges).
fn builtin_memberships() -> Vec<RoleMembership> {
    let mut out = Vec::new();
    for row in crate::builtin::AUTH_MEMBERS.rows {
        let oid = |i: usize| row.get(i).copied().flatten().and_then(|s| s.parse::<Oid>().ok());
        let flag = |i: usize| row.get(i).copied().flatten().is_some_and(|s| s == "t");
        if let (Some(o), Some(r), Some(m), Some(g)) = (oid(0), oid(1), oid(2), oid(3)) {
            out.push(RoleMembership {
                oid: o,
                role: r,
                member: m,
                grantor: g,
                admin_option: flag(4),
                inherit_option: flag(5),
                set_option: flag(6),
            });
        }
    }
    out
}

/// All effective memberships: builtin first, then persisted in OID order.
fn memberships(d: &CatalogData) -> Vec<RoleMembership> {
    let mut v = builtin_memberships();
    v.extend(d.memberships.values().cloned());
    v
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recurse {
    Members,
    Privs,
}

/// PG `roles_is_member_of`: (closure list starting with `start`, first role found holding ADMIN OPTION on
/// `admin_of` or 0).
fn roles_closure(edges: &[RoleMembership], start: Oid, kind: Recurse, admin_of: Oid) -> (Vec<Oid>, Oid) {
    let mut list = vec![start];
    let mut admin_role: Oid = 0;
    let mut i = 0;
    while i < list.len() {
        let memberid = list[i];
        i += 1;
        for e in edges.iter().filter(|e| e.member == memberid) {
            if admin_of != 0 && e.role == admin_of && e.admin_option && admin_role == 0 {
                admin_role = memberid;
            }
            if kind == Recurse::Privs && !e.inherit_option {
                continue;
            }
            if !list.contains(&e.role) {
                list.push(e.role);
            }
        }
    }
    (list, admin_role)
}

fn is_admin_of_role(d: &CatalogData, member: Oid, role: Oid) -> bool {
    if is_super(d, member) {
        return true;
    }
    if member == role {
        return false;
    }
    roles_closure(&memberships(d), member, Recurse::Members, role).1 != 0
}

fn select_best_admin(d: &CatalogData, member: Oid, role: Oid) -> Oid {
    if member == role {
        return 0;
    }
    roles_closure(&memberships(d), member, Recurse::Privs, role).1
}

fn has_privs_of_role(d: &CatalogData, member: Oid, role: Oid) -> bool {
    member == role
        || is_super(d, member)
        || roles_closure(&memberships(d), member, Recurse::Privs, 0).0.contains(&role)
}

fn is_member_of_role_nosuper(d: &CatalogData, member: Oid, role: Oid) -> bool {
    member == role || roles_closure(&memberships(d), member, Recurse::Members, 0).0.contains(&role)
}

/// PG `check_role_membership_authorization`.
fn check_membership_authorization(d: &CatalogData, current: Oid, roleid: Oid, is_grant: bool) -> Result<()> {
    let rn = name_of(d, roleid);
    if is_grant && roleid == PG_DATABASE_OWNER {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("role \"{rn}\" cannot have explicit members")));
    }
    let verb = if is_grant { "grant" } else { "revoke" };
    if is_super(d, roleid) {
        if !is_super(d, current) {
            return Err(errors::permission_denied_to(&format!("{verb} role \"{rn}\"")).with_detail(format!(
                "Only roles with the SUPERUSER attribute may {verb} roles with the SUPERUSER attribute."
            )));
        }
    } else if !is_admin_of_role(d, current, roleid) {
        return Err(errors::permission_denied_to(&format!("{verb} role \"{rn}\""))
            .with_detail(format!("Only roles with the ADMIN option on role \"{rn}\" may {verb} this role.")));
    }
    Ok(())
}

/// PG `check_role_grantor`: validate or infer the grantor (`grantor` 0 = not specified).
fn check_role_grantor(d: &CatalogData, current: Oid, roleid: Oid, grantor: Oid, is_grant: bool) -> Result<Oid> {
    if grantor == 0 {
        if is_super(d, current) {
            return Ok(BOOTSTRAP_SUPERUSER);
        }
        let g = select_best_admin(d, current, roleid);
        if g == 0 {
            return Err(PgError::internal("no possible grantors"));
        }
        return Ok(g);
    }
    let gname = name_of(d, grantor);
    if is_grant {
        if !has_privs_of_role(d, current, grantor) {
            return Err(errors::permission_denied_to(&format!("grant privileges as role \"{gname}\""))
                .with_detail(format!("Only roles with privileges of role \"{gname}\" may grant privileges as this role.")));
        }
        if grantor != BOOTSTRAP_SUPERUSER && select_best_admin(d, grantor, roleid) != grantor {
            return Err(errors::permission_denied_to(&format!("grant privileges as role \"{gname}\""))
                .with_detail(format!("The grantor must have the ADMIN option on role \"{}\".", name_of(d, roleid))));
        }
    } else if !has_privs_of_role(d, current, grantor) {
        return Err(errors::permission_denied_to(&format!("revoke privileges granted by role \"{gname}\""))
            .with_detail(format!("Only roles with privileges of role \"{gname}\" may revoke privileges granted by this role.")));
    }
    Ok(grantor)
}

/// A grantor equal to the current user means "not specified" (the executor fills the current user in).
fn normalize_grantor(current: Oid, grantor: Oid) -> Oid {
    if grantor == current {
        0
    } else {
        grantor
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RevokeAction {
    Noop,
    DeleteGrant,
    RemoveAdmin,
}

fn dependent_privileges_exist() -> PgError {
    PgError::new(SqlState::DEPENDENT_OBJECTS_STILL_EXIST, "dependent privileges exist")
        .with_hint("Use CASCADE to revoke them too.")
}

/// PG `plan_recursive_revoke`.
fn plan_recursive_revoke(
    mem: &[RoleMembership],
    actions: &mut [RevokeAction],
    index: usize,
    admin_only: bool,
    cascade: bool,
) -> Result<()> {
    if actions[index] == RevokeAction::DeleteGrant {
        return Ok(());
    }
    if actions[index] == RevokeAction::RemoveAdmin && admin_only {
        return Ok(());
    }
    let m = &mem[index];
    if !admin_only {
        actions[index] = RevokeAction::DeleteGrant;
        if !m.admin_option {
            return Ok(());
        }
    } else {
        if !m.admin_option {
            return Ok(());
        }
        actions[index] = RevokeAction::RemoveAdmin;
    }
    let would_still_have_admin =
        mem.iter().enumerate().any(|(i, e)| e.member == m.member && e.admin_option && actions[i] == RevokeAction::Noop);
    if would_still_have_admin {
        return Ok(());
    }
    for i in 0..mem.len() {
        if mem[i].grantor == m.member && actions[i] != RevokeAction::DeleteGrant {
            if !cascade {
                return Err(dependent_privileges_exist());
            }
            plan_recursive_revoke(mem, actions, i, false, cascade)?;
        }
    }
    Ok(())
}

/// PG `plan_single_revoke`: false if no matching grant.
fn plan_single_revoke(
    mem: &[RoleMembership],
    actions: &mut [RevokeAction],
    member: Oid,
    grantor: Oid,
    admin_only: bool,
    cascade: bool,
) -> Result<bool> {
    for i in 0..mem.len() {
        if mem[i].member == member && mem[i].grantor == grantor {
            plan_recursive_revoke(mem, actions, i, admin_only, cascade)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// PG `plan_member_revoke` (always CASCADE).
fn plan_member_revoke(mem: &[RoleMembership], actions: &mut [RevokeAction], member: Oid) -> Result<()> {
    for i in 0..mem.len() {
        if mem[i].member == member {
            plan_recursive_revoke(mem, actions, i, false, true)?;
        }
    }
    Ok(())
}

/// Options of one GRANT role (`None` = not specified; insert defaults: admin false, set true, inherit =
/// member's `rolinherit`).
#[derive(Clone, Copy, Default)]
struct GrantOpts {
    admin: Option<bool>,
    inherit: Option<bool>,
    set: Option<bool>,
}

fn already_granted_notice(d: &CatalogData, member: Oid, rolename: &str, grantor: Oid) -> PgError {
    notice_msg(
        SqlState::SUCCESSFUL_COMPLETION,
        format!(
            "role \"{}\" has already been granted membership in role \"{}\" by role \"{}\"",
            name_of(d, member),
            rolename,
            name_of(d, grantor)
        ),
    )
}

/// PG `AddRoleMems`: add `members` to `roleid`. `grantor_in` 0 = infer.
fn add_role_mems(
    txn: &mut CatalogTxn<'_>,
    current: Oid,
    roleid: Oid,
    members: &[Oid],
    grantor_in: Oid,
    opts: GrantOpts,
) -> Result<()> {
    let grantor = check_role_grantor(&txn.data, current, roleid, grantor_in, true)?;
    let rolename = name_of(&txn.data, roleid);

    for &m in members {
        if m == PG_DATABASE_OWNER {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("role \"{}\" cannot be a member of any role", name_of(&txn.data, m)),
            ));
        }
        if is_member_of_role_nosuper(&txn.data, roleid, m) {
            return Err(errors::circular_membership(&rolename, &name_of(&txn.data, m)));
        }
    }

    if opts.admin.unwrap_or(false) && grantor != BOOTSTRAP_SUPERUSER {
        let mem: Vec<RoleMembership> = memberships(&txn.data).into_iter().filter(|e| e.role == roleid).collect();
        let mut actions = vec![RevokeAction::Noop; mem.len()];
        let back = || invalid_grant_operation("ADMIN option cannot be granted back to your own grantor");
        for &m in members {
            if m == BOOTSTRAP_SUPERUSER {
                return Err(back());
            }
            plan_member_revoke(&mem, &mut actions, m)?;
        }
        let still = mem
            .iter()
            .enumerate()
            .any(|(i, e)| actions[i] == RevokeAction::Noop && e.member == grantor && e.admin_option);
        if !still {
            return Err(back());
        }
    }

    let builtin = builtin_memberships();
    for &m in members {
        let existing = txn
            .data
            .memberships
            .values()
            .find(|e| e.role == roleid && e.member == m && e.grantor == grantor)
            .cloned();
        if let Some(mut e) = existing {
            let mut changed = false;
            if let Some(a) = opts.admin {
                if e.admin_option != a {
                    e.admin_option = a;
                    changed = true;
                }
            }
            if let Some(i) = opts.inherit {
                if e.inherit_option != i {
                    e.inherit_option = i;
                    changed = true;
                }
            }
            if let Some(s) = opts.set {
                if e.set_option != s {
                    e.set_option = s;
                    changed = true;
                }
            }
            if !changed {
                let n = already_granted_notice(&txn.data, m, &rolename, grantor);
                txn.notice(n);
                continue;
            }
            txn.put(CatalogObject::RoleMembership(e))?;
            continue;
        }
        if let Some(b) = builtin.iter().find(|e| e.role == roleid && e.member == m && e.grantor == grantor) {
            let differs = opts.admin.is_some_and(|a| a != b.admin_option)
                || opts.inherit.is_some_and(|i| i != b.inherit_option)
                || opts.set.is_some_and(|s| s != b.set_option);
            if differs {
                return Err(PgError::unimplemented("CAT-10", "changing options of a builtin role membership"));
            }
            let n = already_granted_notice(&txn.data, m, &rolename, grantor);
            txn.notice(n);
            continue;
        }
        let member_inherit =
            attrs(&txn.data, m).map(|a| a.inherit).ok_or_else(|| errors::cache_lookup_failed("role", m))?;
        let oid = txn.new_oid()?;
        txn.put(CatalogObject::RoleMembership(RoleMembership {
            oid,
            role: roleid,
            member: m,
            grantor,
            admin_option: opts.admin.unwrap_or(false),
            inherit_option: opts.inherit.unwrap_or(member_inherit),
            set_option: opts.set.unwrap_or(true),
        }))?;
    }
    Ok(())
}

fn builtin_role_error(action: &str, name: &str) -> PgError {
    errors::permission_denied_to(action).with_detail(format!("The role \"{name}\" is reserved."))
}

impl<'a> CatalogTxn<'a> {
    /// CREATE ROLE. Requires the CREATEROLE attribute (or superuser); creating a SUPERUSER /
    /// REPLICATION / BYPASSRLS role needs the corresponding attribute yourself (superuser for the
    /// first); else 42501 `errors::permission_denied_create_role`. Checks: reserved `pg_` prefix
    /// (42939), duplicate (42710), password hashed (22023 `errors::password_not_hashed` unless it
    /// starts with `SCRAM-SHA-256$` or is `md5` + 32 hex). Inserts `Role{..}`; processes `in_roles`,
    /// `members`, `admins` through the membership logic below (grantor = current user). Returns the
    /// new OID.
    // CONTRACT
    pub fn create_role(&mut self, spec: &CreateRoleSpec) -> Result<Oid> {
        let cur = self.current_user();
        check_conn_limit(spec.conn_limit)?;

        if !is_super(&self.data, cur) {
            let need = |attr: &str| {
                errors::permission_denied_to("create role").with_detail(format!(
                    "Only roles with the {attr} attribute may create roles with the {attr} attribute."
                ))
            };
            if !has_createrole(&self.data, cur) {
                return Err(errors::permission_denied_create_role());
            }
            if spec.superuser {
                return Err(need("SUPERUSER"));
            }
            if spec.create_db && !has_createdb(&self.data, cur) {
                return Err(need("CREATEDB"));
            }
            if spec.replication && !has_replication(&self.data, cur) {
                return Err(need("REPLICATION"));
            }
            if spec.bypass_rls && !has_bypassrls(&self.data, cur) {
                return Err(need("BYPASSRLS"));
            }
        }

        if reserved_name(&spec.name) {
            return Err(errors::reserved_role_name(&spec.name));
        }
        if role_name_taken(&self.data, &spec.name) {
            return Err(errors::role_already_exists(&spec.name));
        }

        let mut password = None;
        if let Some(p) = &spec.password {
            password = check_password(p)?;
            if password.is_none() {
                self.notice(empty_password_notice());
            }
        }

        let oid = self.new_oid()?;
        self.put(CatalogObject::Role(Role {
            oid,
            name: spec.name.clone(),
            superuser: spec.superuser,
            inherit: spec.inherit,
            create_role: spec.create_role,
            create_db: spec.create_db,
            can_login: spec.can_login,
            replication: spec.replication,
            bypass_rls: spec.bypass_rls,
            conn_limit: spec.conn_limit,
            password,
            valid_until: spec.valid_until,
            config: Vec::new(),
        }))?;
        self.record_created(ObjectAddress::new(ObjClass::Role, oid));

        // IN ROLE: the new role becomes a member of each.
        for &r in &spec.in_roles {
            if attrs(&self.data, r).is_none() {
                return Err(errors::cache_lookup_failed("role", r));
            }
            check_membership_authorization(&self.data, cur, r, true)?;
            add_role_mems(self, cur, r, &[oid], 0, GrantOpts::default())?;
        }

        // A non-superuser creator administers the role it created (grantor: the bootstrap superuser).
        if !is_super(&self.data, cur) {
            let o = GrantOpts { admin: Some(true), inherit: Some(false), set: Some(false) };
            add_role_mems(self, BOOTSTRAP_SUPERUSER, oid, &[cur], BOOTSTRAP_SUPERUSER, o)?;
        }

        if !spec.members.is_empty() {
            add_role_mems(self, cur, oid, &spec.members, 0, GrantOpts::default())?;
        }
        if !spec.admins.is_empty() {
            add_role_mems(self, cur, oid, &spec.admins, 0, GrantOpts { admin: Some(true), ..GrantOpts::default() })?;
        }
        Ok(oid)
    }

    /// ALTER ROLE `role` attribute changes (`user.c AlterRole`): the superuser-only attributes
    /// (SUPERUSER, REPLICATION, BYPASSRLS) need superuser; the others need CREATEROLE plus ADMIN OPTION
    /// on the target role; any role may change its own PASSWORD. The bootstrap superuser (OID 10)
    /// cannot lose SUPERUSER: 42501 `permission denied to alter role` DETAIL `The bootstrap
    /// superuser must have the SUPERUSER attribute.`
    // CONTRACT
    pub fn alter_role(&mut self, role: Oid, changes: &RoleChanges) -> Result<()> {
        let cur = self.current_user();
        if let Some(l) = changes.conn_limit {
            check_conn_limit(l)?;
        }
        let target = attrs(&self.data, role).ok_or_else(|| errors::cache_lookup_failed("role", role))?;
        if !self.data.roles.contains_key(&role) {
            return Err(builtin_role_error("alter role", &target.name));
        }

        let cur_super = is_super(&self.data, cur);
        if !cur_super && target.superuser {
            return Err(errors::permission_denied_to("alter role")
                .with_detail("Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute."));
        }
        if !cur_super && changes.superuser.is_some() {
            return Err(errors::permission_denied_to("alter role")
                .with_detail("Only roles with the SUPERUSER attribute may change the SUPERUSER attribute."));
        }

        if !has_createrole(&self.data, cur) || !is_admin_of_role(&self.data, cur, role) {
            if changes.inherit.is_some()
                || changes.create_role.is_some()
                || changes.create_db.is_some()
                || changes.can_login.is_some()
                || changes.conn_limit.is_some()
                || changes.valid_until.is_some()
                || changes.replication.is_some()
                || changes.bypass_rls.is_some()
            {
                return Err(errors::permission_denied_to("alter role").with_detail(format!(
                    "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                    target.name
                )));
            }
            if changes.password.is_some() && role != cur {
                return Err(errors::permission_denied_to("alter role").with_detail(
                    "To change another role's password, the current user must have the CREATEROLE attribute and the ADMIN option on the role.",
                ));
            }
        } else if !cur_super {
            let change_attr = |attr: &str| {
                errors::permission_denied_to("alter role")
                    .with_detail(format!("Only roles with the {attr} attribute may change the {attr} attribute."))
            };
            if changes.create_db.is_some() && !has_createdb(&self.data, cur) {
                return Err(change_attr("CREATEDB"));
            }
            if changes.replication.is_some() && !has_replication(&self.data, cur) {
                return Err(change_attr("REPLICATION"));
            }
            if changes.bypass_rls.is_some() && !has_bypassrls(&self.data, cur) {
                return Err(change_attr("BYPASSRLS"));
            }
        }

        // PG raises ERRCODE_FEATURE_NOT_SUPPORTED (0A000) here (the doc comment above says 42501).
        if changes.superuser == Some(false) && role == BOOTSTRAP_SUPERUSER {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "permission denied to alter role")
                .with_detail("The bootstrap superuser must have the SUPERUSER attribute."));
        }

        // Password: Some(Some(p)) sets, Some(None) clears.
        let new_password: Option<Option<String>> = match &changes.password {
            None => None,
            Some(None) => Some(None),
            Some(Some(p)) => {
                let v = check_password(p)?;
                if v.is_none() {
                    self.notice(empty_password_notice());
                }
                Some(v)
            }
        };

        self.update_role(role, |r| {
            if let Some(v) = changes.superuser {
                r.superuser = v;
            }
            if let Some(v) = changes.inherit {
                r.inherit = v;
            }
            if let Some(v) = changes.create_role {
                r.create_role = v;
            }
            if let Some(v) = changes.create_db {
                r.create_db = v;
            }
            if let Some(v) = changes.can_login {
                r.can_login = v;
            }
            if let Some(v) = changes.replication {
                r.replication = v;
            }
            if let Some(v) = changes.bypass_rls {
                r.bypass_rls = v;
            }
            if let Some(v) = changes.conn_limit {
                r.conn_limit = v;
            }
            if let Some(p) = &new_password {
                r.password = p.clone();
            }
            if let Some(v) = &changes.valid_until {
                r.valid_until = *v;
            }
            Ok(())
        })
    }

    /// ALTER ROLE `role` RENAME TO `new_name`: duplicate 42710; `pg_` prefix 42939; the session /
    /// current user cannot be renamed (`session user cannot be renamed`, 0A000); a role with an MD5
    /// password loses it in PG (`MD5 password cleared because of role rename` NOTICE) - only when
    /// the stored verifier starts with `md5` (SCRAM verifiers are kept).
    // CONTRACT
    pub fn rename_role(&mut self, role: Oid, new_name: &str) -> Result<()> {
        let cur = self.current_user();
        let target = attrs(&self.data, role).ok_or_else(|| errors::cache_lookup_failed("role", role))?;
        if role == self.env.session_user() {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "session user cannot be renamed"));
        }
        if role == cur {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "current user cannot be renamed"));
        }
        if reserved_name(&target.name) {
            return Err(errors::reserved_role_name(&target.name));
        }
        if reserved_name(new_name) {
            return Err(errors::reserved_role_name(new_name));
        }
        if role_name_taken(&self.data, new_name) {
            return Err(errors::role_already_exists(new_name));
        }
        if target.superuser {
            if !is_super(&self.data, cur) {
                return Err(errors::permission_denied_to("rename role").with_detail(
                    "Only roles with the SUPERUSER attribute may rename roles with the SUPERUSER attribute.",
                ));
            }
        } else if !has_createrole(&self.data, cur) || !is_admin_of_role(&self.data, cur, role) {
            return Err(errors::permission_denied_to("rename role").with_detail(format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may rename this role.",
                target.name
            )));
        }

        let clear_md5 = target.password.as_deref().is_some_and(|p| p.starts_with("md5"));
        if clear_md5 {
            self.notice(notice_msg(
                SqlState::SUCCESSFUL_COMPLETION,
                "MD5 password cleared because of role rename".to_string(),
            ));
        }
        self.update_role(role, |r| {
            r.name = new_name.to_string();
            if clear_md5 {
                r.password = None;
            }
            Ok(())
        })
    }

    /// ALTER ROLE `role` SET name = value / RESET (`value = None`): updates `Role::config`.
    // CONTRACT
    pub fn set_role_config(&mut self, role: Oid, guc: &str, value: Option<&str>) -> Result<()> {
        let cur = self.current_user();
        let target = attrs(&self.data, role).ok_or_else(|| errors::cache_lookup_failed("role", role))?;
        if !self.data.roles.contains_key(&role) {
            return Err(builtin_role_error("alter role", &target.name));
        }
        if target.superuser {
            if !is_super(&self.data, cur) {
                return Err(errors::permission_denied_to("alter role")
                    .with_detail("Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute."));
            }
        } else if (!has_createrole(&self.data, cur) || !is_admin_of_role(&self.data, cur, role)) && role != cur {
            return Err(errors::permission_denied_to("alter role").with_detail(format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                target.name
            )));
        }

        let matches =
            |entry: &str| entry.split_once('=').is_some_and(|(n, _)| n.eq_ignore_ascii_case(guc));
        self.update_role(role, |r| {
            match value {
                Some(v) => {
                    let item = format!("{guc}={v}");
                    if let Some(slot) = r.config.iter_mut().find(|e| matches(e.as_str())) {
                        *slot = item;
                    } else {
                        r.config.push(item);
                    }
                }
                None => r.config.retain(|e| !matches(e.as_str())),
            }
            Ok(())
        })
    }

    /// DROP ROLE `role` (already resolved): checks in PG order - not the current/session user
    /// (55006), not a builtin role, caller has CREATEROLE + admin (or superuser; dropping a
    /// superuser needs superuser), no owned objects / no ACL grants anywhere (2BP01
    /// `errors::role_cannot_be_dropped`; the role must be the `owner` of nothing: scan relations,
    /// indexes, sequences, types, functions, schemas, databases; also the grantee/grantor in any
    /// ACL). Then removes the role row, all `RoleMembership` rows where it is role or member, its
    /// descriptions, and `Role::config`. Roles that merely HAVE privileges (acl entries) block the
    /// drop with `privileges for <object>` detail lines.
    // CONTRACT
    pub fn drop_role(&mut self, role: Oid) -> Result<()> {
        let cur = self.current_user();
        if !has_createrole(&self.data, cur) {
            return Err(errors::permission_denied_to("drop role").with_detail(
                "Only roles with the CREATEROLE attribute and the ADMIN option on the target roles may drop roles.",
            ));
        }
        let target = attrs(&self.data, role).ok_or_else(|| errors::cache_lookup_failed("role", role))?;
        if role == cur {
            return Err(errors::cannot_drop_current_user());
        }
        if role == self.env.session_user() {
            return Err(errors::cannot_drop_session_user());
        }
        if target.superuser && !is_super(&self.data, cur) {
            return Err(errors::permission_denied_to("drop role")
                .with_detail("Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute."));
        }
        if !is_admin_of_role(&self.data, cur, role) {
            return Err(errors::permission_denied_to("drop role").with_detail(format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may drop this role.",
                target.name
            )));
        }
        // Pinned (predefined / bootstrap) roles are required by the database system.
        if role < FIRST_NORMAL_OID || !self.data.roles.contains_key(&role) {
            return Err(PgError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop role {} because it is required by the database system", target.name),
            ));
        }

        // Shared dependencies: owned objects and ACL mentions, then grants made BY this role.
        let snap = self.snapshot()?;
        let env = self.env()?;
        let deps = crate::depend::role_dependencies(&snap, role)?;
        let mut lines: Vec<String> = Vec::with_capacity(deps.len());
        for dep in &deps {
            let desc = match crate::describe::describe(&env, &dep.object)? {
                Some(d) => d,
                None => continue,
            };
            lines.push(match dep.kind {
                crate::depend::SharedDepKind::Owner => format!("owner of {desc}"),
                crate::depend::SharedDepKind::Acl => format!("privileges for {desc}"),
            });
        }
        // `SHARED_DEPENDENCY_POLICY`: roles named in a policy's TO list.
        for r in snap.user_relations() {
            for p in &r.policies {
                if p.roles.contains(&role) {
                    lines.push(format!("target of policy {} on table {}", p.name, r.name));
                }
            }
        }
        for m in self.data.memberships.values() {
            if m.grantor == role && m.role != role && m.member != role {
                lines.push(format!(
                    "privileges for membership of role {} in role {}",
                    name_of(&self.data, m.member),
                    name_of(&self.data, m.role)
                ));
            }
        }
        if !lines.is_empty() {
            return Err(errors::role_cannot_be_dropped(&target.name, &lines));
        }

        let doomed: Vec<Oid> =
            self.data.memberships.values().filter(|m| m.role == role || m.member == role).map(|m| m.oid).collect();
        for oid in doomed {
            self.remove(ObjKind::RoleMembership, oid)?;
        }
        self.clear_descriptions(ObjClass::Role.classid(), role)?;
        self.remove(ObjKind::Role, role)?;
        self.record_dropped(ObjectAddress::new(ObjClass::Role, role));
        Ok(())
    }

    /// GRANT `spec.role` TO `spec.member` (PG 16+ semantics; rows keyed by (role, member, grantor)):
    /// authorization via `check_role_membership_authorization` (superuser, or admin option on the
    /// role); no self-membership (`role "a" is a member of role "a"` 0LP01); no cycles (0LP01
    /// `errors::circular_membership(role, member)`); granting to a role that already has the
    /// grant from the same grantor updates the options and emits NOTICE `role "m" has already been
    /// granted membership in role "r" by role "g"` (00000? severity NOTICE, SQLSTATE 00000) when
    /// nothing changes. Inserts/updates `RoleMembership{oid, role, member, grantor, admin_option,
    /// inherit_option (default = member's `inherit`), set_option}`. Granting a `pg_` predefined role
    /// is allowed for superusers.
    // CONTRACT
    pub fn grant_role(&mut self, spec: &MembershipSpec) -> Result<()> {
        let cur = self.current_user();
        if attrs(&self.data, spec.role).is_none() {
            return Err(errors::cache_lookup_failed("role", spec.role));
        }
        if attrs(&self.data, spec.member).is_none() {
            return Err(errors::cache_lookup_failed("role", spec.member));
        }
        check_membership_authorization(&self.data, cur, spec.role, true)?;
        // The spec carries plain booleans: ADMIN counts as specified only when true, SET only when
        // false (their non-default values); INHERIT is explicit when `Some`.
        let opts = GrantOpts {
            admin: if spec.admin_option { Some(true) } else { None },
            inherit: spec.inherit_option,
            set: if spec.set_option { None } else { Some(false) },
        };
        let grantor = normalize_grantor(cur, spec.grantor);
        add_role_mems(self, cur, spec.role, &[spec.member], grantor, opts)
    }

    /// REVOKE role FROM member [GRANTED BY grantor] (`admin_only` = REVOKE ADMIN OPTION FOR ...,
    /// which only clears the option). Missing membership -> WARNING 01000 `role "m" has not been
    /// granted membership in role "r" by role "g"` (not an error). CASCADE/RESTRICT dependent
    /// grants (grants made by `member` through its admin option) follow PG `DelRoleMems`
    /// (RESTRICT -> 2BP01 `dependent privileges exist` HINT `Use CASCADE to revoke them too.`).
    // CONTRACT
    pub fn revoke_role(&mut self, spec: &MembershipSpec, admin_option_only: bool, cascade: bool) -> Result<()> {
        let cur = self.current_user();
        if attrs(&self.data, spec.role).is_none() {
            return Err(errors::cache_lookup_failed("role", spec.role));
        }
        check_membership_authorization(&self.data, cur, spec.role, false)?;
        let grantor = check_role_grantor(&self.data, cur, spec.role, normalize_grantor(cur, spec.grantor), false)?;
        let rolename = name_of(&self.data, spec.role);

        let mem: Vec<RoleMembership> = memberships(&self.data).into_iter().filter(|e| e.role == spec.role).collect();
        let mut actions = vec![RevokeAction::Noop; mem.len()];
        let found = plan_single_revoke(&mem, &mut actions, spec.member, grantor, admin_option_only, cascade)?;
        if !found {
            let w = warning_msg(format!(
                "role \"{}\" has not been granted membership in role \"{}\" by role \"{}\"",
                name_of(&self.data, spec.member),
                rolename,
                name_of(&self.data, grantor)
            ));
            self.notice(w);
            return Ok(());
        }

        // Builtin memberships are not persisted and cannot be changed.
        for (i, a) in actions.iter().enumerate() {
            if *a != RevokeAction::Noop && !self.data.memberships.contains_key(&mem[i].oid) {
                return Err(PgError::unimplemented("CAT-10", "revoking a builtin role membership"));
            }
        }
        for (i, a) in actions.iter().enumerate() {
            match a {
                RevokeAction::Noop => {}
                RevokeAction::DeleteGrant => self.remove(ObjKind::RoleMembership, mem[i].oid)?,
                RevokeAction::RemoveAdmin => {
                    let mut m = mem[i].clone();
                    m.admin_option = false;
                    self.put(CatalogObject::RoleMembership(m))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::CatalogEnv;
    use crate::testing;
    use pgarbor_common::MemKv;

    fn code(e: &PgError) -> String {
        e.sqlstate.code().to_string()
    }

    fn mk(kv: &mut MemKv, env: &CatalogEnv, spec: &CreateRoleSpec) -> Result<(Oid, CatalogEnv)> {
        let mut oid = 0;
        let (_, env) = testing::run(kv, env, |t| {
            oid = t.create_role(spec)?;
            Ok(())
        })?;
        Ok((oid, env))
    }

    fn grant_spec(role: Oid, member: Oid) -> MembershipSpec {
        MembershipSpec { role, member, grantor: 0, admin_option: false, inherit_option: None, set_option: true }
    }

    #[test]
    fn password_classification() {
        assert!(is_hashed_password("SCRAM-SHA-256$4096:abc$def:ghi"));
        assert!(is_hashed_password(&format!("md5{}", "a".repeat(32))));
        assert!(!is_hashed_password(&format!("md5{}", "A".repeat(32))));
        assert!(!is_hashed_password("hunter2"));
        assert!(!is_hashed_password("md5abc"));
    }

    #[test]
    fn closure_follows_inherit_edges_only_for_privs() {
        let e = |oid, role, member, inherit| RoleMembership {
            oid,
            role,
            member,
            grantor: 10,
            admin_option: false,
            inherit_option: inherit,
            set_option: true,
        };
        let edges = vec![e(1, 20, 21, true), e(2, 22, 20, false)];
        assert_eq!(roles_closure(&edges, 21, Recurse::Members, 0).0, vec![21, 20, 22]);
        assert_eq!(roles_closure(&edges, 21, Recurse::Privs, 0).0, vec![21, 20]);
    }

    #[test]
    fn create_role_basic_and_errors() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut spec = CreateRoleSpec::new("alice");
        spec.can_login = true;
        let (oid, env) = mk(&mut kv, &env, &spec)?;
        let r = env.snapshot().role(oid).cloned().expect("role");
        assert_eq!(r.name, "alice");
        assert!(r.can_login && r.inherit && !r.superuser);

        let e = mk(&mut kv, &env, &CreateRoleSpec::new("alice")).err().expect("expected an error");
        assert_eq!(code(&e), "42710");
        assert_eq!(e.message, "role \"alice\" already exists");

        let e = mk(&mut kv, &env, &CreateRoleSpec::new("pg_x")).err().expect("expected an error");
        assert_eq!(code(&e), "42939");

        let mut p = CreateRoleSpec::new("bob");
        p.password = Some("plaintext".into());
        let e = mk(&mut kv, &env, &p).err().expect("expected an error");
        assert_eq!(code(&e), "22023");
        assert_eq!(e.message, "password is not hashed");

        p.password = Some(format!("md5{}", "0".repeat(32)));
        assert!(mk(&mut kv, &env, &p).is_ok());
        Ok(())
    }

    #[test]
    fn grant_cycle_detected() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (a, env) = mk(&mut kv, &env, &CreateRoleSpec::new("a"))?;
        let (b, env) = mk(&mut kv, &env, &CreateRoleSpec::new("b"))?;
        let ab = grant_spec(a, b);
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_role(&ab))?;
        let ba = grant_spec(b, a);
        let e = testing::run(&mut kv, &env, |t| t.grant_role(&ba)).err().expect("expected an error");
        assert_eq!(code(&e), "0LP01");
        assert_eq!(e.message, "role \"b\" is a member of role \"a\"");
        let aa = grant_spec(a, a);
        let e = testing::run(&mut kv, &env, |t| t.grant_role(&aa)).err().expect("expected an error");
        assert_eq!(e.message, "role \"a\" is a member of role \"a\"");
        Ok(())
    }

    #[test]
    fn grant_twice_notices_and_revoke_missing_warns() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (a, env) = mk(&mut kv, &env, &CreateRoleSpec::new("a"))?;
        let (b, env) = mk(&mut kv, &env, &CreateRoleSpec::new("b"))?;
        let g = grant_spec(a, b);
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_role(&g))?;
        let (res, env) = testing::run(&mut kv, &env, |t| t.grant_role(&g))?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(
            res.outcome.notices[0].message,
            "role \"b\" has already been granted membership in role \"a\" by role \"postgres\""
        );
        let (res, env) = testing::run(&mut kv, &env, |t| t.revoke_role(&g, false, false))?;
        assert!(res.outcome.notices.is_empty());
        let (res, _env) = testing::run(&mut kv, &env, |t| t.revoke_role(&g, false, false))?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(
            res.outcome.notices[0].message,
            "role \"b\" has not been granted membership in role \"a\" by role \"postgres\""
        );
        Ok(())
    }

    #[test]
    fn drop_role_rules() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (a, env) = mk(&mut kv, &env, &CreateRoleSpec::new("a"))?;
        let (b, env) = mk(&mut kv, &env, &CreateRoleSpec::new("b"))?;
        let g = grant_spec(a, b);
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_role(&g))?;

        let e = testing::run(&mut kv, &env, |t| t.drop_role(BOOTSTRAP_SUPERUSER)).err().expect("expected an error");
        assert_eq!(code(&e), "55006");
        assert_eq!(e.message, "current user cannot be dropped");

        let (res, _env) = testing::run(&mut kv, &env, |t| t.drop_role(a))?;
        assert!(res.snapshot.role(a).is_none());
        assert!(res.snapshot.data().memberships.is_empty());
        Ok(())
    }

    #[test]
    fn rename_and_config() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (a, env) = mk(&mut kv, &env, &CreateRoleSpec::new("a"))?;
        let e = testing::run(&mut kv, &env, |t| t.rename_role(a, "pg_z")).err().expect("expected an error");
        assert_eq!(code(&e), "42939");
        let e = testing::run(&mut kv, &env, |t| t.rename_role(a, "postgres")).err().expect("expected an error");
        assert_eq!(code(&e), "42710");
        let (res, env) = testing::run(&mut kv, &env, |t| t.rename_role(a, "z"))?;
        assert_eq!(res.snapshot.role(a).map(|r| r.name.clone()), Some("z".to_string()));
        let (res, env) = testing::run(&mut kv, &env, |t| t.set_role_config(a, "work_mem", Some("1MB")))?;
        assert_eq!(res.snapshot.role(a).map(|r| r.config.clone()), Some(vec!["work_mem=1MB".to_string()]));
        let (res, _) = testing::run(&mut kv, &env, |t| t.set_role_config(a, "work_mem", None))?;
        assert!(res.snapshot.role(a).is_some_and(|r| r.config.is_empty()));
        Ok(())
    }

    #[test]
    fn bootstrap_superuser_keeps_superuser() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let ch = RoleChanges { superuser: Some(false), ..RoleChanges::default() };
        let e = testing::run(&mut kv, &env, |t| t.alter_role(BOOTSTRAP_SUPERUSER, &ch)).err().expect("expected an error");
        assert_eq!(e.message, "permission denied to alter role");
        Ok(())
    }
}
