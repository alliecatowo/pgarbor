//! CREATE / ALTER / DROP POLICY (amendment pgarbor-catalog-policies-rules). PG reference: `commands/policy.c`.
//! Policies live inside their table's `Relation::policies`; dropping the table drops them.
use crate::errors;
use crate::model::{ObjectAddress, Policy, RelKind, StoredExpr};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState, FIRST_NORMAL_OID};

/// Input of [`CatalogTxn::create_policy`] (the binder resolved names and bound the expressions).
#[derive(Clone, Debug, PartialEq)]
pub struct PolicySpec {
    pub table: Oid,
    pub name: String,
    pub cmd: u8,
    pub permissive: bool,
    /// `0` = PUBLIC.
    pub roles: Vec<Oid>,
    pub using: Option<StoredExpr>,
    pub with_check: Option<StoredExpr>,
}

/// `ALTER POLICY` changes (`None` = unchanged).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PolicyChanges {
    pub roles: Option<Vec<Oid>>,
    pub using: Option<StoredExpr>,
    pub with_check: Option<StoredExpr>,
}

fn duplicate(name: &str, table: &str) -> PgError {
    PgError::new(SqlState::DUPLICATE_OBJECT, format!("policy \"{name}\" for table \"{table}\" already exists"))
}

fn missing(name: &str, table: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("policy \"{name}\" for table \"{table}\" does not exist"))
}

impl<'a> CatalogTxn<'a> {
    /// PG `RangeVarCallbackForPolicy`: a table owned by the current user (superuser bypasses).
    fn policy_table(&mut self, table: Oid) -> Result<crate::model::Relation> {
        let snap = self.snapshot()?;
        let rel = snap.relation(table).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
        if table < FIRST_NORMAL_OID {
            return Err(PgError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{}\" is a system catalog", rel.name),
            ));
        }
        if !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable) {
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", rel.name)));
        }
        self.require_owner(&ObjectAddress::relation(table))?;
        Ok(rel)
    }

    /// CREATE POLICY.
    pub fn create_policy(&mut self, spec: &PolicySpec) -> Result<()> {
        let rel = self.policy_table(spec.table)?;
        if rel.policies.iter().any(|p| p.name == spec.name) {
            return Err(duplicate(&spec.name, &rel.name));
        }
        let oid = self.new_oid()?;
        let policy = Policy {
            oid,
            name: spec.name.clone(),
            cmd: spec.cmd,
            permissive: spec.permissive,
            roles: spec.roles.clone(),
            using: spec.using.clone(),
            with_check: spec.with_check.clone(),
        };
        self.update_relation(spec.table, |r| {
            r.policies.push(policy);
            Ok(())
        })
    }

    /// ALTER POLICY ... [TO roles] [USING (..)] [WITH CHECK (..)].
    pub fn alter_policy(&mut self, table: Oid, name: &str, changes: &PolicyChanges) -> Result<()> {
        let rel = self.policy_table(table)?;
        let Some(existing) = rel.policies.iter().find(|p| p.name == name) else {
            return Err(missing(name, &rel.name));
        };
        if changes.using.is_some() && existing.cmd == b'a' {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "only WITH CHECK expression allowed for INSERT"));
        }
        if changes.with_check.is_some() && matches!(existing.cmd, b'r' | b'd') {
            return Err(PgError::new(SqlState::SYNTAX_ERROR, "only USING expression allowed for SELECT, DELETE"));
        }
        let name = name.to_string();
        let changes = changes.clone();
        self.update_relation(table, move |r| {
            if let Some(p) = r.policies.iter_mut().find(|p| p.name == name) {
                if let Some(roles) = changes.roles {
                    p.roles = roles;
                }
                if let Some(u) = changes.using {
                    p.using = Some(u);
                }
                if let Some(w) = changes.with_check {
                    p.with_check = Some(w);
                }
            }
            Ok(())
        })
    }

    /// ALTER POLICY ... RENAME TO.
    pub fn rename_policy(&mut self, table: Oid, name: &str, new_name: &str) -> Result<()> {
        let rel = self.policy_table(table)?;
        if rel.policies.iter().any(|p| p.name == new_name) {
            return Err(duplicate(new_name, &rel.name));
        }
        if !rel.policies.iter().any(|p| p.name == name) {
            return Err(missing(name, &rel.name));
        }
        let (name, new_name) = (name.to_string(), new_name.to_string());
        self.update_relation(table, move |r| {
            if let Some(p) = r.policies.iter_mut().find(|p| p.name == name) {
                p.name = new_name;
            }
            Ok(())
        })
    }

    /// DROP POLICY [IF EXISTS].
    pub fn drop_policy(&mut self, table: Oid, name: &str, missing_ok: bool) -> Result<()> {
        // PG `RemoveObjects`: the policy is looked up first (any relation kind), then ownership is checked
        // on the relation ("must be owner of relation t").
        let snap = self.snapshot()?;
        let rel = snap.relation(table).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", table))?;
        if !rel.policies.iter().any(|p| p.name == name) {
            if missing_ok {
                self.notice(PgError::notice(format!(
                    "policy \"{name}\" for relation \"{}\" does not exist, skipping",
                    rel.name
                )));
                return Ok(());
            }
            return Err(missing(name, &rel.name));
        }
        if self.require_owner(&ObjectAddress::relation(table)).is_err() {
            return Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be owner of relation {}", rel.name)));
        }
        let name = name.to_string();
        self.update_relation(table, move |r| {
            r.policies.retain(|p| p.name != name);
            Ok(())
        })
    }
}
