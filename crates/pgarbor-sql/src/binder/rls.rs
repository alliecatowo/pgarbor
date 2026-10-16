//! Row-level security (amendment pgarbor-sql-rewrite-rls-merge). PG references: `rewrite/rowsecurity.c`
//! (`get_row_security_policies`, `add_security_quals`, `add_with_check_options`), `utils/misc/rls.c`
//! (`check_enable_rls`).
//!
//! Policies are applied at bind time, for the current user of the statement's `CatalogEnv`:
//! * a table read in FROM is replaced by `(SELECT <all columns> FROM t WHERE <security quals>)`
//!   ([`wrap_select`]), so the policy quals run below every user qual;
//! * UPDATE/DELETE targets get their command's USING quals (and the SELECT ones when the statement
//!   reads the table) in front of the WHERE clause ([`target_quals`]);
//! * INSERT/UPDATE targets get `WITH CHECK` options ([`target_wcos`]) enforced by the executor.
use pgarbor_catalog::acl::RoleGraph;
use pgarbor_catalog::model::{Policy, RelKind};
use pgarbor_catalog::Relation;
use pgarbor_common::{PgError, Result, SqlState, FIRST_NORMAL_OID};
use pgarbor_types::TypeId;

use super::context::{Binder, ExprCtx};
use super::stored::bind_stored_expr;
use crate::diag::ExprClause;
use crate::ir::{
    AccessNeeded, BoolExpr, BoolOp, BoundExpr, BoundQuery, BoundSelect, ColumnOrigin, DistinctSpec, ExprKind,
    FromItem, GroupBy, OutputColumn, QueryBody, RangeEntry, RangeKind, TargetEntry, TargetWco, WcoKind,
};

/// The statement command a policy set is selected for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RlsCmd {
    Select,
    Insert,
    Update,
    Delete,
}

impl RlsCmd {
    fn matches(self, polcmd: u8) -> bool {
        polcmd == b'*'
            || matches!((self, polcmd), (RlsCmd::Select, b'r') | (RlsCmd::Insert, b'a') | (RlsCmd::Update, b'w') | (RlsCmd::Delete, b'd'))
    }
}

/// PG `check_enable_rls`: true when policies apply to `rel` for the current user. With
/// `row_security = off` a relation that would be affected raises 42501.
pub fn rls_active(b: &Binder, rel: &Relation) -> Result<bool> {
    if rel.oid < FIRST_NORMAL_OID || !rel.row_security || !matches!(rel.kind, RelKind::Table | RelKind::PartitionedTable) {
        return Ok(false);
    }
    let env = b.env;
    let snap = env.snapshot();
    let user = env.current_user();
    if snap.role(user).is_some_and(|r| r.superuser || r.bypass_rls) {
        return Ok(false);
    }
    let graph = RoleGraph::build(snap, env.database())?;
    let owner = graph.has_privs_of_role(user, rel.owner)?;
    if owner && !rel.force_row_security {
        return Ok(false);
    }
    if !env.row_security() {
        let mut e = PgError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("query would be affected by row-level security policy for table \"{}\"", rel.name),
        );
        if owner {
            e = e.with_hint("To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY.");
        }
        return Err(e);
    }
    Ok(true)
}

/// Policies of `rel` for `cmd` that apply to the current user, split (permissive, restrictive), each
/// sorted by name (PG reads pg_policy through its (polrelid, polname) index).
fn policies_for<'r>(b: &Binder, rel: &'r Relation, cmd: RlsCmd) -> Result<(Vec<&'r Policy>, Vec<&'r Policy>)> {
    let env = b.env;
    let user = env.current_user();
    let graph = RoleGraph::build(env.snapshot(), env.database())?;
    let mut perm: Vec<&Policy> = Vec::new();
    let mut restr: Vec<&Policy> = Vec::new();
    for p in &rel.policies {
        if !cmd.matches(p.cmd) {
            continue;
        }
        let applies = p.roles.is_empty()
            || p.roles.contains(&0)
            || p.roles.iter().map(|r| graph.has_privs_of_role(user, *r)).collect::<Result<Vec<bool>>>()?.into_iter().any(|x| x);
        if !applies {
            continue;
        }
        if p.permissive {
            perm.push(p);
        } else {
            restr.push(p);
        }
    }
    perm.sort_by(|a, b| a.name.cmp(&b.name));
    restr.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((perm, restr))
}

fn policy_ctx() -> ExprCtx {
    ExprCtx {
        clause: ExprClause::PolicyExpression,
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: true,
        columns_ok: true,
    }
}

/// Bind a stored policy expression over the table (`Column{0, 1, attnum}`).
fn bind_policy_expr(b: &mut Binder, rel: &Relation, sql: &str) -> Result<BoundExpr> {
    let e = bind_stored_expr(b, sql, rel.oid, policy_ctx())?;
    if e.ty.oid == pgarbor_types::oids::BOOL {
        Ok(e)
    } else {
        super::coerce::coerce_to_boolean(b, &super::context::BoundArg::new(e, None), "POLICY")
    }
}

fn or_all(mut parts: Vec<BoundExpr>) -> BoundExpr {
    if parts.len() == 1 {
        if let Some(p) = parts.pop() {
            return p;
        }
    }
    BoundExpr::new(ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::Or, args: parts })), TypeId::BOOL)
}

/// PG `add_security_quals`: restrictive USING quals (name order), then the OR of the permissive ones;
/// no permissive policy = a single FALSE.
fn security_quals(b: &mut Binder, rel: &Relation, cmd: RlsCmd, out: &mut Vec<BoundExpr>) -> Result<()> {
    let (perm, restr) = policies_for(b, rel, cmd)?;
    let mut permissive: Vec<BoundExpr> = Vec::new();
    for p in perm {
        if let Some(u) = &p.using {
            permissive.push(bind_policy_expr(b, rel, &u.sql)?);
        }
    }
    if permissive.is_empty() {
        out.push(BoundExpr::bool_const(false));
        return Ok(());
    }
    for p in restr {
        if let Some(u) = &p.using {
            let q = bind_policy_expr(b, rel, &u.sql)?;
            if !out.contains(&q) {
                out.push(q);
            }
        }
    }
    let combined = or_all(permissive);
    if !out.contains(&combined) {
        out.push(combined);
    }
    Ok(())
}

/// PG `add_with_check_options`.
fn with_check_options(
    b: &mut Binder,
    rel: &Relation,
    cmd: RlsCmd,
    kind: WcoKind,
    force_using: bool,
    out: &mut Vec<TargetWco>,
) -> Result<()> {
    let (perm, restr) = policies_for(b, rel, cmd)?;
    let pick = |p: &Policy| -> Option<String> {
        if !force_using {
            if let Some(w) = &p.with_check {
                return Some(w.sql.clone());
            }
        }
        p.using.as_ref().map(|u| u.sql.clone())
    };
    let mut permissive: Vec<BoundExpr> = Vec::new();
    for p in &perm {
        if let Some(sql) = pick(p) {
            permissive.push(bind_policy_expr(b, rel, &sql)?);
        }
    }
    if permissive.is_empty() {
        out.push(TargetWco { kind, relname: rel.name.clone(), polname: None, expr: BoundExpr::bool_const(false) });
        return Ok(());
    }
    out.push(TargetWco { kind, relname: rel.name.clone(), polname: None, expr: or_all(permissive) });
    for p in &restr {
        if let Some(sql) = pick(p) {
            let expr = bind_policy_expr(b, rel, &sql)?;
            out.push(TargetWco { kind, relname: rel.name.clone(), polname: Some(p.name.clone()), expr });
        }
    }
    Ok(())
}

/// Security quals for a DML target (UPDATE/DELETE; module docs): the command's USING quals, then the
/// SELECT ones when `reads` (the statement references the table's columns). Over rt 1.
pub fn target_quals(b: &mut Binder, rel: &Relation, cmd: RlsCmd, reads: bool) -> Result<Vec<BoundExpr>> {
    let mut out = Vec::new();
    if !rls_active(b, rel)? {
        return Ok(out);
    }
    security_quals(b, rel, cmd, &mut out)?;
    if reads {
        security_quals(b, rel, RlsCmd::Select, &mut out)?;
    }
    Ok(out)
}

/// WITH CHECK options for an INSERT/UPDATE target (module docs). Over rt 1.
pub fn target_wcos(b: &mut Binder, rel: &Relation, cmd: RlsCmd, reads: bool) -> Result<Vec<TargetWco>> {
    let mut out = Vec::new();
    if !rls_active(b, rel)? {
        return Ok(out);
    }
    let kind = if cmd == RlsCmd::Insert { WcoKind::RlsInsert } else { WcoKind::RlsUpdate };
    with_check_options(b, rel, cmd, kind, false, &mut out)?;
    if reads {
        with_check_options(b, rel, RlsCmd::Select, kind, true, &mut out)?;
    }
    Ok(out)
}

/// A table read in FROM under RLS (module docs): the relation entry becomes a sub-query with the same
/// columns (attnum `k` <-> output `k`, dropped slots included) filtered by the SELECT policies.
pub fn wrap_select(b: &mut Binder, rel: &Relation, entry: RangeEntry) -> Result<RangeEntry> {
    if !matches!(entry.kind, RangeKind::Relation { .. }) || !rls_active(b, rel)? {
        return Ok(entry);
    }
    let mut quals = Vec::new();
    security_quals(b, rel, RlsCmd::Select, &mut quals)?;
    let mut inner = entry.clone();
    inner.alias = rel.name.clone();
    inner.visible = true;
    inner.lateral = false;
    let mut target_list: Vec<TargetEntry> = Vec::with_capacity(rel.columns.len());
    let mut output: Vec<OutputColumn> = Vec::with_capacity(rel.columns.len());
    for (i, c) in rel.columns.iter().enumerate() {
        let attnum = (i + 1) as i16;
        let ty = if c.dropped { TypeId::of(0) } else { c.type_id };
        let name = entry.columns.get(i).map(|rc| rc.name.clone()).unwrap_or_else(|| c.name.clone());
        let origin = if c.dropped { None } else { Some(ColumnOrigin { table: rel.oid, attnum }) };
        target_list.push(TargetEntry { expr: BoundExpr::column(0, 1, attnum, ty), name: name.clone(), junk: false, origin });
        output.push(OutputColumn { name, ty, origin });
    }
    let where_ = match quals.len() {
        0 => None,
        1 => quals.pop(),
        _ => Some(BoundExpr::new(ExprKind::Bool(Box::new(BoolExpr { op: BoolOp::And, args: quals })), TypeId::BOOL)),
    };
    let has_sublinks = where_.as_ref().is_some_and(|w| w.contains_sublink());
    let sel = BoundSelect {
        range_table: vec![inner],
        from: vec![FromItem::Range(1)],
        where_,
        target_list,
        group_by: GroupBy::default(),
        having: None,
        distinct: DistinctSpec::None,
        windows: Vec::new(),
        has_aggs: false,
        has_windows: false,
        has_target_srfs: false,
        has_sublinks,
    };
    let query = BoundQuery {
        ctes: Vec::new(),
        body: QueryBody::Select(Box::new(sel)),
        output,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        limit_with_ties: false,
        locking: Vec::new(),
    };
    Ok(RangeEntry {
        kind: RangeKind::Subquery { query: Box::new(query) },
        alias: entry.alias,
        columns: entry.columns,
        lateral: entry.lateral,
        visible: entry.visible,
        access: AccessNeeded::default(),
    })
}
