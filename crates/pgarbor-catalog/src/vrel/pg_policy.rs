//! `pg_policy`, `pg_policies` and `pg_rules` (amendment pgarbor-catalog-policies-rules): rows from
//! `Relation::policies` and `Relation::rules`. PG reference: `catalog/system_views.sql` (`pg_policies`,
//! `pg_rules`), `utils/adt/ruleutils.c` (`pg_get_ruledef`).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use crate::model::{Policy, Relation, Rule};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::Datum;

fn schema_name(snap: &CatalogSnapshot, oid: Oid) -> Result<String> {
    snap.namespace(oid)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::corruption(format!("namespace {oid} referenced by a catalog object does not exist")))
}

/// `pg_policies.cmd` text for `polcmd`.
pub fn policy_cmd_name(cmd: u8) -> &'static str {
    match cmd {
        b'r' => "SELECT",
        b'a' => "INSERT",
        b'w' => "UPDATE",
        b'd' => "DELETE",
        _ => "ALL",
    }
}

/// `pg_policies.roles`: `{public}` for PUBLIC, else the role names sorted by name.
fn policy_role_names(snap: &CatalogSnapshot, p: &Policy) -> Vec<String> {
    if p.roles.is_empty() || p.roles.contains(&0) {
        return vec!["public".to_string()];
    }
    let mut names: Vec<String> = p.roles.iter().filter_map(|r| snap.role_name(*r).map(|s| s.to_string())).collect();
    names.sort();
    names.dedup();
    names
}

/// Producer for `pg_policy`.
#[derive(Clone, Copy, Debug)]
pub struct PgPolicy;

impl VirtualRelation for PgPolicy {
    fn oid(&self) -> Oid {
        relid::PG_POLICY
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out = Vec::new();
        for rel in snap.user_relations() {
            for p in &rel.policies {
                let mut b = RowBuilder::new(schema);
                b.set("oid", d::oid(p.oid))
                    .set("polname", d::name(&p.name))
                    .set("polrelid", d::oid(rel.oid))
                    .set("polcmd", d::char_(p.cmd))
                    .set("polpermissive", d::bool_(p.permissive))
                    .set("polroles", d::oid_array(&p.roles))
                    .set("polqual", d::opt_text(p.using.as_ref().map(|e| e.sql.as_str())))
                    .set("polwithcheck", d::opt_text(p.with_check.as_ref().map(|e| e.sql.as_str())));
                out.push(b.finish()?);
            }
        }
        Ok(out)
    }
}

/// Producer for `pg_policies`.
#[derive(Clone, Copy, Debug)]
pub struct PgPolicies;

impl VirtualRelation for PgPolicies {
    fn oid(&self) -> Oid {
        relid::PG_POLICIES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out = Vec::new();
        for rel in snap.user_relations() {
            if rel.policies.is_empty() {
                continue;
            }
            let schemaname = schema_name(snap, rel.namespace)?;
            for p in &rel.policies {
                let mut b = RowBuilder::new(schema);
                b.set("schemaname", d::name(&schemaname))
                    .set("tablename", d::name(&rel.name))
                    .set("policyname", d::name(&p.name))
                    .set("permissive", d::text(if p.permissive { "PERMISSIVE" } else { "RESTRICTIVE" }))
                    .set("roles", d::name_array(&policy_role_names(snap, p)))
                    .set("cmd", d::text(policy_cmd_name(p.cmd)))
                    .set("qual", d::opt_text(p.using.as_ref().map(|e| e.sql.as_str())))
                    .set("with_check", d::opt_text(p.with_check.as_ref().map(|e| e.sql.as_str())));
                out.push(b.finish()?);
            }
        }
        Ok(out)
    }
}

/// `ev_type` keyword.
pub fn rule_event_name(event: u8) -> &'static str {
    match event {
        b'1' => "SELECT",
        b'2' => "UPDATE",
        b'3' => "INSERT",
        _ => "DELETE",
    }
}

/// `pg_get_ruledef(oid, pretty)` text of a user rule (`ruleutils.c` `make_ruledef`): the table name is
/// schema-qualified when the schema is not visible in the search path.
pub fn rule_definition(ctx_env: &crate::env::CatalogEnv, rel: &Relation, rule: &Rule) -> Result<String> {
    let snap = ctx_env.snapshot();
    use pgarbor_types::text::format::quote_identifier;
    let table = if ctx_env.search_path().contains(&rel.namespace) {
        quote_identifier(&rel.name)?
    } else {
        format!("{}.{}", quote_identifier(&schema_name(snap, rel.namespace)?)?, quote_identifier(&rel.name)?)
    };
    let mut s = format!(
        "CREATE RULE {} AS\n    ON {} TO {}",
        quote_identifier(&rule.name)?,
        rule_event_name(rule.event),
        table
    );
    if let Some(q) = &rule.qual {
        s.push_str(&format!("\n   WHERE {q}"));
    }
    s.push_str(if rule.instead { " DO INSTEAD" } else { " DO" });
    match rule.actions.as_slice() {
        [] => s.push_str(" NOTHING;"),
        [one] => {
            s.push(' ');
            s.push_str(one);
            s.push(';');
        }
        many => {
            s.push_str(" (");
            for (i, a) in many.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                s.push(' ');
                s.push_str(a);
            }
            s.push_str(";\n);");
        }
    }
    Ok(s)
}

/// Producer for `pg_rules` (user rules only; view `_RETURN` rules are excluded like PG's view does).
#[derive(Clone, Copy, Debug)]
pub struct PgRules;

impl VirtualRelation for PgRules {
    fn oid(&self) -> Oid {
        relid::PG_RULES
    }
    fn scan(&self, ctx: &ScanContext, _req: &ScanRequest) -> Result<Vec<Row>> {
        let snap = ctx.env.snapshot();
        let schema = self.schema()?;
        let mut out = Vec::new();
        for rel in snap.user_relations() {
            if rel.rules.is_empty() {
                continue;
            }
            let schemaname = schema_name(snap, rel.namespace)?;
            for r in &rel.rules {
                let def = rule_definition(ctx.env, rel, r)?;
                let mut b = RowBuilder::new(schema);
                b.set("schemaname", d::name(&schemaname))
                    .set("tablename", d::name(&rel.name))
                    .set("rulename", d::name(&r.name))
                    .set("definition", Datum::Text(def));
                out.push(b.finish()?);
            }
        }
        Ok(out)
    }
}

pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgPolicy, &PgPolicies, &PgRules];
