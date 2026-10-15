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
use pgarbor_catalog::ddl::spec::ConstraintSpec;
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{ColumnOption, TableConstraint};

use super::context::Binder;

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
#[allow(unused_variables)]
pub fn bind_table_constraint(b: &mut Binder, tc: &TableConstraint, tctx: &ConstraintTableCtx) -> Result<ConstraintSpec> {
    Err(PgError::unimplemented("SQL-31", "table constraint binding (ddl_constraint::bind_table_constraint)"))
}

/// Bind a column-level PRIMARY KEY / UNIQUE / REFERENCES / CHECK option of column `column` into a
/// table constraint (`None` for options that are not constraints, e.g. NOT NULL, DEFAULT).
#[allow(unused_variables)]
pub fn bind_column_constraint(
    b: &mut Binder,
    column: &str,
    name: Option<&sqlparser::ast::Ident>,
    option: &ColumnOption,
    tctx: &ConstraintTableCtx,
) -> Result<Option<ConstraintSpec>> {
    Err(PgError::unimplemented("SQL-31", "column constraint binding (ddl_constraint::bind_column_constraint)"))
}
