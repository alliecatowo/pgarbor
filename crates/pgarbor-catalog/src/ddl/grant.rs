//! GRANT / REVOKE privileges on objects. Lease CAT-21. PG sources: `catalog/aclchk.c`
//! (`ExecuteGrantStmt`, `ExecGrant_Relation`, `ExecGrant_Attribute`, `ExecGrant_common`,
//! `objectNamesToOids`, `objectsInSchemaToOids`, `string_to_privilege`, `privilege_to_string`),
//! `utils/adt/acl.c` (`aclupdate`).
//!
//! `CatalogTxn::grant_object_privileges(spec, revoke)` expands the target to its object list
//! (`GrantTarget::AllTablesInSchema(ns)` -> tables, views, matviews, foreign tables of the schema,
//! `AllSequencesInSchema`, `AllFunctionsInSchema`), then for each object:
//! 1. kind check: the privilege bits must be applicable to the kind (`AclObject::all_privileges`);
//!    otherwise 0LP01 `invalid privilege type SELECT for schema` (`errors`: build with
//!    `PgError::new(SqlState(*b"0LP01"), format!("invalid privilege type {PRIV} for {kind_word}"))`,
//!    PRIV upper-case word `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCES`,
//!    `TRIGGER`, `MAINTAIN`, `EXECUTE`, `USAGE`, `CREATE`, `TEMP`, `CONNECT`, `SET`,
//!    `ALTER SYSTEM`). `ALL` expands per kind.
//! 2. sequences accept table-style `GRANT SELECT ON TABLE seq` (privilege set restricted to
//!    `ALL_SEQUENCE`: PG treats USAGE/SELECT/UPDATE; others -> 0LP01 `invalid privilege type INSERT for
//!    sequence`).
//! 3. the ACL change is computed by `acl::grant_privileges`/`revoke_privileges` with
//!    `AclRequest{caller = current_user, grantor = spec.grantor, ...}`; its WARNINGs go to the
//!    outcome notices; the new ACL is stored in the object (`Relation::acl`, `Sequence::acl`,
//!    `TypeObj::acl`, `Function::acl`, `Namespace::acl`, `Database::acl`) or, for column grants
//!    (`spec.columns` non-empty, tables/views only), in `Column::acl` per column. Column-level
//!    REVOKE of table-level privileges follows PG (`ExecGrant_Attribute`; `REVOKE SELECT ON t` also
//!    clears column-level SELECT grants).
//! 4. a GRANT to a non-existent role is the binder's 42704; granting to `0` (PUBLIC) is allowed.
//! Ownership is not required: any role holding the privilege WITH GRANT OPTION may grant it
//! (the grantor-selection step inside `acl::grant_privileges` (PG `select_best_grantor`)); if neither -> 42501 `permission denied for table t` (or
//! `must be owner of`, PG `aclcheck_error` rules: owner-only kinds report `must be owner of`).
//! pg_catalog objects (persisted ACL overrides of builtin relations) are refused: 42501
//! `permission denied: "pg_class" is a system catalog`.
//! `GRANT role TO role` is `role::grant_role`.
//!
//! Implementation notes (this file):
//! - All targets are resolved BEFORE the first write, so a failing target leaves nothing behind
//!   (the caller rolls the statement back anyway).
//! - The catalog snapshot is taken once per statement; grants never change roles or memberships,
//!   so the role graph used inside `acl` stays valid for the whole statement.
//! - Relation names in messages are bare (PG `aclcheck_error` uses `relname`), as are column names.
//! - A relation-level REVOKE that carries table privileges also revokes the matching column
//!   privileges of every column that has a column ACL (PG implicit column revoke). The warnings of
//!   that implicit step are not emitted (PG's implicit step is silent for users).
use super::spec::*;
use crate::acl::{self, AclChange, AclObject, AclRequest};
use crate::errors;
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PG `privilege_to_string` names, indexed by bit position (`acl_mode` bit i).
const PRIV_NAMES: [&str; 15] = [
    "INSERT",
    "SELECT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "EXECUTE",
    "USAGE",
    "CREATE",
    "TEMP",
    "CONNECT",
    "SET",
    "ALTER SYSTEM",
    "MAINTAIN",
];

/// Name of the lowest privilege bit set in `bits` (PG reports the privilege it rejected).
fn priv_name(bits: u32) -> &'static str {
    let idx = bits.trailing_zeros() as usize;
    PRIV_NAMES.get(idx).copied().unwrap_or("UNKNOWN")
}

/// 0LP01 `invalid privilege type <PRIV> for <word>`; `bits` names the first rejected privilege.
fn invalid_privilege(bits: u32, word: &str) -> PgError {
    PgError::new(SqlState(*b"0LP01"), format!("invalid privilege type {} for {word}", priv_name(bits)))
}

/// The privilege bits requested for an object kind: `ALL` expands to `all`, explicit bits must be
/// within `allowed` (PG `ExecuteGrantStmt` kind check).
fn requested_bits(privs: PrivilegeSet, all: u32, allowed: u32, word: &str) -> Result<u32> {
    let bits = match privs {
        PrivilegeSet::All => all,
        PrivilegeSet::Bits(b) => b,
    };
    let bad = bits & !allowed;
    if bad != 0 {
        return Err(invalid_privilege(bad, word));
    }
    Ok(bits)
}

/// Privilege bits for a relation target (PG `ExecuteGrantStmt` + `ExecGrant_Relation`): table
/// syntax accepts the table and sequence privilege sets; a non-sequence rejects USAGE with
/// `for table`.
fn relation_bits(privs: PrivilegeSet) -> Result<u32> {
    let all_allowed = acl_mode::ALL_RELATION | acl_mode::ALL_SEQUENCE;
    let bits = match privs {
        PrivilegeSet::All => acl_mode::ALL_RELATION,
        PrivilegeSet::Bits(b) => b,
    };
    let outside = bits & !all_allowed;
    if outside != 0 {
        return Err(invalid_privilege(outside, "relation"));
    }
    if bits & !acl_mode::ALL_RELATION != 0 {
        return Err(invalid_privilege(acl_mode::USAGE, "table"));
    }
    Ok(bits)
}

/// 42809 `"name" is an index` / `"name" is a composite type` (PG `ExecGrant_Relation`).
fn not_grantable(name: &str, what: &str) -> PgError {
    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is {what}"))
}

/// 42703 for a column number that names no visible column of the relation.
fn missing_column(attnum: i16, rel: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_COLUMN, format!("column number {attnum} of relation \"{rel}\" does not exist"))
}

/// A target object resolved for a privilege change (owned copy of the stored object).
enum Found {
    Relation(Relation),
    Sequence(Sequence),
    Type(TypeObj),
    Namespace(Namespace),
    Function(Function),
    Database(Database),
}

impl<'a> CatalogTxn<'a> {
    /// GRANT (`revoke = false`) or REVOKE (`revoke = true`) privileges (module docs).
    // CONTRACT
    pub fn grant_object_privileges(&mut self, spec: &GrantSpec, revoke: bool) -> Result<()> {
        let snap = self.snapshot()?;
        let targets = self.grant_targets(&snap, &spec.target)?;
        for found in targets {
            self.grant_one(&snap, found, spec, revoke)?;
        }
        Ok(())
    }

    /// Expand the target into concrete objects (no writes).
    fn grant_targets(&self, snap: &CatalogSnapshot, target: &GrantTarget) -> Result<Vec<Found>> {
        let data = self.data();
        match target {
            GrantTarget::Object(addr) => Ok(vec![self.grant_resolve(snap, addr)?]),
            GrantTarget::AllTablesInSchema(ns) => {
                self.grant_check_schema(snap, *ns)?;
                Ok(data
                    .relations
                    .values()
                    .filter(|r| {
                        r.namespace == *ns
                            && matches!(
                                r.kind,
                                RelKind::Table
                                    | RelKind::View
                                    | RelKind::MatView
                                    | RelKind::PartitionedTable
                                    | RelKind::ForeignTable
                            )
                    })
                    .map(|r| Found::Relation(r.clone()))
                    .collect())
            }
            GrantTarget::AllSequencesInSchema(ns) => {
                self.grant_check_schema(snap, *ns)?;
                Ok(data
                    .sequences
                    .values()
                    .filter(|s| s.namespace == *ns)
                    .map(|s| Found::Sequence(s.clone()))
                    .collect())
            }
            GrantTarget::AllFunctionsInSchema(ns) => {
                self.grant_check_schema(snap, *ns)?;
                // PG: every routine of the schema except procedures (aggregates and window functions
                // included).
                Ok(data
                    .functions
                    .values()
                    .filter(|f| f.namespace == *ns && !matches!(f.kind, FuncKind::Procedure))
                    .map(|f| Found::Function(f.clone()))
                    .collect())
            }
        }
    }

    /// A schema-wide target must name a user schema: builtin schemas hold system objects whose
    /// ACLs cannot be overridden.
    fn grant_check_schema(&self, snap: &CatalogSnapshot, ns: Oid) -> Result<()> {
        if self.data().namespaces.contains_key(&ns) {
            return Ok(());
        }
        match snap.namespace(ns) {
            Some(n) => Err(errors::system_catalog_permission(&n.name)),
            None => Err(errors::cache_lookup_failed("schema", ns)),
        }
    }

    /// Resolve one object address to a stored, user-owned object.
    fn grant_resolve(&self, snap: &CatalogSnapshot, addr: &ObjectAddress) -> Result<Found> {
        let data = self.data();
        let oid = addr.oid;
        match addr.class {
            ObjClass::Relation => {
                if let Some(r) = data.relations.get(&oid) {
                    return match r.kind {
                        RelKind::CompositeType => Err(not_grantable(&r.name, "a composite type")),
                        RelKind::Index | RelKind::PartitionedIndex => Err(not_grantable(&r.name, "an index")),
                        _ => Ok(Found::Relation(r.clone())),
                    };
                }
                if let Some(s) = data.sequences.get(&oid) {
                    return Ok(Found::Sequence(s.clone()));
                }
                if let Some(i) = data.indexes.get(&oid) {
                    return Err(not_grantable(&i.name, "an index"));
                }
                if let Some(r) = snap.relation(oid) {
                    return Err(errors::system_catalog_permission(&r.name));
                }
                if let Some(s) = snap.sequence(oid) {
                    return Err(errors::system_catalog_permission(&s.name));
                }
                if let Some(i) = snap.index(oid) {
                    return Err(not_grantable(&i.name, "an index"));
                }
                Err(errors::cache_lookup_failed("relation", oid))
            }
            ObjClass::Type => {
                if let Some(t) = data.types.get(&oid) {
                    return Ok(Found::Type(t.clone()));
                }
                if data.types.values().any(|t| t.array_type == oid) {
                    let mut err = PgError::new(SqlState(*b"0LP01"), "cannot set privileges of array types");
                    err.hint = Some("Set the privileges of the element type instead.".to_string());
                    return Err(err);
                }
                if snap.type_obj(oid).is_some() {
                    return Err(errors::system_catalog_permission("pg_type"));
                }
                Err(errors::cache_lookup_failed("type", oid))
            }
            ObjClass::Namespace => {
                if let Some(n) = data.namespaces.get(&oid) {
                    return Ok(Found::Namespace(n.clone()));
                }
                if snap.namespace(oid).is_some() {
                    return Err(errors::system_catalog_permission("pg_namespace"));
                }
                Err(errors::cache_lookup_failed("schema", oid))
            }
            ObjClass::Proc => {
                if let Some(f) = data.functions.get(&oid) {
                    return Ok(Found::Function(f.clone()));
                }
                if snap.function(oid).is_some() {
                    return Err(errors::system_catalog_permission("pg_proc"));
                }
                // A built-in routine has a pg_proc row PGArbor does not persist, so its ACL cannot be
                // changed yet. That is unsupported, not a missing object.
                if pgarbor_types::catalog_data::builtin_proc(oid).is_some() {
                    return Err(PgError::unimplemented("CAT-21", "GRANT/REVOKE on built-in functions"));
                }
                Err(errors::cache_lookup_failed("function", oid))
            }
            ObjClass::Database => {
                if let Some(d) = data.databases.get(&oid) {
                    return Ok(Found::Database(d.clone()));
                }
                if snap.database(oid).is_some() {
                    return Err(errors::system_catalog_permission("pg_database"));
                }
                Err(errors::cache_lookup_failed("database", oid))
            }
            // Language and tablespace ACLs need storage that the v1 catalog data does not have yet.
            ObjClass::Language => Err(PgError::unimplemented("CAT-21", "GRANT/REVOKE on language objects")),
            ObjClass::Tablespace => Err(PgError::unimplemented("CAT-21", "GRANT/REVOKE on tablespace objects")),
            // Not GRANT targets (PG `ExecGrantStmt_oids` / `objectNamesToOids` reject them by grammar).
            other => Err(PgError::internal(format!("unrecognized GrantStmt.objtype for {other:?}"))),
        }
    }

    /// Apply the grant or revoke to one resolved object and store the new ACL(s).
    fn grant_one(&mut self, snap: &CatalogSnapshot, found: Found, spec: &GrantSpec, revoke: bool) -> Result<()> {
        if !spec.columns.is_empty() {
            match &found {
                Found::Relation(_) => {}
                Found::Sequence(_) => {
                    return Err(PgError::unimplemented("CAT-21", "column privileges on sequences"));
                }
                _ => {
                    return Err(PgError::new(
                        SqlState(*b"0LP01"),
                        "column privileges are only valid for relations",
                    ));
                }
            }
        }
        match found {
            Found::Relation(rel) => self.grant_relation(snap, rel, spec, revoke),
            Found::Sequence(seq) => {
                let bits = requested_bits(spec.privileges, acl_mode::ALL_SEQUENCE, acl_mode::ALL_SEQUENCE, "sequence")?;
                let acl = self.grant_plain(
                    snap,
                    AclObject::Sequence,
                    &seq.name,
                    seq.owner,
                    seq.acl.as_ref(),
                    bits,
                    spec,
                    revoke,
                )?;
                self.update_sequence(seq.oid, |s| {
                    s.acl = acl;
                    Ok(())
                })
            }
            Found::Type(t) => {
                let bits = requested_bits(spec.privileges, acl_mode::ALL_TYPE, acl_mode::ALL_TYPE, "type")?;
                let acl = self.grant_plain(
                    snap,
                    AclObject::Type,
                    &t.name,
                    t.owner,
                    t.acl.as_ref(),
                    bits,
                    spec,
                    revoke,
                )?;
                self.update_type(t.oid, |x| {
                    x.acl = acl;
                    Ok(())
                })
            }
            Found::Namespace(n) => {
                let bits = requested_bits(spec.privileges, acl_mode::ALL_SCHEMA, acl_mode::ALL_SCHEMA, "schema")?;
                let acl = self.grant_plain(
                    snap,
                    AclObject::Schema,
                    &n.name,
                    n.owner,
                    n.acl.as_ref(),
                    bits,
                    spec,
                    revoke,
                )?;
                self.update_namespace(n.oid, |x| {
                    x.acl = acl;
                    Ok(())
                })
            }
            Found::Function(f) => {
                let word = if matches!(f.kind, FuncKind::Procedure) { "procedure" } else { "function" };
                let bits = requested_bits(spec.privileges, acl_mode::ALL_FUNCTION, acl_mode::ALL_FUNCTION, word)?;
                let acl = self.grant_plain(
                    snap,
                    AclObject::Function,
                    &f.name,
                    f.owner,
                    f.acl.as_ref(),
                    bits,
                    spec,
                    revoke,
                )?;
                self.update_function(f.oid, |x| {
                    x.acl = acl;
                    Ok(())
                })
            }
            Found::Database(d) => {
                let bits =
                    requested_bits(spec.privileges, acl_mode::ALL_DATABASE, acl_mode::ALL_DATABASE, "database")?;
                let acl = self.grant_plain(
                    snap,
                    AclObject::Database,
                    &d.name,
                    d.owner,
                    d.acl.as_ref(),
                    bits,
                    spec,
                    revoke,
                )?;
                self.update_database(d.oid, |x| {
                    x.acl = acl;
                    Ok(())
                })
            }
        }
    }

    /// Relation (table, view, matview, ...): table-level privileges or column privileges.
    fn grant_relation(&mut self, snap: &CatalogSnapshot, rel: Relation, spec: &GrantSpec, revoke: bool) -> Result<()> {
        if !spec.columns.is_empty() {
            return self.grant_relation_columns(snap, rel, spec, revoke);
        }
        let bits = relation_bits(spec.privileges)?;
        let change = self.acl_change(snap, AclObject::Table, &rel.name, rel.owner, rel.acl.as_ref(), bits, spec, revoke)?;
        let table_acl = self.take_change(change);
        let mut col_acls: Vec<(i16, Option<Acl>)> = Vec::new();
        if revoke && (bits & acl_mode::ALL_COLUMN) != 0 {
            for col in rel.columns.iter().filter(|c| c.is_visible() && c.attnum > 0 && c.acl.is_some()) {
                let ch = self.acl_change(
                    snap,
                    AclObject::Column,
                    &col.name,
                    rel.owner,
                    col.acl.as_ref(),
                    bits & acl_mode::ALL_COLUMN,
                    spec,
                    true,
                )?;
                col_acls.push((col.attnum, ch.acl));
            }
        }
        self.update_relation(rel.oid, |r| {
            r.acl = table_acl;
            for (attnum, acl) in col_acls {
                if let Some(c) = r.columns.iter_mut().find(|c| c.attnum == attnum) {
                    c.acl = acl;
                }
            }
            Ok(())
        })
    }

    /// `GRANT priv (cols) ON t` / `REVOKE priv (cols) ON t`: column ACLs only; the table ACL is
    /// untouched.
    fn grant_relation_columns(
        &mut self,
        snap: &CatalogSnapshot,
        rel: Relation,
        spec: &GrantSpec,
        revoke: bool,
    ) -> Result<()> {
        let bits = requested_bits(spec.privileges, acl_mode::ALL_COLUMN, acl_mode::ALL_COLUMN, "column")?;
        let mut attnums = spec.columns.clone();
        attnums.sort_unstable();
        attnums.dedup();
        let mut col_acls: Vec<(i16, Option<Acl>)> = Vec::with_capacity(attnums.len());
        for attnum in attnums {
            if attnum == 0 {
                // PG `expand_col_privileges`: the binder never yields attnum 0.
                return Err(PgError::internal("column number out of range"));
            }
            if attnum < 0 {
                // System-column ACLs need per-attribute storage for pg_attribute system rows, which
                // the v1 relation model does not carry.
                return Err(PgError::unimplemented("CAT-21", "privileges on system columns"));
            }
            // Dropped columns are invisible to name lookup, so an attnum naming one is unknown here.
            let col = match rel.columns.iter().find(|c| c.attnum == attnum) {
                Some(c) if c.is_visible() => c,
                _ => return Err(missing_column(attnum, &rel.name)),
            };
            let change = self.acl_change(
                snap,
                AclObject::Column,
                &col.name,
                rel.owner,
                col.acl.as_ref(),
                bits,
                spec,
                revoke,
            )?;
            let acl = self.take_change(change);
            col_acls.push((attnum, acl));
        }
        self.update_relation(rel.oid, |r| {
            for (attnum, acl) in col_acls {
                if let Some(c) = r.columns.iter_mut().find(|c| c.attnum == attnum) {
                    c.acl = acl;
                }
            }
            Ok(())
        })
    }

    /// Non-relation objects: one ACL change, returning the new stored ACL.
    #[allow(clippy::too_many_arguments)]
    fn grant_plain(
        &mut self,
        snap: &CatalogSnapshot,
        kind: AclObject,
        name: &str,
        owner: Oid,
        current: Option<&Acl>,
        privileges: u32,
        spec: &GrantSpec,
        revoke: bool,
    ) -> Result<Option<Acl>> {
        let change = self.acl_change(snap, kind, name, owner, current, privileges, spec, revoke)?;
        Ok(self.take_change(change))
    }

    /// Run `acl::grant_privileges` / `acl::revoke_privileges` for one object.
    #[allow(clippy::too_many_arguments)]
    fn acl_change(
        &self,
        snap: &CatalogSnapshot,
        kind: AclObject,
        name: &str,
        owner: Oid,
        current: Option<&Acl>,
        privileges: u32,
        spec: &GrantSpec,
        revoke: bool,
    ) -> Result<AclChange> {
        let req = AclRequest {
            snapshot: snap,
            kind,
            object_name: name,
            owner,
            current,
            caller: self.current_user(),
            grantor: spec.grantor,
            privileges,
            grantees: &spec.grantees,
            grant_option: spec.grant_option,
            cascade: spec.cascade,
        };
        if revoke {
            acl::revoke_privileges(&req)
        } else {
            acl::grant_privileges(&req)
        }
    }

    /// Queue the WARNINGs of an ACL change as notices and return the new ACL.
    fn take_change(&mut self, change: AclChange) -> Option<Acl> {
        let AclChange { acl, warnings } = change;
        for w in warnings {
            self.notice(w);
        }
        acl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec};
    use crate::testing;
    use crate::env::CatalogEnv;
    use crate::search_path::ResolvedSearchPath;
    use pgarbor_types::TypeId;

    const POSTGRES: Oid = 10;
    const PUBLIC: Oid = 0;

    fn table_spec(name: &str) -> CreateTableSpec {
        CreateTableSpec {
            namespace: 2200,
            name: name.to_string(),
            owner: POSTGRES,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::of(23))],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        }
    }

    fn grant_spec(target: GrantTarget, privileges: PrivilegeSet, grantees: Vec<Oid>, columns: Vec<i16>) -> GrantSpec {
        GrantSpec {
            target,
            privileges,
            grantees,
            grant_option: false,
            grantor: POSTGRES,
            columns,
            cascade: false,
        }
    }

    fn table_oid(env: &crate::env::CatalogEnv, name: &str) -> Result<Oid> {
        env.snapshot()
            .data()
            .relations
            .values()
            .find(|r| r.name == name)
            .map(|r| r.oid)
            .ok_or_else(|| PgError::internal(format!("no relation {name}")))
    }

    fn stored_acl(env: &crate::env::CatalogEnv, oid: Oid) -> Result<Option<Acl>> {
        env.snapshot()
            .relation(oid)
            .map(|r| r.acl.clone())
            .ok_or_else(|| PgError::internal("relation missing"))
    }

    fn column_acl(env: &CatalogEnv, oid: Oid, attnum: i16) -> Result<Option<Acl>> {
        env.snapshot()
            .relation(oid)
            .and_then(|r| r.columns.iter().find(|c| c.attnum == attnum))
            .map(|c| c.acl.clone())
            .ok_or_else(|| PgError::internal("column missing"))
    }

    fn expect_err<T>(res: Result<T>) -> Result<PgError> {
        match res {
            Ok(_) => Err(PgError::internal("expected an error")),
            Err(e) => Ok(e),
        }
    }

    #[test]
    fn privilege_names_follow_pg_bit_order() {
        assert_eq!(priv_name(acl_mode::SELECT), "SELECT");
        assert_eq!(priv_name(acl_mode::USAGE | acl_mode::CREATE), "USAGE");
        assert_eq!(priv_name(acl_mode::CREATE_TEMP), "TEMP");
        assert_eq!(priv_name(acl_mode::ALTER_SYSTEM), "ALTER SYSTEM");
        assert_eq!(priv_name(acl_mode::MAINTAIN), "MAINTAIN");
    }

    #[test]
    fn relation_bits_follow_pg_kind_checks() -> Result<()> {
        assert_eq!(relation_bits(PrivilegeSet::All)?, acl_mode::ALL_RELATION);
        assert_eq!(relation_bits(PrivilegeSet::Bits(acl_mode::SELECT | acl_mode::UPDATE))?, acl_mode::SELECT | acl_mode::UPDATE);
        let err = expect_err(relation_bits(PrivilegeSet::Bits(acl_mode::USAGE)))?;
        assert_eq!(err.sqlstate.0, *b"0LP01");
        assert_eq!(err.message, "invalid privilege type USAGE for table");
        let err = expect_err(relation_bits(PrivilegeSet::Bits(acl_mode::CREATE)))?;
        assert_eq!(err.message, "invalid privilege type CREATE for relation");
        Ok(())
    }

    #[test]
    fn sequence_rejects_insert_with_0lp01() -> Result<()> {
        let err = expect_err(requested_bits(
            PrivilegeSet::Bits(acl_mode::INSERT),
            acl_mode::ALL_SEQUENCE,
            acl_mode::ALL_SEQUENCE,
            "sequence",
        ))?;
        assert_eq!(err.sqlstate.0, *b"0LP01");
        assert_eq!(err.message, "invalid privilege type INSERT for sequence");
        Ok(())
    }

    #[test]
    fn grant_by_non_owner_without_grant_option_is_42501() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = table_oid(&env, "t")?;
        // Role 4242 is not in the catalog, holds no privileges and is not the owner.
        let stranger = CatalogEnv::with_resolved(env.snapshot().clone(), ResolvedSearchPath::builtin_default(), 4242);
        let mut spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            Vec::new(),
        );
        // No GRANTED BY: the grantor is the current user (PG `ExecuteGrantStmt`, only a named
        // grantor is checked against GetUserId()). A grantor of POSTGRES here would trip the
        // "grantor must be current user" check (0LP01) before the privilege check (42501).
        spec.grantor = 4242;
        let err = expect_err(testing::run(&mut kv, &stranger, |t| t.grant_object_privileges(&spec, false)))?;
        assert_eq!(err.sqlstate.0, *b"42501");
        assert_eq!(err.message, "permission denied for table t");
        Ok(())
    }

    #[test]
    fn revoking_table_privilege_clears_column_grants() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = table_oid(&env, "t")?;
        let col_spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            vec![1],
        );
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&col_spec, false))?;
        assert!(column_acl(&env, oid, 1)?.is_some());
        let tbl_spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            Vec::new(),
        );
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&tbl_spec, true))?;
        assert_eq!(column_acl(&env, oid, 1)?, None);
        Ok(())
    }

    #[test]
    fn grant_select_to_public_then_revoke_to_default() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = table_oid(&env, "t")?;
        let spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            Vec::new(),
        );
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&spec, false))?;
        // Acceptance: the default ACL plus `=r/postgres`, PUBLIC item appended after the owner item.
        let mut expected = acl::acldefault(AclObject::Table, POSTGRES)?;
        expected.push(AclItem { grantee: PUBLIC, grantor: POSTGRES, privileges: acl_mode::SELECT, grant_options: 0 });
        assert_eq!(stored_acl(&env, oid)?, Some(expected));

        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&spec, true))?;
        assert_eq!(stored_acl(&env, oid)?, None);
        Ok(())
    }

    #[test]
    fn usage_on_table_is_0lp01() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = table_oid(&env, "t")?;
        let spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::USAGE),
            vec![PUBLIC],
            Vec::new(),
        );
        let err = match testing::run(&mut kv, &env, |t| t.grant_object_privileges(&spec, false)) {
            Ok(_) => return Err(PgError::internal("GRANT USAGE ON table must fail")),
            Err(e) => e,
        };
        assert_eq!(err.sqlstate.0, *b"0LP01");
        assert_eq!(err.message, "invalid privilege type USAGE for table");
        Ok(())
    }

    #[test]
    fn all_tables_in_schema_covers_user_tables() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t1")).map(|_| ()))?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t2")).map(|_| ()))?;
        let spec = grant_spec(
            GrantTarget::AllTablesInSchema(2200),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            Vec::new(),
        );
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&spec, false))?;
        for name in ["t1", "t2"] {
            let oid = table_oid(&env, name)?;
            assert!(stored_acl(&env, oid)?.is_some(), "{name} should carry the grant");
        }
        Ok(())
    }

    #[test]
    fn column_grant_leaves_table_acl_alone() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table_spec("t")).map(|_| ()))?;
        let oid = table_oid(&env, "t")?;
        let spec = grant_spec(
            GrantTarget::Object(ObjectAddress::relation(oid)),
            PrivilegeSet::Bits(acl_mode::SELECT),
            vec![PUBLIC],
            vec![1],
        );
        let (_, env) = testing::run(&mut kv, &env, |t| t.grant_object_privileges(&spec, false))?;
        assert_eq!(stored_acl(&env, oid)?, None);
        let col = env
            .snapshot()
            .relation(oid)
            .and_then(|r| r.columns.iter().find(|c| c.attnum == 1).and_then(|c| c.acl.clone()))
            .ok_or_else(|| PgError::internal("column acl missing"))?;
        assert!(col.iter().any(|i| i.grantee == PUBLIC && i.privileges == acl_mode::SELECT));
        Ok(())
    }
}
