//! UPDATE and DELETE (SQL-28), plus the pieces INSERT/ON CONFLICT share: SET assignments and
//! RETURNING. PG references: `analyze.c` (`transformUpdateStmt`, `transformDeleteStmt`,
//! `transformUpdateTargetList`, `transformReturningList`), `parse_target.c`
//! (`transformAssignedExpr`, `transformAssignmentIndirection`, `checkInsertTargets`,
//! `ExpandRowReference`), `rewriteHandler.c` (generated/identity rules).
//!
//! ## UPDATE (`bind_update`, sqlparser `Update { table, assignments, from, selection, returning, .. }`)
//! 1. Reject MySQL/other options: `or`, `order_by`, `limit`, `output`, multi-table `table` with joins
//!    (`UPDATE a JOIN b ...` is MySQL: 42601).
//! 2. `ScopeKind::Update`; target from `table.relation` (`TableFactor::Table { name, alias }`) via
//!    `target_table::open_target_table(.., DmlKind::Update, false)`.
//! 3. `FROM a, b JOIN c ...` (`UpdateTableFromKind::AfterSet`/`BeforeSet`): `from::bind_from_clause`
//!    items follow the target in the range table; the target is visible to the FROM items only through
//!    LATERAL? (PG: not at all; sub-selects in FROM cannot reference the target).
//! 4. `SET` assignments: [`bind_assignments`]. WHERE: `bind_boolean(.., ExprCtx::where_(), "WHERE")`
//!    (`WHERE CURRENT OF cursor`: 0A000). RETURNING: [`bind_returning`].
//! 5. Result `BoundStatement::Update(Box<BoundUpdate { target, target_rt: 1, range_table, ctes, from,
//!    assignments, where_, returning }>)`.
//!
//! ## DELETE (`bind_delete`, sqlparser `Delete { from: FromTable, using, selection, returning, .. }`)
//! `tables` (MySQL multi-table) non-empty -> 42601. Target = the single `FromTable::WithFromKeyword`
//! item; `USING a, b` -> like UPDATE's FROM. `BoundStatement::Delete(..)`.
//!
//! ## Assignments ([`bind_assignments`])
//! * `SET col = expr`: column resolved against the TARGET (42703 `column "x" of relation "t" does not
//!   exist`, `diag::column_of_relation_missing`). A qualified target `SET t.c = 1` parses as column `t`
//!   with a field/subscript indirection (oracle): 42703 `column "t" of relation "t" does not exist` with
//!   HINT `SET target columns cannot be qualified with the relation name.`, position at `t`.
//! * `expr` bound with `ExprCtx::update_set()`; `DEFAULT` keyword -> `SetToDefault{attnum}`; coerced with
//!   `coerce::coerce_assignment`.
//! * `SET (a, b) = (e1, e2)` -> `Assignment { targets: [a, b], value: Row([e1', e2']) }` each coerced;
//!   `SET (a, b) = (SELECT x, y ...)` -> `value: SubLink{Row}` with the sub-query's projection coerced;
//!   `SET (a, b) = ROW(..)` same as the first; arity mismatch 42601 `number of columns does not match
//!   number of values`.
//! * Duplicate targets in one statement: 42601 `multiple assignments to same column "x"`
//!   (`diag::multiple_assignments`; no position in PG's output).
//! * Generated column target: 428C9 `column "g" can only be updated to DEFAULT` DETAIL `Column "g" is a
//!   generated column.`; identity ALWAYS: 428C9 `column "id" can only be updated to DEFAULT` DETAIL
//!   `Column "id" is an identity column defined as GENERATED ALWAYS.`.
//! * Marks `access.update_cols` (and `select_cols` for referenced columns).
//!
//! ## RETURNING ([`bind_returning`])
//! `Vec<SelectItem>` bound in the DML scope with `target::bind_target_list(.., ExprCtx::returning())`:
//! `*` expands the target's columns (and FROM columns for UPDATE/DELETE: `RETURNING *` includes the
//! FROM/USING tables' columns too); aggregates/windows/SRFs are errors (42803 `aggregate functions
//! are not allowed in RETURNING`, 0A000 SRF); unknown-typed outputs resolved to text. Empty -> `vec![]`.
use pgarbor_catalog::model::IdentityKind;
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_types::{oids, TypeId};
use sqlparser::ast::{
    Assignment, AssignmentTarget, Delete, Expr, FromTable, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    Ident, ObjectName, ObjectNamePart, Query, SelectItem, TableFactor, Update, UpdateTableFromKind,
};

use super::coerce::coerce_assignment;
use super::context::{Binder, ExprCtx};
use super::expr::{bind_arg, bind_boolean};
use super::expr_sublink::bind_nested_query;
use super::from::bind_from_clause;
use super::scope::{mark_access, AccessKind, QueryScope, ScopeKind};
use super::target::bind_target_list;
use super::target_table::{open_target_table, DmlKind};
use crate::diag::{self, ExprClause};
use crate::ir::{
    AssignTarget, Assignment as BoundAssignment, BoundDelete, BoundExpr, BoundStatement, BoundUpdate, ExprKind,
    RangeEntry, RowExpr, SubLink, SubLinkKind, TargetColumn, TargetEntry, TargetTable,
};

/// SQLSTATE 428C9 `generated_always` (PG `ERRCODE_GENERATED_ALWAYS`).
const GENERATED_ALWAYS: SqlState = SqlState(*b"428C9");

/// Bind an UPDATE statement (module docs).
pub fn bind_update(b: &mut Binder, u: &Update) -> Result<BoundStatement> {
    if u.or.is_some() {
        return Err(syntax_at("OR"));
    }
    if !u.order_by.is_empty() {
        return Err(syntax_at("ORDER"));
    }
    if u.limit.is_some() {
        return Err(syntax_at("LIMIT"));
    }
    if u.output.is_some() {
        return Err(syntax_at("OUTPUT"));
    }
    if !u.table.joins.is_empty() {
        return Err(syntax_at("JOIN"));
    }
    if let Some(UpdateTableFromKind::BeforeSet(_)) = &u.from {
        return Err(syntax_at("FROM"));
    }
    let (name, alias) = plain_table_target(&u.table.relation)?;
    b.scopes.push(QueryScope::new(ScopeKind::Update));
    let result = bind_update_in_scope(b, u, name, alias);
    b.scopes.pop();
    result
}

/// Bind a DELETE statement (module docs).
pub fn bind_delete(b: &mut Binder, d: &Delete) -> Result<BoundStatement> {
    if !d.tables.is_empty() {
        return Err(syntax_at("FROM"));
    }
    let items = match &d.from {
        FromTable::WithFromKeyword(items) => items,
        FromTable::WithoutKeyword(_) => return Err(syntax_at("DELETE")),
    };
    let only = match items.as_slice() {
        [only] => only,
        [_, _, ..] => return Err(syntax_at(",")),
        [] => return Err(syntax_at("FROM")),
    };
    if !only.joins.is_empty() {
        return Err(syntax_at("JOIN"));
    }
    if !d.order_by.is_empty() {
        return Err(syntax_at("ORDER"));
    }
    if d.limit.is_some() {
        return Err(syntax_at("LIMIT"));
    }
    if d.output.is_some() {
        return Err(syntax_at("OUTPUT"));
    }
    let (name, alias) = plain_table_target(&only.relation)?;
    b.scopes.push(QueryScope::new(ScopeKind::Delete));
    let result = bind_delete_in_scope(b, d, name, alias);
    b.scopes.pop();
    result
}

/// Bind `SET` assignments against `target` (range-table index `target_rt` of the current scope);
/// shared by UPDATE and ON CONFLICT DO UPDATE.
pub fn bind_assignments(b: &mut Binder, assignments: &[Assignment], target: &TargetTable, target_rt: u32) -> Result<Vec<BoundAssignment>> {
    let mut seen: Vec<i16> = Vec::with_capacity(assignments.len());
    let mut out: Vec<BoundAssignment> = Vec::with_capacity(assignments.len());
    for a in assignments {
        match &a.target {
            AssignmentTarget::ColumnName(name) => {
                let col = resolve_set_column(b, target, target_rt, name)?;
                claim(&mut seen, &col)?;
                let is_default = is_default_keyword(&a.value);
                let value = bind_set_value(b, &a.value, &col)?;
                check_default_only(&col, is_default)?;
                mark_update(b, target_rt, col.attnum)?;
                out.push(BoundAssignment { targets: vec![col.assign_target()], value });
            }
            AssignmentTarget::Tuple(names) => {
                let mut cols: Vec<SetCol> = Vec::with_capacity(names.len());
                for n in names {
                    let col = resolve_set_column(b, target, target_rt, n)?;
                    claim(&mut seen, &col)?;
                    cols.push(col);
                }
                let value = bind_multi_source(b, a, &cols)?;
                for col in &cols {
                    mark_update(b, target_rt, col.attnum)?;
                }
                out.push(BoundAssignment { targets: cols.iter().map(SetCol::assign_target).collect(), value });
            }
        }
    }
    Ok(out)
}

/// Bind a RETURNING list in the current (DML) scope.
pub fn bind_returning(b: &mut Binder, items: Option<&[SelectItem]>) -> Result<Vec<TargetEntry>> {
    let items = match items {
        Some(items) => items,
        None => return Ok(Vec::new()),
    };
    let srf_before = b.scope()?.has_target_srfs;
    let entries = bind_target_list(b, items, ExprCtx::returning())?;
    if !srf_before && b.scope()?.has_target_srfs {
        return Err(diag::srf_not_allowed(ExprClause::Other("RETURNING"), None));
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------------------------
// Statement bodies (run with the DML scope already pushed; the caller pops it)
// ---------------------------------------------------------------------------------------------

fn bind_update_in_scope(b: &mut Binder, u: &Update, name: &ObjectName, alias: Option<&Ident>) -> Result<BoundStatement> {
    let (target, target_rt) = open_target_table(b, name, alias, DmlKind::Update, false)?;
    // `open_target_table` already put the target in the namespace (SET, WHERE, RETURNING); FROM items follow.
    let from = match &u.from {
        Some(UpdateTableFromKind::BeforeSet(items)) | Some(UpdateTableFromKind::AfterSet(items)) => bind_from_clause(b, items)?,
        None => Vec::new(),
    };
    let assignments = bind_assignments(b, &u.assignments, &target, target_rt)?;
    let where_ = match &u.selection {
        Some(e) => Some(bind_boolean(b, e, ExprCtx::where_(), "WHERE")?),
        None => None,
    };
    let returning = bind_returning(b, u.returning.as_deref())?;
    let range_table = std::mem::take(&mut b.scope_mut()?.range_table);
    Ok(BoundStatement::Update(Box::new(BoundUpdate {
        target,
        target_rt,
        range_table,
        ctes: Vec::new(),
        from,
        assignments,
        where_,
        returning,
    })))
}

fn bind_delete_in_scope(b: &mut Binder, d: &Delete, name: &ObjectName, alias: Option<&Ident>) -> Result<BoundStatement> {
    let (target, target_rt) = open_target_table(b, name, alias, DmlKind::Delete, false)?;
    let using = match &d.using {
        Some(items) => bind_from_clause(b, items)?,
        None => Vec::new(),
    };
    let where_ = match &d.selection {
        Some(e) => Some(bind_boolean(b, e, ExprCtx::where_(), "WHERE")?),
        None => None,
    };
    let returning = bind_returning(b, d.returning.as_deref())?;
    let range_table = std::mem::take(&mut b.scope_mut()?.range_table);
    Ok(BoundStatement::Delete(Box::new(BoundDelete {
        target,
        target_rt,
        range_table,
        ctes: Vec::new(),
        using,
        where_,
        returning,
    })))
}

// ---------------------------------------------------------------------------------------------
// Assignment helpers
// ---------------------------------------------------------------------------------------------

/// A resolved SET target column (non-dropped column of the DML target).
#[derive(Clone, Debug)]
struct SetCol {
    attnum: i16,
    name: String,
    ty: TypeId,
    /// `GENERATED ALWAYS AS (..) STORED` column: only DEFAULT may be assigned.
    generated: bool,
    /// `GENERATED ALWAYS AS IDENTITY` column: only DEFAULT may be assigned.
    identity_always: bool,
}

impl SetCol {
    fn from_target(c: &TargetColumn) -> SetCol {
        SetCol {
            attnum: c.attnum,
            name: c.name.clone(),
            ty: c.ty,
            generated: c.generated.is_some(),
            identity_always: matches!(c.identity, Some(IdentityKind::Always)),
        }
    }

    fn assign_target(&self) -> AssignTarget {
        AssignTarget { attnum: self.attnum, name: self.name.clone(), ty: self.ty }
    }
}

/// Resolve one SET target name against the DML target (PG `transformUpdateTargetList`).
/// The first identifier is the column; further identifiers are indirection (`SET t.c = 1` names
/// column `t`, which does not exist: 42703 with the qualified-name HINT when `t` is the target alias).
fn resolve_set_column(b: &mut Binder, target: &TargetTable, target_rt: u32, name: &ObjectName) -> Result<SetCol> {
    let pos = b.pos(name);
    let mut idents: Vec<&Ident> = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            ObjectNamePart::Identifier(id) => idents.push(id),
            _ => return Err(diag::syntax("syntax error", pos)),
        }
    }
    let first: &Ident = match idents.first().copied() {
        Some(first) => first,
        None => return Err(diag::syntax("syntax error", pos)),
    };
    let col = b.ident(first);
    let has_indirection = idents.len() > 1;
    if !has_indirection {
        return match target.live_columns().find(|c| c.name == col) {
            Some(c) => Ok(SetCol::from_target(c)),
            None => Err(diag::column_of_relation_missing(&col, &target.name, pos)),
        };
    }
    // Qualified or indirected target: only a column that exists can take indirection, and field /
    // subscript assignment is not representable in the IR yet.
    if target.live_columns().any(|c| c.name == col) {
        return Err(diag::unsupported_construct("field or subscript assignment in SET", pos));
    }
    let alias = target_alias(b, target_rt)?;
    let mut err = diag::column_of_relation_missing(&col, &target.name, pos);
    if col == alias {
        err = err.with_hint("SET target columns cannot be qualified with the relation name.");
    }
    Err(err)
}

/// Record that `col` is assigned by this statement; a second assignment is 42601.
fn claim(seen: &mut Vec<i16>, col: &SetCol) -> Result<()> {
    if seen.contains(&col.attnum) {
        return Err(diag::multiple_assignments(&col.name, None));
    }
    seen.push(col.attnum);
    Ok(())
}

/// `DEFAULT` as the whole source expression. sqlparser yields an unquoted `default` identifier.
fn is_default_keyword(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(id) if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("default"))
}

/// Generated and identity-ALWAYS columns accept only DEFAULT (PG `rewriteTargetListIU`; identity
/// is checked first, as in PG). These errors carry no position.
fn check_default_only(col: &SetCol, is_default: bool) -> Result<()> {
    if is_default {
        return Ok(());
    }
    if col.identity_always {
        return Err(PgError::new(GENERATED_ALWAYS, format!("column \"{}\" can only be updated to DEFAULT", col.name))
            .with_detail(format!("Column \"{}\" is an identity column defined as GENERATED ALWAYS.", col.name)));
    }
    if col.generated {
        return Err(PgError::new(GENERATED_ALWAYS, format!("column \"{}\" can only be updated to DEFAULT", col.name))
            .with_detail(format!("Column \"{}\" is a generated column.", col.name)));
    }
    Ok(())
}

/// Bind the value of a single-column assignment (`DEFAULT` or an assignment-coerced expression).
fn bind_set_value(b: &mut Binder, value: &Expr, col: &SetCol) -> Result<BoundExpr> {
    if is_default_keyword(value) {
        return Ok(BoundExpr::new(ExprKind::SetToDefault { attnum: col.attnum }, col.ty));
    }
    let arg = bind_arg(b, value, ExprCtx::update_set())?;
    coerce_assignment(b, &arg, &col.name, col.ty)
}

/// Source of a multi-column assignment `SET (a, b) = <source>`: a parenthesized row, `ROW(..)`, or
/// a sub-SELECT (PG `transformMultiAssignRef`).
fn bind_multi_source(b: &mut Binder, a: &Assignment, cols: &[SetCol]) -> Result<BoundExpr> {
    let pos = b.pos(&a.value);
    let items: Option<Vec<&Expr>> = match &a.value {
        Expr::Tuple(items) => Some(items.iter().collect()),
        Expr::Nested(e) if cols.len() == 1 => Some(vec![e.as_ref()]),
        Expr::Function(f) => row_function_args(f),
        Expr::Subquery(q) => return bind_subquery_source(b, q, cols, pos),
        _ => None,
    };
    match items {
        Some(items) => bind_row_items(b, &items, cols, pos),
        None => Err(diag::with_position(
            PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() expression",
            ),
            pos,
        )),
    }
}

/// `ROW(e1, e2, ..)` arguments when `f` is the unquoted function `row`.
fn row_function_args(f: &Function) -> Option<Vec<&Expr>> {
    let is_row = match f.name.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => id.quote_style.is_none() && id.value.eq_ignore_ascii_case("row"),
        _ => false,
    };
    if !is_row {
        return None;
    }
    match &f.args {
        FunctionArguments::None => Some(Vec::new()),
        FunctionArguments::List(list) => {
            let mut out: Vec<&Expr> = Vec::with_capacity(list.args.len());
            for arg in &list.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => out.push(e),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Row source: one expression per target column (`DEFAULT` allowed per item). A single target
/// yields its scalar value; several targets yield a `Row` of coerced values.
fn bind_row_items(b: &mut Binder, items: &[&Expr], cols: &[SetCol], pos: Option<u32>) -> Result<BoundExpr> {
    if items.len() != cols.len() {
        return Err(diag::syntax("number of columns does not match number of values", pos));
    }
    let mut args: Vec<BoundExpr> = Vec::with_capacity(cols.len());
    for (&e, col) in items.iter().zip(cols.iter()) {
        let is_default = is_default_keyword(e);
        let v = bind_set_value(b, e, col)?;
        check_default_only(col, is_default)?;
        args.push(v);
    }
    if args.len() == 1 {
        if let Some(v) = args.pop() {
            return Ok(v);
        }
    }
    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    Ok(BoundExpr::new(ExprKind::Row(Box::new(RowExpr { args, names })), TypeId::of(oids::RECORD)))
}

/// Sub-SELECT source: `SubLink{kind: Row}`. The sub-query's output must already have the target
/// types, because the projection is not rewritten here (a type mismatch is 0A000 until the binder
/// can coerce the projection of a sub-query in place).
fn bind_subquery_source(b: &mut Binder, q: &Query, cols: &[SetCol], pos: Option<u32>) -> Result<BoundExpr> {
    let bq = bind_nested_query(b, q)?;
    if bq.output.len() != cols.len() {
        return Err(diag::syntax("number of columns does not match number of values", pos));
    }
    for (out, col) in bq.output.iter().zip(cols.iter()) {
        if out.ty.oid != col.ty.oid || (col.ty.typmod != -1 && out.ty.typmod != col.ty.typmod) {
            return Err(diag::unsupported_construct("type coercion of a multiple-column UPDATE sub-SELECT", pos));
        }
        check_default_only(col, false)?;
    }
    b.scope_mut()?.has_sublinks = true;
    Ok(BoundExpr::new(
        ExprKind::SubLink(Box::new(SubLink {
            kind: SubLinkKind::Row,
            test: None,
            ops: Vec::new(),
            subquery: Box::new(bq),
        })),
        TypeId::of(oids::RECORD),
    ))
}

/// Mark `attnum` of the DML target as updated (ACL `update_cols`).
fn mark_update(b: &mut Binder, target_rt: u32, attnum: i16) -> Result<()> {
    let entry = dml_target_entry_mut(b, target_rt)?;
    mark_access(&mut entry.access, AccessKind::Update, attnum);
    Ok(())
}

fn dml_target_entry_mut<'b>(b: &'b mut Binder<'_>, target_rt: u32) -> Result<&'b mut RangeEntry> {
    let idx = (target_rt as usize).wrapping_sub(1);
    b.scope_mut()?
        .range_table
        .get_mut(idx)
        .ok_or_else(|| PgError::internal("binder: DML target range entry missing"))
}

fn target_alias(b: &Binder, target_rt: u32) -> Result<String> {
    let idx = (target_rt as usize).wrapping_sub(1);
    Ok(b.scope()?.range_table.get(idx).map(|e| e.alias.clone()).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// Syntax helpers
// ---------------------------------------------------------------------------------------------

/// 42601 `syntax error at or near "tok"` for constructs PGArbor rejects (MySQL/SQLite/MSSQL extras).
fn syntax_at(token: &str) -> PgError {
    diag::syntax_error_near(token, None)
}

/// The plain `name [AS alias]` target of UPDATE / DELETE; table functions, derived tables and
/// other factors are 42601.
fn plain_table_target(tf: &TableFactor) -> Result<(&ObjectName, Option<&Ident>)> {
    match tf {
        TableFactor::Table { name, alias, args: None, .. } => Ok((name, alias.as_ref().map(|a| &a.name))),
        _ => Err(syntax_at("(")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_col(attnum: i16, name: &str, generated: bool, identity_always: bool) -> SetCol {
        SetCol { attnum, name: name.to_string(), ty: TypeId::of(oids::INT4), generated, identity_always }
    }

    #[test]
    fn default_keyword_is_unquoted_default_identifier_only() {
        assert!(is_default_keyword(&Expr::Identifier(Ident::new("DEFAULT"))));
        assert!(is_default_keyword(&Expr::Identifier(Ident::new("default"))));
        let mut quoted = Ident::new("default");
        quoted.quote_style = Some('"');
        assert!(!is_default_keyword(&Expr::Identifier(quoted)));
        assert!(!is_default_keyword(&Expr::Identifier(Ident::new("defaults"))));
    }

    #[test]
    fn duplicate_assignment_is_42601_without_position() {
        let mut seen: Vec<i16> = Vec::new();
        assert!(claim(&mut seen, &set_col(1, "a", false, false)).is_ok());
        let err = claim(&mut seen, &set_col(1, "a", false, false)).err().expect("duplicate must fail");
        assert_eq!(err.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(err.message, "multiple assignments to same column \"a\"");
        assert_eq!(err.position, None);
        assert!(claim(&mut seen, &set_col(2, "b", false, false)).is_ok());
    }

    #[test]
    fn generated_column_accepts_only_default() {
        let g = set_col(2, "g", true, false);
        assert!(check_default_only(&g, true).is_ok());
        let err = check_default_only(&g, false).err().expect("non-default must fail");
        assert_eq!(err.sqlstate.code(), "428C9");
        assert_eq!(err.message, "column \"g\" can only be updated to DEFAULT");
        assert_eq!(err.detail.as_deref(), Some("Column \"g\" is a generated column."));
    }

    #[test]
    fn identity_always_column_accepts_only_default() {
        let id = set_col(1, "id", false, true);
        assert!(check_default_only(&id, true).is_ok());
        let err = check_default_only(&id, false).err().expect("non-default must fail");
        assert_eq!(err.sqlstate.code(), "428C9");
        assert_eq!(err.message, "column \"id\" can only be updated to DEFAULT");
        assert_eq!(
            err.detail.as_deref(),
            Some("Column \"id\" is an identity column defined as GENERATED ALWAYS.")
        );
    }

    #[test]
    fn plain_column_accepts_any_value() {
        let a = set_col(1, "a", false, false);
        assert!(check_default_only(&a, false).is_ok());
    }

    #[test]
    fn update_and_delete_register_target_once() {
        use crate::binder::context::{BindContext, BindSession};
        use pgarbor_catalog::ddl::spec::{ColumnSpec, CreateTableSpec};
        use pgarbor_catalog::model::{Persistence, RelKind};
        use pgarbor_catalog::testing;
        use pgarbor_types::context::IoConfig;

        let (mut kv, env) = testing::bootstrapped().unwrap();
        let spec = CreateTableSpec {
            namespace: pgarbor_catalog::builtin::PUBLIC_NAMESPACE,
            name: "t".to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::of(oids::INT4)), ColumnSpec::new("b", TypeId::of(oids::TEXT))],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        };
        let (_r, env2) = testing::run(&mut kv, &env, |t| {
            t.create_table(&spec)?;
            Ok(())
        })
        .unwrap();
        let ctx = BindContext {
            env: &env2,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        for sql in ["UPDATE t SET b = b || '!' WHERE a = 1", "DELETE FROM t WHERE a = 2", "UPDATE t x SET b = 'q' WHERE x.a = 1"] {
            let raw = crate::parser::parse_one(sql).unwrap();
            crate::binder::bind(&raw, &ctx).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
    }
}
