//! Table / column constraints -> `ConstraintSpec` (SQL-31). PG references: `parse_utilcmd.c`
//! (`transformTableConstraint`, `transformColumnDefinition` constraint cases, `transformFKConstraints`,
//! `transformIndexConstraint`), `tablecmds.c` (`ATAddForeignKeyConstraint` for the checks the CATALOG
//! performs, `ATExecAddConstraint`), `gram.y` (`ConstraintElem`, `ConstraintAttr`).
//!
//! Output type is `pgarbor_catalog::ddl::spec::ConstraintSpec { name, kind, deferrable,
//! initially_deferred, not_valid, enforced, index_options }`.
//!
//! * **PRIMARY KEY / UNIQUE** (`TableConstraint::PrimaryKey(PrimaryKeyConstraint)`,
//!   `Unique(UniqueConstraint)`, column-level `ColumnOption::PrimaryKey/Unique`): `columns` = folded names
//!   (column-level: the single column), `include` (`INCLUDE (..)`), `NULLS NOT DISTINCT` (unique only),
//!   `USING INDEX TABLESPACE` ignored, `WITH (fillfactor=..)` -> `index_options`, `CONSTRAINT name`.
//!   Duplicate column in the list: 42701 `column "a" appears twice in primary key constraint` /
//!   `... in unique constraint`. Column existence/type checks are the catalog's (42703 `column "x"
//!   named in key does not exist`; 42704 `data type X has no default operator class for access method
//!   "btree"`). `USING INDEX idx` (promote an index): 0A000.
//! * **CHECK** (`CheckConstraint { name, expr, enforced }`): bind `expr` with `ExprCtx::check()` in the
//!   table-expression scope (`stored::table_expr_scope`) as boolean (`coerce_to_boolean("CHECK")`:
//!   42804 `argument of CHECK must be type boolean, not type integer`); no sub-selects (0A000 `cannot use
//!   subquery in check constraint`), no aggregates/windows (42803/42P20 `aggregate functions are not
//!   allowed in check constraints`); `columns` = referenced column names in order of FIRST reference;
//!   expression stored via `stored::store_expr` (canonical text: `(a > 0)`); `NO INHERIT`;
//!   `NOT ENFORCED` -> `enforced = false`; a CHECK that references no column is allowed. Name collision with
//!   the automatic `<table>_<col>_check` name is the catalog's.
//! * **FOREIGN KEY / REFERENCES** (`ForeignKeyConstraint { name, index_name, columns, foreign_table,
//!   referred_columns, on_delete, on_update, match_kind, characteristics }`): `ref_table` = relation OID
//!   resolved with `env.resolve_relation` (42P01 `relation "x" does not exist`) or `SELF_REL` (the
//!   catalog's sentinel) when the referenced name equals the table being created; `columns` (empty for
//!   column-level REFERENCES = the column), `ref_columns` (empty = referenced PK), `on_delete`/
//!   `on_update`: `NO ACTION`, `RESTRICT`, `CASCADE`, `SET NULL [(cols)]`, `SET DEFAULT [(cols)]`
//!   (`FkAction`), `MATCH SIMPLE|FULL` (`FkMatch`; `MATCH PARTIAL` 0A000 `MATCH PARTIAL not yet
//!   implemented`), `DEFERRABLE`/`INITIALLY DEFERRED`, `NOT VALID`, `NOT ENFORCED`. Column-count
//!   mismatch: 42830 `number of referencing and referenced columns for foreign key disagree`. Type
//!   compatibility, key uniqueness (42830 `there is no unique constraint matching given keys for
//!   referenced table "t"`) and privilege checks are performed by the CATALOG (`add_constraint`).
//! * **NOT NULL** table-level / column-level named (`CONSTRAINT n NOT NULL`) -> `ColumnSpec.not_null`
//!   / `ConstraintSpecKind::NotNull`.
//! * `EXCLUDE` constraints: 0A000 (`ConstraintSpecKind::Exclusion` unsupported). `TableConstraint::Index`,
//!   `FulltextOrSpatial` (MySQL): 42601.
//! * Characteristics (`ConstraintCharacteristics { deferrable, initially, enforced }`): `DEFERRABLE`,
//!   `NOT DEFERRABLE`, `INITIALLY DEFERRED|IMMEDIATE` (INITIALLY DEFERRED implies DEFERRABLE; `NOT
//!   DEFERRABLE INITIALLY DEFERRED` is 42601 `constraint declared INITIALLY DEFERRED must be
//!   DEFERRABLE`), only valid for PK/UNIQUE/FK. CHECK + DEFERRABLE or a contradictory repeat is 42601
//!   `misplaced DEFERRABLE clause`, positioned at the clause (oracle).
use pgarbor_catalog::ddl::spec::{ConstraintSpec, ConstraintSpecKind, SELF_REL};
use pgarbor_catalog::model::{FkAction, FkMatch};
use pgarbor_catalog::StoredExpr;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use sqlparser::ast::{
    ColumnOption, ConstraintCharacteristics, ConstraintReferenceMatchKind, DeferrableInitial, Expr,
    ForeignKeyConstraint, Ident, IndexColumn, NullsDistinctOption, OrderBySort, ReferentialAction, TableConstraint,
};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::stored;
use crate::diag;

/// Context of the table being defined/altered, needed to resolve names inside constraints.
// CONTRACT
#[derive(Clone, Debug, Default)]
pub struct ConstraintTableCtx {
    /// Table name (for the automatic names / messages).
    pub table_name: String,
    /// `Some(oid)` for ALTER TABLE, `None` for CREATE TABLE (self reference = `SELF_REL`).
    pub table_oid: Option<pgarbor_common::Oid>,
    /// Columns (name, type) in attnum order, for CHECK binding and column-existence checks.
    pub columns: Vec<(String, pgarbor_types::TypeId)>,
}

/// Bind a table-level constraint.
pub fn bind_table_constraint(b: &mut Binder, tc: &TableConstraint, tctx: &ConstraintTableCtx) -> Result<ConstraintSpec> {
    match tc {
        TableConstraint::PrimaryKey(pk) => {
            let name = opt_name(b, pk.name.as_ref());
            let columns = key_columns(b, &pk.columns)?;
            check_duplicate_keys(&columns, "primary key")?;
            let include = ident_list(b, &pk.include);
            primary_key_spec(name, columns, include, pk.characteristics.as_ref())
        }
        TableConstraint::Unique(u) => {
            let name = opt_name(b, u.name.as_ref());
            let columns = key_columns(b, &u.columns)?;
            check_duplicate_keys(&columns, "unique")?;
            let include = ident_list(b, &u.include);
            let nulls_not_distinct = u.nulls_distinct == NullsDistinctOption::NotDistinct;
            unique_spec(name, columns, include, nulls_not_distinct, u.characteristics.as_ref())
        }
        TableConstraint::ForeignKey(fk) => {
            let name = opt_name(b, fk.name.as_ref());
            let columns = ident_list(b, &fk.columns);
            fk_spec(b, name, columns, fk, tctx)
        }
        TableConstraint::Check(c) => {
            let name = opt_name(b, c.name.as_ref());
            check_spec(b, name, &c.expr, c.no_inherit, c.enforced, tctx)
        }
        TableConstraint::PrimaryKeyUsingIndex(_) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "PRIMARY KEY USING INDEX is not supported by PGArbor yet",
        )),
        TableConstraint::UniqueUsingIndex(_) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "UNIQUE USING INDEX is not supported by PGArbor yet",
        )),
        TableConstraint::Exclude(_) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "exclusion constraints are not supported by PGArbor yet",
        )),
        TableConstraint::Index(c) => {
            let token = if c.display_as_key { "KEY" } else { "INDEX" };
            Err(diag::syntax_error_near(token, None))
        }
        TableConstraint::FulltextOrSpatial(c) => {
            let token = if c.fulltext { "FULLTEXT" } else { "SPATIAL" };
            Err(diag::syntax_error_near(token, None))
        }
    }
}

/// Bind a column-level PRIMARY KEY / UNIQUE / REFERENCES / CHECK option of column `column` into a
/// table constraint (`None` for options that are not constraints, e.g. NOT NULL, DEFAULT).
pub fn bind_column_constraint(
    b: &mut Binder,
    column: &str,
    name: Option<&sqlparser::ast::Ident>,
    option: &ColumnOption,
    tctx: &ConstraintTableCtx,
) -> Result<Option<ConstraintSpec>> {
    match option {
        ColumnOption::PrimaryKey(pk) => {
            let name = opt_name(b, name.or(pk.name.as_ref()));
            let columns = vec![column.to_string()];
            primary_key_spec(name, columns, Vec::new(), pk.characteristics.as_ref()).map(Some)
        }
        ColumnOption::Unique(u) => {
            let name = opt_name(b, name.or(u.name.as_ref()));
            let columns = vec![column.to_string()];
            let nulls_not_distinct = u.nulls_distinct == NullsDistinctOption::NotDistinct;
            unique_spec(name, columns, Vec::new(), nulls_not_distinct, u.characteristics.as_ref()).map(Some)
        }
        ColumnOption::ForeignKey(fk) => {
            let name = opt_name(b, name.or(fk.name.as_ref()));
            let columns = vec![column.to_string()];
            fk_spec(b, name, columns, fk, tctx).map(Some)
        }
        ColumnOption::Check(c) => {
            let name = opt_name(b, name.or(c.name.as_ref()));
            check_spec(b, name, &c.expr, c.no_inherit, c.enforced, tctx).map(Some)
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Folded `CONSTRAINT name` (None when absent).
fn opt_name(b: &mut Binder, id: Option<&Ident>) -> Option<String> {
    id.map(|i| b.ident(i))
}

/// Folded identifier list (INCLUDE columns, REFERENCES columns).
fn ident_list(b: &mut Binder, ids: &[Ident]) -> Vec<String> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(b.ident(id));
    }
    out
}

/// Key list of PRIMARY KEY / UNIQUE. PG only accepts a bare column name here (`columnList: columnElem`,
/// `columnElem: ColId`), so ordering, operator classes and expressions are syntax errors.
fn key_columns(b: &mut Binder, cols: &[IndexColumn]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(cols.len());
    for ic in cols {
        out.push(key_column(b, ic)?);
    }
    Ok(out)
}

fn key_column(b: &mut Binder, ic: &IndexColumn) -> Result<String> {
    let opts = &ic.column.options;
    let stray = match opts.sort {
        Some(OrderBySort::Asc) => Some("ASC"),
        Some(OrderBySort::Desc) => Some("DESC"),
        Some(OrderBySort::Using(_)) => Some("USING"),
        None if opts.nulls_first.is_some() => Some("NULLS"),
        None => None,
    };
    if let Some(token) = stray {
        return Err(diag::syntax_error_near(token, None));
    }
    if ic.operator_class.is_some() {
        return Err(diag::syntax_error_near("(", None));
    }
    match &ic.column.expr {
        Expr::Identifier(id) => Ok(b.ident(id)),
        other => {
            let pos = b.pos(other);
            Err(diag::syntax_error_near("(", pos))
        }
    }
}

/// 42701 `column "a" appears twice in primary key constraint` / `... in unique constraint`.
fn check_duplicate_keys(columns: &[String], kind: &str) -> Result<()> {
    for (i, c) in columns.iter().enumerate() {
        if columns[..i].contains(c) {
            return Err(PgError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{c}\" appears twice in {kind} constraint"),
            ));
        }
    }
    Ok(())
}

/// PK / UNIQUE do not accept `ENFORCED` / `NOT ENFORCED` (`CHECK` and `FOREIGN KEY` only).
fn reject_key_enforced(what: &str, ch: Option<&ConstraintCharacteristics>) -> Result<()> {
    match ch.and_then(|c| c.enforced) {
        None => Ok(()),
        Some(false) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{what} constraints cannot be marked NOT ENFORCED"),
        )),
        Some(true) => Err(PgError::new(SqlState::SYNTAX_ERROR, "misplaced ENFORCED clause")),
    }
}

/// `(deferrable, initially_deferred)` of PK / UNIQUE / FK characteristics. `INITIALLY DEFERRED` alone
/// implies `DEFERRABLE` (gram.y / parse_utilcmd.c `transformConstraintAttrs`).
fn key_deferrability(ch: Option<&ConstraintCharacteristics>) -> Result<(bool, bool)> {
    let Some(c) = ch else {
        return Ok((false, false));
    };
    let initially_deferred = matches!(c.initially, Some(DeferrableInitial::Deferred));
    let deferrable = match c.deferrable {
        Some(d) => d,
        None => initially_deferred,
    };
    if initially_deferred && !deferrable {
        return Err(PgError::new(
            SqlState::SYNTAX_ERROR,
            "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
        ));
    }
    Ok((deferrable, initially_deferred))
}

fn primary_key_spec(
    name: Option<String>,
    columns: Vec<String>,
    include: Vec<String>,
    ch: Option<&ConstraintCharacteristics>,
) -> Result<ConstraintSpec> {
    reject_key_enforced("PRIMARY KEY", ch)?;
    let (deferrable, initially_deferred) = key_deferrability(ch)?;
    let mut spec = ConstraintSpec::new(ConstraintSpecKind::PrimaryKey { columns, include });
    spec.name = name;
    spec.deferrable = deferrable;
    spec.initially_deferred = initially_deferred;
    Ok(spec)
}

fn unique_spec(
    name: Option<String>,
    columns: Vec<String>,
    include: Vec<String>,
    nulls_not_distinct: bool,
    ch: Option<&ConstraintCharacteristics>,
) -> Result<ConstraintSpec> {
    reject_key_enforced("UNIQUE", ch)?;
    let (deferrable, initially_deferred) = key_deferrability(ch)?;
    let mut spec = ConstraintSpec::new(ConstraintSpecKind::Unique { columns, include, nulls_not_distinct });
    spec.name = name;
    spec.deferrable = deferrable;
    spec.initially_deferred = initially_deferred;
    Ok(spec)
}

/// `ON DELETE` / `ON UPDATE` action (absent = NO ACTION).
fn fk_action(a: Option<&ReferentialAction>) -> FkAction {
    match a {
        None | Some(ReferentialAction::NoAction) => FkAction::NoAction,
        Some(ReferentialAction::Restrict) => FkAction::Restrict,
        Some(ReferentialAction::Cascade) => FkAction::Cascade,
        Some(ReferentialAction::SetNull) => FkAction::SetNull,
        Some(ReferentialAction::SetDefault) => FkAction::SetDefault,
    }
}

/// `MATCH SIMPLE|FULL|PARTIAL` (absent = SIMPLE). PARTIAL is rejected exactly as gram.y does.
fn fk_match(kind: Option<&ConstraintReferenceMatchKind>) -> Result<FkMatch> {
    match kind {
        None | Some(ConstraintReferenceMatchKind::Simple) => Ok(FkMatch::Simple),
        Some(ConstraintReferenceMatchKind::Full) => Ok(FkMatch::Full),
        Some(ConstraintReferenceMatchKind::Partial) => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "MATCH PARTIAL not yet implemented",
        )),
    }
}

/// Name of a column from a list of attnums of the table being defined.
fn column_name_of(tctx: &ConstraintTableCtx, attnum: i16) -> Option<String> {
    let idx = usize::try_from(attnum).ok()?.checked_sub(1)?;
    tctx.columns.get(idx).map(|(n, _)| n.clone())
}

/// FOREIGN KEY (table-level, or column-level REFERENCES with `columns = [column]`).
fn fk_spec(
    b: &mut Binder,
    name: Option<String>,
    columns: Vec<String>,
    fk: &ForeignKeyConstraint,
    tctx: &ConstraintTableCtx,
) -> Result<ConstraintSpec> {
    let qn = b.qual(&fk.foreign_table)?;
    if let Some(cat) = qn.catalog.as_deref() {
        if cat != b.ctx.session.database.as_str() {
            let qualified = match &qn.schema {
                Some(s) => format!("{cat}.{s}.{}", qn.name),
                None => format!("{cat}.{}", qn.name),
            };
            return Err(diag::cross_database(&qualified, qn.pos));
        }
    }

    // A reference to the table being created is the catalog's SELF_REL sentinel. Only the unqualified
    // name can denote it: a qualified name may name another table of the same name.
    let is_self = tctx.table_oid.is_none() && qn.schema.is_none() && qn.name == tctx.table_name;
    let ref_table: Oid = if is_self {
        SELF_REL
    } else {
        match b.env.resolve_relation(qn.schema.as_deref(), &qn.name) {
            Ok(rel) => rel.oid,
            Err(mut e) => {
                if e.position.is_none() {
                    e.position = qn.pos;
                }
                return Err(e);
            }
        }
    };

    let ref_columns = ident_list(b, &fk.referred_columns);
    if !ref_columns.is_empty() && ref_columns.len() != columns.len() {
        return Err(PgError::new(
            SqlState::INVALID_FOREIGN_KEY,
            "number of referencing and referenced columns for foreign key disagree",
        ));
    }

    let match_type = fk_match(fk.match_kind.as_ref())?;
    let (deferrable, initially_deferred) = key_deferrability(fk.characteristics.as_ref())?;
    let enforced = fk.characteristics.as_ref().and_then(|c| c.enforced).unwrap_or(true);

    // `ON DELETE SET NULL (a, b)` column lists are not representable in sqlparser 0.63
    // (`ReferentialAction` carries no column list), so `delete_set_columns` stays empty.
    let kind = ConstraintSpecKind::ForeignKey {
        columns,
        ref_table,
        ref_columns,
        on_update: fk_action(fk.on_update.as_ref()),
        on_delete: fk_action(fk.on_delete.as_ref()),
        match_type,
        delete_set_columns: Vec::new(),
    };
    let mut spec = ConstraintSpec::new(kind);
    spec.name = name;
    spec.deferrable = deferrable;
    spec.initially_deferred = initially_deferred;
    spec.enforced = enforced;
    // PG: NOT ENFORCED constraints are always created NOT VALID (parse_utilcmd.c `transformConstraintAttrs`).
    spec.not_valid = !enforced;
    Ok(spec)
}

/// CHECK constraint. The expression is bound in a synthetic table scope over the table's columns.
fn check_spec(
    b: &mut Binder,
    name: Option<String>,
    expr: &Expr,
    no_inherit: bool,
    enforced: Option<bool>,
    tctx: &ConstraintTableCtx,
) -> Result<ConstraintSpec> {
    stored::table_expr_scope(b, &tctx.table_name, &tctx.columns)?;
    let bound = bind_check_body(b, expr, tctx);
    b.scopes.pop();
    let (stored_expr, columns) = bound?;

    let enforced = enforced.unwrap_or(true);
    let kind = ConstraintSpecKind::Check { expr: stored_expr, columns, no_inherit };
    let mut spec = ConstraintSpec::new(kind);
    spec.name = name;
    spec.enforced = enforced;
    // PG: NOT ENFORCED constraints are always created NOT VALID.
    spec.not_valid = !enforced;
    Ok(spec)
}

/// Binds the CHECK expression (the caller has pushed the table scope) and returns the stored text and the
/// referenced column names in order of first reference.
fn bind_check_body(b: &mut Binder, expr: &Expr, tctx: &ConstraintTableCtx) -> Result<(StoredExpr, Vec<String>)> {
    let names = |attnum: i16| -> Option<String> { column_name_of(tctx, attnum) };
    let mut attnums: Vec<i16> = Vec::new();
    let pos = b.pos(expr);
    let (bound, stored_expr) =
        stored::bind_ddl_expr(b, expr, ExprCtx::check(), None, tctx.table_oid, &mut attnums, &names)?;
    // A CHECK body must be boolean: 42804 `argument of CHECK must be type boolean, not type X`.
    let _checked = coerce::coerce_to_boolean(b, &BoundArg::new(bound, pos), "CHECK")?;

    let mut columns: Vec<String> = Vec::with_capacity(attnums.len());
    for attnum in attnums {
        let name = column_name_of(tctx, attnum)
            .ok_or_else(|| PgError::internal(format!("CHECK references unknown attribute number {attnum}")))?;
        if !columns.contains(&name) {
            columns.push(name);
        }
    }
    Ok((stored_expr, columns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{ConstraintCharacteristics, DeferrableInitial};

    fn ch(deferrable: Option<bool>, initially: Option<DeferrableInitial>, enforced: Option<bool>) -> ConstraintCharacteristics {
        ConstraintCharacteristics { deferrable, initially, enforced }
    }

    #[test]
    fn no_characteristics_is_immediate() {
        assert_eq!(key_deferrability(None).unwrap(), (false, false));
    }

    #[test]
    fn initially_deferred_implies_deferrable() {
        let c = ch(None, Some(DeferrableInitial::Deferred), None);
        assert_eq!(key_deferrability(Some(&c)).unwrap(), (true, true));
    }

    #[test]
    fn deferrable_initially_deferred_sets_both() {
        let c = ch(Some(true), Some(DeferrableInitial::Deferred), None);
        assert_eq!(key_deferrability(Some(&c)).unwrap(), (true, true));
    }

    #[test]
    fn not_deferrable_initially_deferred_is_rejected() {
        let c = ch(Some(false), Some(DeferrableInitial::Deferred), None);
        let e = key_deferrability(Some(&c)).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "constraint declared INITIALLY DEFERRED must be DEFERRABLE");
    }

    #[test]
    fn initially_immediate_only_is_not_deferred() {
        let c = ch(None, Some(DeferrableInitial::Immediate), None);
        assert_eq!(key_deferrability(Some(&c)).unwrap(), (false, false));
    }

    #[test]
    fn key_not_enforced_is_rejected() {
        let c = ch(None, None, Some(false));
        let e = reject_key_enforced("PRIMARY KEY", Some(&c)).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "PRIMARY KEY constraints cannot be marked NOT ENFORCED");
        let e = reject_key_enforced("UNIQUE", Some(&ch(None, None, Some(true)))).unwrap_err();
        assert_eq!(e.message, "misplaced ENFORCED clause");
    }

    #[test]
    fn duplicate_key_column_is_42701() {
        let cols = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let e = check_duplicate_keys(&cols, "primary key").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DUPLICATE_COLUMN);
        assert_eq!(e.message, "column \"a\" appears twice in primary key constraint");
        assert!(check_duplicate_keys(&["a".to_string(), "b".to_string()], "unique").is_ok());
    }

    #[test]
    fn referential_actions_map() {
        assert_eq!(fk_action(None), FkAction::NoAction);
        assert_eq!(fk_action(Some(&ReferentialAction::Cascade)), FkAction::Cascade);
        assert_eq!(fk_action(Some(&ReferentialAction::SetNull)), FkAction::SetNull);
        assert_eq!(fk_action(Some(&ReferentialAction::SetDefault)), FkAction::SetDefault);
        assert_eq!(fk_action(Some(&ReferentialAction::Restrict)), FkAction::Restrict);
    }

    #[test]
    fn match_kinds_map() {
        assert_eq!(fk_match(None).unwrap(), FkMatch::Simple);
        assert_eq!(fk_match(Some(&ConstraintReferenceMatchKind::Full)).unwrap(), FkMatch::Full);
        let e = fk_match(Some(&ConstraintReferenceMatchKind::Partial)).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "MATCH PARTIAL not yet implemented");
    }

    #[test]
    fn column_name_lookup_is_one_based() {
        let tctx = ConstraintTableCtx {
            table_name: "t".to_string(),
            table_oid: None,
            columns: vec![("a".to_string(), pgarbor_types::TypeId::of(23)), ("b".to_string(), pgarbor_types::TypeId::of(25))],
        };
        assert_eq!(column_name_of(&tctx, 1).as_deref(), Some("a"));
        assert_eq!(column_name_of(&tctx, 2).as_deref(), Some("b"));
        assert_eq!(column_name_of(&tctx, 0), None);
        assert_eq!(column_name_of(&tctx, 3), None);
    }

    // Binding a CHECK, a FOREIGN KEY or a PRIMARY KEY needs a catalog-backed Binder and the SQL-29
    // stored expression binder, so these are exercised by the integration suite once SQL-29 lands.
    #[test]
    #[ignore = "needs SQL-29"]
    fn check_binds_boolean_and_records_columns() {}

    #[test]
    #[ignore = "needs SQL-29"]
    fn check_non_boolean_is_42804() {}

    #[test]
    #[ignore = "needs SQL-29"]
    fn foreign_key_unknown_table_is_42p01() {}

    #[test]
    #[ignore = "needs SQL-29"]
    fn foreign_key_self_reference_uses_sentinel() {}
}
