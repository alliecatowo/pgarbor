//! CREATE [OR REPLACE] VIEW, CREATE MATERIALIZED VIEW, REFRESH MATERIALIZED VIEW (catalog side).
//! Lease CAT-17. PG sources: `commands/view.c` (`DefineView`, `DefineVirtualRelation`,
//! `checkViewColumns`), `commands/createas.c` (`create_ctas_internal`), `commands/matview.c`
//! (`ExecRefreshMatView`, `SetMatViewPopulatedState`), `catalog/heap.c`.
//!
//! The view's QUERY is analysed by the binder: `ViewSpec::columns` are the output columns (names
//! after the optional column alias list, `type_id` incl. typmod, collation), `ViewSpec::query` is
//! the `pg_get_viewdef` text with `depends_on` listing every relation, column, function and type the
//! query references (so DROP ... RESTRICT/CASCADE and `ALTER COLUMN TYPE` checks work; see
//! `depend.rs`: a view depends NORMALly on each listed object).
//!
//! ## `create_view`
//! Kinds: `ViewSpec::materialized == false` -> `RelKind::View`, true -> `RelKind::MatView`.
//! 1. checks: `check_creatable_namespace`, `require_schema_create`, name check
//!    (`check_new_relation_name`; matviews with `if_not_exists`: NOTICE `relation "mv" already exists,
//!    skipping` + `Ok(None)`); > 1600 columns; zero columns -> 42P16 `view must have at least one
//!    column`; duplicate output names -> 42701 `column "a" specified more than once`; pseudo-type
//!    columns: 42P16 `column "a" has pseudo-type record` (unknown-typed literals are coerced to text
//!    by the binder, as PG: `create view v as select 'x' as a` yields text).
//! 2. new relation: OIDs for relation, rowtype, array type; `Column`s (attnum = position + 1, no
//!    defaults/constraints), `view = Some(ViewDef{query, check_option, populated})`, `options`
//!    (`security_barrier=true`, `security_invoker=...`, `check_option` mirrored), views have NO
//!    system columns and `replica_identity = Nothing`; matviews `Default` and storage.
//! 3. materialized views queue `FollowUp::PopulateMatView{relation}` when `populated`.
//! 4. `or_replace` on an EXISTING relation of the same name (`ViewSpec::or_replace`): the existing
//!    relation must be a plain view (42809 `"t" is not a view`; matview: 42809 `"mv" is not a view`
//!    HINT `Use DROP MATERIALIZED VIEW to remove a materialized view.`) and, per
//!    `checkViewColumns`: the new column list must START WITH the old columns, same names
//!    (42P16 `errors::cannot_change_view_column_name(old, new)`), same types/typmods/collations
//!    (42P16 `errors::cannot_change_view_column_type(col, from, to)` - type names via
//!    `pgarbor_types::names::format_type`), and not be shorter (42P16 `errors::cannot_drop_view_columns`);
//!    extra columns are appended (attnum continues). The owner, ACL, options not given, and OID are
//!    kept; `query`/`check_option` replaced. Dependencies follow automatically. The caller must own
//!    the view. Oracle-verified: `create or replace view zz_v as select 1 as b` (old column `a`)
//!    -> `cannot change name of view column "a" to "b"` HINT `Use ALTER VIEW ... RENAME COLUMN ...
//!    to change name of view column instead.`; adding `, 2 as c` works.
//! 5. Records `created` (for replace: nothing created).
//!
//! ## `refresh_matview`
//! `with_data = true`: sets `ViewDef::populated = true`, queues `PopulateMatView`; `false`
//! (`WITH NO DATA`): `populated = false`, queues `DropRelationData` for the matview and each of its
//! indexes. A non-matview -> 42809 `"v" is not a materialized view`. `REFRESH ... CONCURRENTLY`
//! requires a unique index (55000 `cannot refresh materialized view "public.mv" concurrently` HINT
//! `Create a unique index with no WHERE clause on one or more columns of the materialized view.`)
//! and a populated matview (55000 `CONCURRENTLY cannot be used when the materialized view is not
//! populated`) - checked by `check_refresh_concurrently`.
//!
//! Column defaults on views (`ALTER VIEW v ALTER COLUMN c SET DEFAULT`, used by auto-updatable
//! views) go through `alter_column::set_column_default`, which accepts views.
use super::spec::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

impl<'a> CatalogTxn<'a> {
    /// CREATE [OR REPLACE] [MATERIALIZED] VIEW (module docs). `Ok(None)` only for matview
    /// `IF NOT EXISTS` skip.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_view(&mut self, spec: &ViewSpec) -> Result<Option<Oid>> {
        Err(PgError::unimplemented("CAT-17", "CREATE VIEW"))
    }

    /// REFRESH MATERIALIZED VIEW [WITH [NO] DATA] (catalog side, module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn refresh_matview(&mut self, matview: Oid, with_data: bool) -> Result<()> {
        Err(PgError::unimplemented("CAT-17", "REFRESH MATERIALIZED VIEW"))
    }

    /// Precondition checks for `REFRESH MATERIALIZED VIEW CONCURRENTLY` (module docs).
    // CONTRACT
    #[allow(unused_variables)]
    pub fn check_refresh_concurrently(&mut self, matview: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-17", "REFRESH CONCURRENTLY checks"))
    }

    /// ALTER VIEW v SET (opts) / RESET: `security_barrier`, `security_invoker`, `check_option`
    /// (`local|cascaded`; also mirrors `ViewDef::check_option`); 22023 `unrecognized parameter "x"`.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_view_options(&mut self, view: Oid, set: &[String], reset: &[String]) -> Result<()> {
        Err(PgError::unimplemented("CAT-17", "ALTER VIEW SET"))
    }
}
