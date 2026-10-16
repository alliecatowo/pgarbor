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
use super::common::MAX_COLUMNS;
use super::spec::*;
use crate::builtin;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::snapshot::ClassRef;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::context::TypeEnv;
use pgarbor_types::names;
use std::collections::HashSet;

/// PG `parse_bool`: case-insensitive, surrounding blanks ignored, unique prefixes accepted.
fn parse_pg_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" | "1" => Some(true),
        "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Split a `name=value` option; a bare `name` has no value.
fn split_option(opt: &str) -> (&str, Option<&str>) {
    match opt.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (opt, None),
    }
}

/// Validate one view option and return `(name, value)`. Booleans keep the text as written, like PG
/// reloptions (a bare name means `true`); `check_option` as `local`/`cascaded`.
/// Unknown names -> 22023 `unrecognized parameter`.
fn normalize_view_option(name: &str, value: Option<&str>) -> Result<(String, String)> {
    match name {
        "security_barrier" | "security_invoker" => {
            let raw = value.unwrap_or("true");
            match parse_pg_bool(raw) {
                Some(_) => Ok((name.to_string(), raw.to_string())),
                None => Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for boolean option \"{name}\": {raw}"),
                )),
            }
        }
        "check_option" => {
            let raw = value.unwrap_or("");
            let lower = raw.to_ascii_lowercase();
            if lower == "local" || lower == "cascaded" {
                Ok((name.to_string(), lower))
            } else {
                Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for enum option \"check_option\": {raw}"),
                )
                .with_detail("Valid values are \"local\" and \"cascaded\"."))
            }
        }
        _ => Err(errors::unrecognized_parameter(name)),
    }
}

/// Upsert `(name, value)`: an existing entry keeps its position.
fn upsert(entries: &mut Vec<(String, String)>, name: String, value: String) {
    if let Some(pos) = entries.iter().position(|(n, _)| n.as_str() == name.as_str()) {
        entries[pos].1 = value;
    } else {
        entries.push((name, value));
    }
}

/// Validate `WITH (...)` / `SET (...)` view options in the given order. A repeated name is
/// 22023 `parameter "x" specified more than once`.
fn parse_view_options(options: &[String]) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(options.len());
    for opt in options {
        let (name, value) = split_option(opt);
        if out.iter().any(|(n, _)| n.as_str() == name) {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" specified more than once"),
            ));
        }
        let (n, v) = normalize_view_option(name, value)?;
        out.push((n, v));
    }
    Ok(out)
}

/// The stored `reloptions` of a relation as `(name, value)` pairs (bare names read as `true`).
fn existing_entries(options: &[String]) -> Vec<(String, String)> {
    options
        .iter()
        .map(|o| {
            let (n, v) = split_option(o);
            (n.to_string(), v.unwrap_or("true").to_string())
        })
        .collect()
}

fn render_options(entries: &[(String, String)]) -> Vec<String> {
    entries.iter().map(|(n, v)| format!("{n}={v}")).collect()
}

/// `ViewDef::check_option` mirror of the `check_option` entry.
fn check_option_of(entries: &[(String, String)]) -> Option<String> {
    entries.iter().find(|(n, _)| n.as_str() == "check_option").map(|(_, v)| v.clone())
}

/// Options and check option of a new VIEW (matviews accept no options in this catalog).
fn view_options(spec: &ViewSpec) -> Result<(Vec<String>, Option<String>)> {
    if spec.materialized {
        if let Some(first) = spec.options.first() {
            let (name, _) = split_option(first);
            return Err(errors::unrecognized_parameter(name));
        }
        if spec.check_option.is_some() {
            return Err(errors::unrecognized_parameter("check_option"));
        }
        return Ok((Vec::new(), None));
    }
    let mut entries = parse_view_options(&spec.options)?;
    if let Some(co) = &spec.check_option {
        let (n, v) = normalize_view_option("check_option", Some(co.as_str()))?;
        upsert(&mut entries, n, v);
    }
    let check = check_option_of(&entries);
    Ok((render_options(&entries), check))
}

/// Output columns of a view as stored: attnum = position + 1, collation = the explicit one or the
/// type's default. Checks zero columns, the 1600 limit, duplicate names (42701) and pseudo-type
/// columns (42P16). System column names are allowed for views (PG `CheckAttributeNamesTypes`
/// skips them for `RELKIND_VIEW`).
fn view_columns(env: &CatalogEnv, columns: &[ColumnSpec]) -> Result<Vec<Column>> {
    if columns.is_empty() {
        return Err(PgError::new(SqlState::INVALID_TABLE_DEFINITION, "view must have at least one column"));
    }
    if columns.len() > MAX_COLUMNS {
        return Err(errors::too_many_columns());
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(columns.len());
    for c in columns {
        if !seen.insert(c.name.as_str()) {
            return Err(errors::column_specified_more_than_once(&c.name));
        }
    }
    let mut out = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let info = env.type_info(c.type_id.oid);
        if let Some(t) = &info {
            if t.typtype == b'p' {
                let type_name = names::display_name(env, c.type_id.oid);
                return Err(errors::column_has_pseudo_type(&c.name, &type_name));
            }
        }
        let collation = match c.collation {
            Some(coll) => coll,
            None => info.as_ref().map(|t| t.collation).unwrap_or(0),
        };
        let mut col = Column::new((i + 1) as i16, c.name.clone(), c.type_id.clone(), collation);
        col.ndims = c.ndims;
        out.push(col);
    }
    Ok(out)
}

/// Collation name for messages (builtin collations; others are shown by OID).
fn collation_label(oid: Oid) -> String {
    match builtin::builtin_collation(oid) {
        Some(c) => c.name.to_string(),
        None => oid.to_string(),
    }
}

/// PG `checkViewColumns`: `new` must start with `old` (same names, types, typmods, collations).
fn check_replace_columns(old: &[Column], new: &[Column], env: &CatalogEnv) -> Result<()> {
    if new.len() < old.len() {
        return Err(errors::cannot_drop_view_columns());
    }
    for (o, n) in old.iter().zip(new.iter()) {
        if o.name != n.name {
            return Err(errors::cannot_change_view_column_name(&o.name, &n.name));
        }
        if o.type_id != n.type_id {
            let from = names::format_type(env, o.type_id.oid, o.type_id.typmod)?;
            let to = names::format_type(env, n.type_id.oid, n.type_id.typmod)?;
            return Err(errors::cannot_change_view_column_type(&o.name, &from, &to));
        }
        if o.collation != n.collation {
            return Err(PgError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change collation of view column \"{}\" from \"{}\" to \"{}\"",
                    o.name,
                    collation_label(o.collation),
                    collation_label(n.collation)
                ),
            ));
        }
    }
    Ok(())
}

/// Kind check for materialized view operations. PG 18.6 (`RangeVarCallbackMaintainsTable` and
/// `ExecRefreshMatView`): a view or other non-table kind -> 42809 `is not a table or materialized
/// view`; a table -> 0A000 `is not a materialized view`.
fn matview_kind_check(rel: &Relation) -> Result<()> {
    match rel.kind {
        RelKind::MatView => Ok(()),
        RelKind::Table | RelKind::PartitionedTable => Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("\"{}\" is not a materialized view", rel.name),
        )),
        _ => Err(PgError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{}\" is not a table or materialized view", rel.name),
        )),
    }
}

/// PG `is_usable_unique_index`: unique, immediate, valid, no predicate, every key a plain column.
fn is_usable_unique_index(ix: &Index) -> bool {
    ix.unique
        && ix.immediate
        && ix.valid
        && ix.predicate.is_none()
        && !ix.keys.is_empty()
        && ix.keys.iter().all(|k| matches!(k.column, Some(a) if a > 0))
}

/// Identifier quoting for messages: plain lowercase identifiers stay bare, others are quoted.
fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Set `ViewDef::populated` of a materialized view.
fn set_populated(r: &mut Relation, populated: bool) -> Result<()> {
    match r.view.as_mut() {
        Some(v) => {
            v.populated = populated;
            Ok(())
        }
        None => Err(PgError::corruption(format!("materialized view \"{}\" has no definition", r.name))),
    }
}

impl<'a> CatalogTxn<'a> {
    /// CREATE [OR REPLACE] [MATERIALIZED] VIEW (module docs). `Ok(None)` only for matview
    /// `IF NOT EXISTS` skip.
    // CONTRACT
    pub fn create_view(&mut self, spec: &ViewSpec) -> Result<Option<Oid>> {
        self.check_creatable_namespace(spec.namespace, &spec.name)?;
        self.require_schema_create(spec.namespace)?;
        let env = self.env()?;

        // OR REPLACE applies to plain views only (the grammar has no CREATE OR REPLACE MATERIALIZED).
        if spec.or_replace && !spec.materialized {
            match env.snapshot().class_in_namespace(spec.namespace, &spec.name) {
                Some(ClassRef::Relation(existing)) => {
                    self.replace_view(spec, existing, &env)?;
                    return Ok(None);
                }
                Some(_) => return Err(errors::not_a(&spec.name, "view")),
                None => {}
            }
        }

        // PG: a matview's IF NOT EXISTS skip happens before its columns are analysed; a view's name
        // clash is raised after the column checks (`DefineVirtualRelation`).
        if spec.materialized && !self.check_new_relation_name(spec.namespace, &spec.name, spec.if_not_exists)? {
            return Ok(None);
        }
        let columns = view_columns(&env, &spec.columns)?;
        let (options, check_option) = view_options(spec)?;
        if !spec.materialized && !self.check_new_relation_name(spec.namespace, &spec.name, spec.if_not_exists)? {
            return Ok(None);
        }

        // Type-name clash (moves a clashing auto array type), then the array name must be chosen
        // against the post-clash catalog.
        self.resolve_type_name_clash(spec.namespace, &spec.name)?;
        let oid = self.new_oid()?;
        let rowtype = self.new_oid()?;
        let array_oid = self.new_oid()?;
        let snap = self.snapshot()?;
        let array_name = naming::choose_array_type_name(&snap, spec.namespace, &spec.name, &[])?;
        let array_type_name = if array_name == naming::array_type_name(&spec.name) { None } else { Some(array_name) };

        let kind = if spec.materialized { RelKind::MatView } else { RelKind::View };
        let relation = Relation {
            oid,
            name: spec.name.clone(),
            namespace: spec.namespace,
            kind,
            owner: spec.owner,
            persistence: Persistence::Permanent,
            columns,
            rowtype,
            array_type: array_oid,
            array_type_name,
            options,
            acl: None,
            replica_identity: if spec.materialized { ReplicaIdentity::Default } else { ReplicaIdentity::Nothing },
            row_security: false,
            force_row_security: false,
            policies: Vec::new(),
            rules: Vec::new(),
            view: Some(ViewDef {
                query: Self::bind_self_deps(&spec.query, oid),
                check_option,
                populated: !spec.materialized || spec.populated,
            }),
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        };
        self.put(CatalogObject::Relation(relation))?;
        if spec.materialized && spec.populated {
            self.follow_up(FollowUp::PopulateMatView { relation: oid });
        }
        self.record_created(ObjectAddress::relation(oid));
        Ok(Some(oid))
    }

    /// CREATE OR REPLACE VIEW over the existing view `oid` (module docs, step 4).
    fn replace_view(&mut self, spec: &ViewSpec, oid: Oid, env: &CatalogEnv) -> Result<()> {
        let snap = self.snapshot()?;
        let old = snap.relation(oid).ok_or_else(|| errors::cache_lookup_failed("relation", oid))?.clone();
        if old.kind != RelKind::View {
            return Err(if old.kind == RelKind::MatView {
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a view", old.name))
                    .with_hint("Use DROP MATERIALIZED VIEW to remove a materialized view.")
            } else {
                errors::not_a(&old.name, "view")
            });
        }
        self.require_owner(&ObjectAddress::relation(oid))?;

        let columns = view_columns(env, &spec.columns)?;
        check_replace_columns(&old.columns, &columns, env)?;

        // Options not given are kept (without the old check option, which is replaced).
        let mut entries = if spec.options.is_empty() && spec.check_option.is_none() {
            existing_entries(&old.options).into_iter().filter(|(n, _)| n.as_str() != "check_option").collect()
        } else {
            parse_view_options(&spec.options)?
        };
        if let Some(co) = &spec.check_option {
            let (n, v) = normalize_view_option("check_option", Some(co.as_str()))?;
            upsert(&mut entries, n, v);
        }
        let check_option = check_option_of(&entries);
        let options = render_options(&entries);
        let populated = old.view.as_ref().map(|v| v.populated).unwrap_or(true);
        let query = Self::bind_self_deps(&spec.query, oid);
        let added: Vec<Column> = columns[old.columns.len()..].to_vec();

        self.update_relation(oid, move |r| {
            r.columns.extend(added);
            r.options = options;
            r.view = Some(ViewDef { query, check_option, populated });
            Ok(())
        })
    }

    /// REFRESH MATERIALIZED VIEW [WITH [NO] DATA] (catalog side, module docs).
    // CONTRACT
    pub fn refresh_matview(&mut self, matview: Oid, with_data: bool) -> Result<()> {
        let snap = self.snapshot()?;
        let rel = snap.relation(matview).ok_or_else(|| errors::cache_lookup_failed("relation", matview))?;
        matview_kind_check(rel)?;
        let index_oids: Vec<Oid> = snap.indexes_of(matview).iter().map(|ix| ix.oid).collect();
        self.require_owner(&ObjectAddress::relation(matview))?;

        self.update_relation(matview, move |r| set_populated(r, with_data))?;
        if with_data {
            self.follow_up(FollowUp::PopulateMatView { relation: matview });
        } else {
            self.follow_up(FollowUp::DropRelationData { relation: matview });
            for ix in index_oids {
                self.follow_up(FollowUp::DropRelationData { relation: ix });
            }
        }
        Ok(())
    }

    /// Precondition checks for `REFRESH MATERIALIZED VIEW CONCURRENTLY` (module docs).
    // CONTRACT
    pub fn check_refresh_concurrently(&mut self, matview: Oid) -> Result<()> {
        let snap = self.snapshot()?;
        let rel = snap.relation(matview).ok_or_else(|| errors::cache_lookup_failed("relation", matview))?;
        matview_kind_check(rel)?;
        let populated = rel.view.as_ref().map(|v| v.populated).unwrap_or(false);
        if !populated {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "CONCURRENTLY cannot be used when the materialized view is not populated",
            ));
        }
        let has_unique = snap.indexes_of(matview).iter().any(|ix| is_usable_unique_index(*ix));
        if has_unique {
            return Ok(());
        }
        let ns = snap.namespace(rel.namespace).ok_or_else(|| errors::cache_lookup_failed("schema", rel.namespace))?;
        let qualified = format!("{}.{}", quote_ident(&ns.name), quote_ident(&rel.name));
        Err(PgError::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("cannot refresh materialized view \"{qualified}\" concurrently"),
        )
        .with_hint("Create a unique index with no WHERE clause on one or more columns of the materialized view."))
    }

    /// ALTER VIEW v SET (opts) / RESET: `security_barrier`, `security_invoker`, `check_option`
    /// (`local|cascaded`; also mirrors `ViewDef::check_option`); 22023 `unrecognized parameter "x"`.
    // CONTRACT
    pub fn set_view_options(&mut self, view: Oid, set: &[String], reset: &[String]) -> Result<()> {
        let snap = self.snapshot()?;
        let rel = snap.relation(view).ok_or_else(|| errors::cache_lookup_failed("relation", view))?;
        if rel.kind != RelKind::View {
            return Err(errors::not_a(&rel.name, "view"));
        }
        self.require_owner(&ObjectAddress::relation(view))?;

        let mut entries = existing_entries(&rel.options);
        for name in reset {
            match name.as_str() {
                "security_barrier" | "security_invoker" | "check_option" => {
                    entries.retain(|(n, _)| n.as_str() != name.as_str());
                }
                _ => return Err(errors::unrecognized_parameter(name)),
            }
        }
        for (n, v) in parse_view_options(set)? {
            upsert(&mut entries, n, v);
        }
        if set.is_empty() && reset.is_empty() {
            return Ok(());
        }
        let check_option = check_option_of(&entries);
        let options = render_options(&entries);
        self.update_relation(view, move |r| {
            r.options = options;
            match r.view.as_mut() {
                Some(v) => {
                    v.check_option = check_option;
                    Ok(())
                }
                None => Err(PgError::corruption(format!("view \"{}\" has no definition", r.name))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BOOTSTRAP_SUPERUSER, PUBLIC_NAMESPACE};
    use crate::testing;
    use pgarbor_types::oids;
    use pgarbor_types::TypeId;

    fn expect_err<T>(r: Result<T>) -> PgError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn view_spec(name: &str, columns: Vec<ColumnSpec>, or_replace: bool) -> ViewSpec {
        ViewSpec {
            namespace: PUBLIC_NAMESPACE,
            name: name.to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            columns,
            query: StoredExpr::new(" SELECT 1 AS a;"),
            or_replace,
            materialized: false,
            populated: true,
            options: Vec::new(),
            check_option: None,
            if_not_exists: false,
        }
    }

    fn int_col(name: &str) -> ColumnSpec {
        ColumnSpec::new(name, TypeId::INT4)
    }

    #[test]
    fn bool_parsing_follows_pg() {
        assert_eq!(parse_pg_bool("TRUE"), Some(true));
        assert_eq!(parse_pg_bool("of"), Some(false));
        assert_eq!(parse_pg_bool("yes"), Some(true));
        assert_eq!(parse_pg_bool("maybe"), None);
    }

    #[test]
    fn option_validation_messages() {
        let e = expect_err(normalize_view_option("security_barrier", Some("x")));
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "invalid value for boolean option \"security_barrier\": x");
        let e = expect_err(normalize_view_option("check_option", Some("bogus")));
        assert_eq!(e.message, "invalid value for enum option \"check_option\": bogus");
        assert_eq!(e.detail.as_deref(), Some("Valid values are \"local\" and \"cascaded\"."));
        let e = expect_err(normalize_view_option("fillfactor", Some("70")));
        assert_eq!(e.message, "unrecognized parameter \"fillfactor\"");
        let ok = normalize_view_option("security_invoker", None).ok();
        assert_eq!(ok, Some(("security_invoker".to_string(), "true".to_string())));
        let off = normalize_view_option("security_barrier", Some("off")).ok();
        assert_eq!(off, Some(("security_barrier".to_string(), "off".to_string())));
    }

    #[test]
    fn quoting_and_collation_labels() {
        assert_eq!(quote_ident("mv"), "mv");
        assert_eq!(quote_ident("My View"), "\"My View\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn zero_and_duplicate_columns_are_rejected() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let e = expect_err(testing::run(&mut kv, &env, |t| t.create_view(&view_spec("v", vec![], false)).map(|_| ())));
        assert_eq!(e.sqlstate.code(), "42P16");
        assert_eq!(e.message, "view must have at least one column");
        let dup = vec![int_col("a"), int_col("a")];
        let e = expect_err(testing::run(&mut kv, &env, |t| t.create_view(&view_spec("v", dup, false)).map(|_| ())));
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(e.message, "column \"a\" specified more than once");
        Ok(())
    }

    #[test]
    fn create_then_or_replace_appends_column_and_keeps_oid() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (res, env) = testing::run(&mut kv, &env, |t| {
            t.create_view(&view_spec("v", vec![int_col("a")], false)).map(|_| ())
        })?;
        let oid = match res.outcome.created.first() {
            Some(addr) => addr.oid,
            None => panic!("view was not recorded as created"),
        };
        let cols = vec![int_col("a"), int_col("c")];
        let (res2, _env2) = testing::run(&mut kv, &env, |t| t.create_view(&view_spec("v", cols, true)).map(|_| ()))?;
        let rel = match res2.snapshot.relation(oid) {
            Some(r) => r.clone(),
            None => panic!("view disappeared after replace"),
        };
        assert_eq!(rel.columns.len(), 2);
        assert_eq!(rel.columns[1].attnum, 2);
        assert_eq!(rel.columns[1].name, "c");
        Ok(())
    }

    #[test]
    fn or_replace_rename_fewer_and_type_change_errors() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (_, env) = testing::run(&mut kv, &env, |t| {
            t.create_view(&view_spec("zz_v", vec![int_col("a")], false)).map(|_| ())
        })?;
        let renamed = vec![int_col("b")];
        let e = expect_err(testing::run(&mut kv, &env, |t| t.create_view(&view_spec("zz_v", renamed, true)).map(|_| ())));
        assert_eq!(e.sqlstate.code(), "42P16");
        assert_eq!(e.message, "cannot change name of view column \"a\" to \"b\"");
        assert_eq!(
            e.hint.as_deref(),
            Some("Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead.")
        );
        let typed = vec![ColumnSpec::new("a", TypeId::of(oids::INT8))];
        let e = expect_err(testing::run(&mut kv, &env, |t| t.create_view(&view_spec("zz_v", typed, true)).map(|_| ())));
        assert_eq!(e.message, "cannot change data type of view column \"a\" from integer to bigint");
        let e = expect_err(testing::run(&mut kv, &env, |t| {
            t.create_view(&view_spec("zz_v", vec![], true)).map(|_| ())
        }));
        assert_eq!(e.sqlstate.code(), "42P16");
        Ok(())
    }

    #[test]
    fn or_replace_over_a_table_is_not_a_view() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let table = CreateTableSpec {
            namespace: PUBLIC_NAMESPACE,
            name: "t".to_string(),
            owner: BOOTSTRAP_SUPERUSER,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![int_col("a")],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
            extra: Default::default(),
        };
        let (_, env) = testing::run(&mut kv, &env, |t| t.create_table(&table).map(|_| ()))?;
        let e = expect_err(testing::run(&mut kv, &env, |t| {
            t.create_view(&view_spec("t", vec![int_col("a")], true)).map(|_| ())
        }));
        assert_eq!(e.sqlstate.code(), "42809");
        assert_eq!(e.message, "\"t\" is not a view");
        Ok(())
    }

    #[test]
    fn matview_create_queues_populate_and_no_data_refresh_drops_data() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut mv = view_spec("mv", vec![int_col("a")], false);
        mv.materialized = true;
        let (res, env) = testing::run(&mut kv, &env, |t| t.create_view(&mv).map(|_| ()))?;
        let oid = match res.outcome.created.first() {
            Some(addr) => addr.oid,
            None => panic!("matview was not recorded as created"),
        };
        assert!(res.outcome.follow_ups.contains(&FollowUp::PopulateMatView { relation: oid }));
        let (res2, _env2) = testing::run(&mut kv, &env, |t| t.refresh_matview(oid, false))?;
        assert!(res2.outcome.follow_ups.contains(&FollowUp::DropRelationData { relation: oid }));
        Ok(())
    }

    #[test]
    fn alter_view_rejects_unknown_option() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (res, env) = testing::run(&mut kv, &env, |t| {
            t.create_view(&view_spec("v", vec![int_col("a")], false)).map(|_| ())
        })?;
        let oid = match res.outcome.created.first() {
            Some(addr) => addr.oid,
            None => panic!("view was not recorded as created"),
        };
        let set = vec!["fillfactor=70".to_string()];
        let e = expect_err(testing::run(&mut kv, &env, |t| t.set_view_options(oid, &set, &[])));
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "unrecognized parameter \"fillfactor\"");
        Ok(())
    }
}
