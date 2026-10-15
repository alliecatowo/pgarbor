//! ALTER TABLE column structure: ADD COLUMN, DROP COLUMN, RENAME COLUMN, ALTER COLUMN TYPE.
//! Lease CAT-14 (sonnet). PG sources: `commands/tablecmds.c` (`ATExecAddColumn`,
//! `ATExecDropColumn`, `renameatt`, `ATPrepAlterColumnType`/`ATExecAlterColumnType`,
//! `ATRewriteTables` decision in `ATRewriteCatalogs`), `catalog/heap.c` (`AddRelationNewConstraints`,
//! `RemoveAttrDefault`), `catalog/dependency.c` (column drops via `performDeletion`).
//!
//! All four require the relation to be owned by the current user (`require_owner`) and of an
//! acceptable kind; otherwise `errors::alter_action_not_supported("ADD COLUMN", "v", "views")`
//! (42809 `ALTER action ADD COLUMN cannot be performed on relation "v"` DETAIL `This operation is
//! not supported for views.`) - action words `ADD COLUMN`, `DROP COLUMN`, `ALTER COLUMN TYPE`;
//! RENAME COLUMN is allowed on tables, views, matviews and composite types (not indexes/sequences:
//! `"s" is not a table, view, materialized view, composite type, or foreign table`).
//! System catalogs: 42501 `permission denied: "pg_class" is a system catalog`.
//!
//! ## ADD COLUMN ([`CatalogTxn::add_column`])
//! Same column semantics as CREATE TABLE (collation, serial -> sequence + nextval default,
//! identity, generated, NOT NULL constraint `t_a_not_null`) with attnum = `next_attnum()`.
//! - existing name (visible columns) -> 42701 `column "a" of relation "t" already exists`, or NOTICE
//!   `column "a" of relation "t" already exists, skipping` + `Ok(None)` with `if_not_exists`;
//!   system-column names -> 42701 `column name "xmin" conflicts with a system column name`;
//!   > 1600 columns -> 54011 `tables can have at most 1600 columns`.
//! - Data consequences (existing rows):
//!   * constant/non-volatile default (`spec.fast_default = Some(text)`): stored in
//!     `Column::missing_value`; no follow-up (rows read the missing value).
//!   * default present but `default_is_volatile` (serial, `random()`, `nextval()`), identity, or
//!     STORED generated column: `FollowUp::RewriteTable{table}`.
//!   * `generated` VIRTUAL: nothing stored.
//!   * no default: existing rows read NULL; with NOT NULL queue `FollowUp::ValidateNotNull`
//!     (executor raises 23502 `column "a" of relation "t" contains null values` if rows exist).
//! - Records created sequences (`CreateSequenceState`) like CREATE TABLE.
//! - Returns the new attnum.
//!
//! ## DROP COLUMN ([`CatalogTxn::drop_column`])
//! - unknown column -> 42703 `column "x" of relation "t" does not exist` or NOTICE `... does not
//!   exist, skipping` + `Ok(false)` with `missing_ok`; system column -> 0A000 `errors::cannot_drop_system_column`.
//! - Dependency-driven through `drop_objects(&[ObjectAddress::column(table, attnum)], behavior)`:
//!   constraints and indexes involving the column (AUTO) vanish silently - also multi-column
//!   PRIMARY KEY/UNIQUE/CHECK (oracle) -; defaults, serial sequences (AUTO), identity sequences
//!   (INTERNAL) vanish; views, FKs from other tables, generated columns using it (NORMAL) block with
//!   2BP01 `cannot drop column a of table t because other objects depend on it` DETAIL
//!   `view v depends on column a of table t` ... HINT `Use DROP ... CASCADE ...` (RESTRICT) or are
//!   dropped with the CASCADE NOTICE (`drop cascades to view v`). Dropping the column of a rowtype used
//!   as column type elsewhere is the NORMAL dependency of that column on the type.
//! - Effect on the relation (done by `drop.rs` when the column address is executed; this function
//!   only validates and delegates): the `Column` is rewritten as dropped (`Column::dropped_name`,
//!   `type_id` oid 0 typmod -1, not_null false, default None, identity None, generated None,
//!   missing_value None). The slot stays so row tuples keep decoding.
//! - Dropping the last visible column is allowed (zero-column table).
//!
//! Composite types (`RelKind::CompositeType`, reached through ALTER TYPE ... ATTRIBUTE) are accepted by
//! all four functions; for ADD/DROP/ALTER TYPE they queue `FollowUp::RequireEmptyTable` for each table
//! column declared with the composite type (see `typedef.rs`).
//!
//! ## RENAME COLUMN ([`CatalogTxn::rename_column`])
//! - unknown -> 42703 `column "x" does not exist` (note: no `of relation` here, oracle);
//!   system column -> 0A000 `cannot rename system column "ctid"`; new name taken -> 42701
//!   `column "c" of relation "t" already exists`; renaming to the same name is a no-op error-free?
//!   (PG: 42701 as well - `column "a" of relation "t" already exists`).
//! - Stored text that mentions the column must change (CHECK bodies, defaults, generation
//!   expressions, index expressions/predicates, view queries, ...): collect every `StoredExpr`
//!   whose `depends_on` contains `ObjectAddress::column(table, attnum)` and replace it with
//!   `rewriter.rewrite(expr, &NameChange::Column{..}, env)`. Oracle: after `rename b to bb`,
//!   `pg_get_constraintdef` prints `CHECK ((bb <> ''::text))`, `pg_get_indexdef(ib)` prints `(bb)` -
//!   but the INDEX ATTRIBUTE NAME (`IndexKey::name`) stays `b`, as in PG.
//! - Renaming a column of a VIEW changes the view's output name: the view definition text must be
//!   re-deparsed too (` SELECT a AS aa,\n    bb AS b\n   FROM t;`): for views the dependency list of
//!   the VIEW's own query is not enough (the column is an output of the view, not a dependency), so
//!   views ALWAYS get `rewriter.rewrite(view.query, NameChange::Column{table: view, ...})`; views that
//!   SELECT FROM this view are rewritten through their `depends_on` as usual.
//! - Composite type attribute rename (`ALTER TYPE c RENAME ATTRIBUTE a TO b`) uses this function
//!   (`RelKind::CompositeType`); stored data of composite columns is positional so nothing else.
//!
//! ## ALTER COLUMN TYPE ([`CatalogTxn::alter_column_type`])
//! The binder has already decided what PG's `ATPrepAlterColumnType` decides: it resolved the
//! `USING` expression (or the implicit/assignment cast), checked coercibility (42804 `column "bb"
//! cannot be cast automatically to type integer` HINT `You might need to specify "USING bb::integer".`),
//! and whether the stored representation changes (`needs_rewrite`: int4 -> int8 yes; varchar(10) ->
//! varchar(20), varchar -> text, numeric precision widening no). The catalog:
//! - rejects system columns (`errors::cannot_alter_system_column`), dropped/unknown columns (42703);
//! - rejects columns used by views/matviews: 0A000 `errors::cannot_alter_type_used_by_view(view, col)`
//!   (any view/matview whose `depends_on` lists this column), by generated columns of the same table:
//!   0A000 `errors::cannot_alter_type_used_by_generated`, and by partition keys (later);
//! - re-derives index keys that are plain references to the column (`IndexKey::type_id`, `collation`,
//!   `opclass` = default btree opclass of the new type, 42704 `errors::no_default_opclass` if none);
//!   indexes with expression/predicate on the column: left as is (the binder re-validates);
//! - re-validates FKs on both sides (referencing column type changed, or referenced column type
//!   changed): pair compatibility as in `add_constraint`, else 42804 `errors::fk_cannot_be_implemented`;
//!   `ForeignKey::*_eq_ops` recomputed or cleared;
//! - applies `new_default` (the binder re-coerced the default to the new type;
//!   `Some(Some(e))` replaces, `Some(None)` removes, `None` keeps) ;
//! - updates the column: `type_id`, `collation` (explicit or the new type's), `ndims`, clears
//!   `missing_value` when `needs_rewrite`; identity column: the identity sequence's `type_oid` and
//!   default bounds follow the column type (bounds that were the old type's defaults become the
//!   new type's defaults; custom bounds out of range -> 22023 `MAXVALUE (...) is out of range for
//!   sequence data type smallint`);
//! - queues `FollowUp::RewriteTable{table}` when `needs_rewrite` (indexes are rebuilt by the rewrite).
use super::spec::*;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::TypeId;

impl<'a> CatalogTxn<'a> {
    /// ALTER TABLE ADD COLUMN (module docs). Returns the new attnum, or `None` when skipped by
    /// `IF NOT EXISTS`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn add_column(&mut self, table: Oid, column: &ColumnSpec, if_not_exists: bool) -> Result<Option<i16>> {
        Err(PgError::unimplemented("CAT-14", "ADD COLUMN"))
    }

    /// ALTER TABLE DROP COLUMN (module docs). `Ok(false)` when skipped by `missing_ok`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn drop_column(&mut self, table: Oid, column: &str, behavior: DropBehavior, missing_ok: bool) -> Result<bool> {
        Err(PgError::unimplemented("CAT-14", "DROP COLUMN"))
    }

    /// ALTER TABLE/VIEW/TYPE RENAME COLUMN (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_column(&mut self, rel: Oid, old: &str, new: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-14", "RENAME COLUMN"))
    }

    /// ALTER TABLE ALTER COLUMN TYPE (module docs).
    // CONTRACT
    #[allow(unused_variables, clippy::too_many_arguments)]
    pub fn alter_column_type(
        &mut self,
        table: Oid,
        column: &str,
        new_type: TypeId,
        new_collation: Option<Oid>,
        new_ndims: i32,
        new_default: Option<Option<StoredExpr>>,
        needs_rewrite: bool,
    ) -> Result<()> {
        Err(PgError::unimplemented("CAT-14", "ALTER COLUMN TYPE"))
    }
}
