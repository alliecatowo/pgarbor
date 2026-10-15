//! The catalog's `ExprRewriter`: re-deparse stored expressions after a rename (SQL-45, sonnet).
//! Implements amendment `pgarbor-catalog-04-sql-expression-rewriter`.
//!
//! PostgreSQL stores parsed trees, so `ALTER TABLE t RENAME b TO bb` leaves `pg_get_constraintdef`
//! printing `CHECK ((bb <> ''::text))` for free. PGArbor stores canonical TEXT, so every stored
//! expression whose `depends_on` mentions the renamed object must be re-deparsed.
//!
//! [`SqlExprRewriter`] implements `pgarbor_catalog::ddl::spec::ExprRewriter`:
//! `rewrite(expr, change, env)`:
//! 1. Decide whether `expr.sql` is an EXPRESSION or a VIEW QUERY (`expr.sql` starts with `" SELECT"` /
//!    `" VALUES"` / `" WITH"`, i.e. begins with a space + keyword and ends with `;`).
//! 2. Parse the OLD text (`parser::parse_expr_text` or `parse_one`).
//! 3. Rename-aware re-binding: bind the parsed text against a catalog view in which the change is
//!    ALREADY applied (`env` is the snapshot AFTER the rename, per the trait docs) — except that the old
//!    text still contains the OLD names, so first rewrite the AST: for `NameChange::Column{table, attnum,
//!    old, new}` replace column identifiers `old` that resolve to that column; for
//!    `Relation{oid, old, new}` replace relation names in `FROM` and in `regclass` string literals
//!    (`'old_seq'::regclass` -> `'new_seq'::regclass`, schema-qualified as the deparser would print);
//!    for `RelationSchema` / `Schema` re-qualify; for `Type`/`Function` replace names in casts / calls.
//!    The robust way: bind the OLD text with name resolution done through OIDs recorded in
//!    `depends_on` (rebind with a binder whose name lookup maps old names to the renamed objects), then
//!    deparse the bound tree with the NEW names (the deparser reads names from the catalog snapshot, so
//!    it prints new names automatically). Implement it that way: `binder::stored::bind_stored_expr_with_
//!    aliases` semantics = bind with an environment overlay `{old name -> oid}`.
//! 4. `deparse_expr` / `deparse_viewdef` -> new `sql`; `bound` stays `None`; `depends_on` unchanged
//!    (names are not part of dependencies).
//! Oracle fixtures: after `ALTER TABLE t RENAME b TO bb`: `pg_get_constraintdef` -> `CHECK ((bb <> ''::text))`;
//! view `SELECT a, b AS b FROM t` -> ` SELECT a,\n    bb AS b\n   FROM t;` (output alias keeps the OLD
//! view column name `b` — the view's own column is not renamed); after `ALTER SEQUENCE s RENAME TO s2`:
//! default `nextval('s2'::regclass)`.
use pgarbor_catalog::ddl::spec::{ExprRewriter, NameChange};
use pgarbor_catalog::{CatalogEnv, StoredExpr};
use pgarbor_common::{PgError, Result};

/// The rewriter handed to `CatalogTxn::rename_*` / `set_*_schema`.
// CONTRACT
#[derive(Clone, Copy, Debug, Default)]
pub struct SqlExprRewriter;

impl ExprRewriter for SqlExprRewriter {
    fn rewrite(&self, expr: &StoredExpr, change: &NameChange, env: &CatalogEnv) -> Result<StoredExpr> {
        rewrite_stored_expr(expr, change, env)
    }
}

/// Worker body (module docs). Kept as a free function so tests can call it directly.
#[allow(unused_variables)]
pub fn rewrite_stored_expr(expr: &StoredExpr, change: &NameChange, env: &CatalogEnv) -> Result<StoredExpr> {
    Err(PgError::unimplemented("SQL-45", "stored expression rewrite (rewrite::rewrite_stored_expr)"))
}
