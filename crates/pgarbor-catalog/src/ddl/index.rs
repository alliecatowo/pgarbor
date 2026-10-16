//! CREATE INDEX, ALTER INDEX (rename, options), index-name selection. Lease CAT-13. PG sources:
//! `commands/indexcmds.c` (`DefineIndex`, `ComputeIndexAttrs`, `ResolveOpClass`,
//! `GetDefaultOpClass`), `catalog/index.c` (`index_create`, `index_constraint_create`),
//! `commands/tablecmds.c` (`RenameRelation`).
//!
//! [`CatalogTxn::create_index`] is the user-facing CREATE INDEX; constraint-owned indexes (PRIMARY
//! KEY / UNIQUE) are created by `ddl::constraint` through the lower-level
//! [`CatalogTxn::create_index_internal`] which this file also owns.
//!
//! ## `create_index` checks (PG order), all 42xxx unless noted
//! - target must be a table or matview (partitioned later): a view/sequence/composite type ->
//!   42809 `cannot create index on relation "v"` DETAIL `This operation is not supported for views.`
//!   (`errors::operation_not_supported_for`; plural words from `RelKind`: views, sequences, composite
//!   types, indexes, foreign tables); system catalogs -> 42501 `permission denied: "pg_class" is a system catalog`;
//!   must be table owner (`require_owner`) - PG also allows MAINTAIN privilege holders? No: owner
//!   only for CREATE INDEX.
//! - method: only `btree` is implemented; `hash|gist|gin|brin|spgist` -> `Err(PgError::unimplemented(
//!   "CAT-13", "index access method <name>"))` (the binder maps unknown names to
//!   `errors::access_method_does_not_exist`, 42704).
//! - more than 32 key+include columns -> 54011 `errors::too_many_index_columns`.
//! - each key: column must exist (42703 `column "x" does not exist`, NOT the "named in key" wording
//!   used for constraints; dropped columns are not found; system columns resolve (`xmin` ->
//!   attnum -2 of type xid) and then fail on the opclass lookup below: oracle `create index on t(xmin)` ->
//!   42704 `data type xid has no default operator class for access method "btree"`); expression keys need `expr`/`expr_type`;
//!   `opclass` explicit or default btree opclass for the key type (family table:
//!   `builtin::OPCLASSES` rows with `opcdefault = true` and `opcintype`, binary-coercible matches:
//!   enums use `enum_ops` 3522, arrays `array_ops` 397, records `record_ops` 2994; domains use their
//!   base type's opclass) else 42704 `errors::no_default_opclass(type, "btree")`.
//!   Collation: explicit, else the column's collation; for a non-collatable type an explicit one is
//!   rejected: 42804 `collations are not supported by type integer` (oracle).
//! - INCLUDE columns: must exist (plain columns only). Overlap with key columns is ALLOWED (oracle:
//!   `create index on t(a) include (a)` creates `t_a_a1_idx`): PG derives index attribute names over
//!   key columns AND include columns together (`ChooseIndexColumnNames(allIndexParams)`), so pass
//!   all of them (`a`, `a1`) to `naming::choose_index_column_names` / `choose_index_name`.
//! - UNIQUE/`nulls_not_distinct` copied. Partial `predicate` stored as `StoredExpr`.
//! - name: explicit (must be free in the table's namespace: 42P07 `relation "i" already exists`, or
//!   `if_not_exists` NOTICE `relation "i" already exists, skipping` + `Ok(None)`), else
//!   `naming::choose_index_name` with `naming::choose_index_column_names` (e.g. `zz_p_a_idx`,
//!   `t_lower_idx`, `t_expr_expr1_idx`).
//! Stores `Index{oid, name, namespace = table's, table, owner = table owner, method, keys, n_key_columns,
//! unique, primary=false, immediate = true, valid = true, constraint = None, ...}` where each
//! `IndexKey.name` comes from `choose_index_column_names`, `type_id` from the column (typmod kept) or
//! `expr_type`, `opclass`/`collation` resolved, `nulls_first` = explicit or `descending` (PG
//! default NULLS FIRST iff DESC). Queues `FollowUp::BuildIndex{index, table}` (the table may hold
//! rows) and records `created`.
//!
//! ## `rename_index`
//! ALTER INDEX i RENAME TO j: owner of the table; new name free (42P07); the index must not be
//! constraint-owned? PG allows renaming a constraint index and ALSO renames the constraint
//! (`index_rename` -> `RenameConstraintById` when `indisunique`... the constraint is renamed too).
//! Implement: if `Index::constraint` is Some, rename the constraint to the same name.
use super::common::{system_column_attnum, SYSTEM_COLUMNS};
use super::spec::*;
use crate::builtin;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::naming::{self, IndexKeySource};
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::names::format_type;
use pgarbor_types::{TypeEnv, TypeId};

// pg_opclass row layout (`builtin::OPCLASSES`, catalog column order = attnum - 1).
const OPC_OID: usize = 0;
const OPC_METHOD: usize = 1;
const OPC_INTYPE: usize = 6;
const OPC_DEFAULT: usize = 7;

/// pg_am OID of btree in the reference data.
const BTREE_AM: Oid = 403;
/// Pseudo-type OIDs whose default opclass serves every enum / array / composite type (PG
/// `GetDefaultOpClass` binary-coercible matches): `anyenum`, `anyarray`, `anyrecord`.
const ANYENUM: Oid = 3500;
const ANYARRAY: Oid = 2277;
const ANYRECORD: Oid = 2249;
const TEXT: Oid = 25;
const VARCHAR: Oid = 1043;
/// Type OIDs of the system columns (`tid`, `xid`, `cid`, `oid`).
const TID: Oid = 27;
const XID: Oid = 28;
const CID: Oid = 29;
const OID_TYPE: Oid = 26;
/// PG INDEX_MAX_KEYS.
const INDEX_MAX_KEYS: usize = 32;

/// A column of the indexed table as the index sees it.
struct ColRef {
    attnum: i16,
    name: String,
    type_id: TypeId,
    collation: Oid,
}

/// One resolved index attribute before its name is chosen.
struct PreKey {
    column: Option<i16>,
    expr: Option<StoredExpr>,
    type_id: TypeId,
    collation: Oid,
    opclass: Oid,
    descending: bool,
    nulls_first: bool,
    source: IndexKeySource,
}

impl<'a> CatalogTxn<'a> {
    /// CREATE [UNIQUE] INDEX (module docs). `Ok(None)` for `IF NOT EXISTS` skip.
    // CONTRACT
    pub fn create_index(&mut self, spec: &IndexSpec) -> Result<Option<Oid>> {
        let env = self.env()?;
        let rel = relation_of(&env, spec.table)?;
        match rel.kind {
            RelKind::Table | RelKind::MatView => {}
            RelKind::PartitionedTable => {
                return Err(PgError::unimplemented("CAT-13", "CREATE INDEX on a partitioned table"));
            }
            other => {
                return Err(errors::operation_not_supported_for(
                    &format!("cannot create index on relation \"{}\"", rel.name),
                    relkind_plural(other),
                ));
            }
        }
        if rel.is_virtual() {
            return Err(errors::system_catalog_permission(&rel.name));
        }
        self.require_owner(&ObjectAddress::relation(rel.oid))?;
        if spec.method != IndexMethod::Btree {
            return Err(PgError::unimplemented("CAT-13", format!("index access method {}", spec.method.name())));
        }
        if spec.keys.is_empty() {
            return Err(PgError::internal("CREATE INDEX without key columns"));
        }
        if spec.keys.len() + spec.include.len() > INDEX_MAX_KEYS {
            return Err(errors::too_many_index_columns());
        }
        let options = validate_btree_options(&spec.options)?;
        let (keys, n_key) = resolve_index_keys(&env, &rel, spec)?;
        let snap = env.snapshot();
        let name = match &spec.name {
            Some(n) => {
                if snap.class_in_namespace(rel.namespace, n).is_some() {
                    if spec.if_not_exists {
                        self.notice(errors::relation_already_exists_skipping(n));
                        return Ok(None);
                    }
                    return Err(errors::relation_already_exists(n));
                }
                n.clone()
            }
            None => {
                let cols: Vec<String> = keys.iter().map(|k| k.name.clone()).collect();
                naming::choose_index_name(snap, rel.namespace, &rel.name, &cols, false, false, &[])?
            }
        };
        let oid = self.write_index(&rel, spec, name, keys, n_key, None, false, true, options)?;
        self.follow_up(FollowUp::BuildIndex { index: oid, table: rel.oid });
        self.record_created(ObjectAddress::relation(oid));
        Ok(Some(oid))
    }

    /// Lower-level index creation shared with constraints: `spec` is a fully validated
    /// `IndexSpec` whose `name` is `Some`; `constraint` links the owning constraint OID;
    /// `primary` marks `indisprimary`; `immediate` is `indimmediate`. Performs the key resolution
    /// of `create_index` (opclass, collation, key names) without the user-facing checks (owner,
    /// access method, IF NOT EXISTS) and without queueing `BuildIndex`.
    // CONTRACT
    pub fn create_index_internal(
        &mut self,
        spec: &IndexSpec,
        constraint: Option<Oid>,
        primary: bool,
        immediate: bool,
    ) -> Result<Oid> {
        let env = self.env()?;
        let rel = relation_of(&env, spec.table)?;
        let name = spec.name.clone().ok_or_else(|| PgError::internal("create_index_internal requires an index name"))?;
        let options = validate_btree_options(&spec.options)?;
        let (keys, n_key) = resolve_index_keys(&env, &rel, spec)?;
        if env.snapshot().class_in_namespace(rel.namespace, &name).is_some() {
            return Err(errors::relation_already_exists(&name));
        }
        self.write_index(&rel, spec, name, keys, n_key, constraint, primary, immediate, options)
    }

    /// ALTER INDEX `index` RENAME TO `new_name` (module docs).
    // CONTRACT
    pub fn rename_index(&mut self, index: Oid, new_name: &str) -> Result<()> {
        let env = self.env()?;
        let idx = index_of(&env, index)?;
        self.require_owner(&ObjectAddress::relation(index))?;
        if env.snapshot().class_in_namespace(idx.namespace, new_name).is_some() {
            return Err(errors::relation_already_exists(new_name));
        }
        self.update_index(index, |i| {
            i.name = new_name.to_string();
            Ok(())
        })?;
        if let Some(con) = idx.constraint {
            self.update_constraint(con, |c| {
                c.name = new_name.to_string();
                Ok(())
            })?;
        }
        Ok(())
    }

    /// ALTER INDEX `index` SET (opts) / RESET: replaces `Index::options` entries (`name=value`).
    /// Unknown option names for btree -> 22023 `unrecognized parameter "x"`; known: `fillfactor`
    /// (10..100, 22023 `value 5 out of bounds for option "fillfactor"` DETAIL `Valid values are
    /// between "10" and "100".`), `deduplicate_items` (bool), `vacuum_cleanup_index_scale_factor`
    /// (accepted). `reset` removes the named options.
    // CONTRACT
    pub fn alter_index_options(&mut self, index: Oid, set: &[String], reset: &[String]) -> Result<()> {
        let env = self.env()?;
        let idx = index_of(&env, index)?;
        self.require_owner(&ObjectAddress::relation(index))?;
        let new_set = validate_btree_options(set)?;
        let mut opts = idx.options.clone();
        // RESET names are not validated (PG `ATExecSetRelOptions` drops them as given): a name that
        // is not a parameter of this index simply matches nothing.
        for r in reset {
            let (name, _) = split_option(r);
            opts.retain(|o| option_name(o) != name);
        }
        for s in new_set {
            let name = option_name(&s).to_string();
            opts.retain(|o| option_name(o) != name);
            opts.push(s);
        }
        let validated = validate_btree_options(&opts)?;
        self.update_index(index, |i| {
            i.options = validated;
            Ok(())
        })
    }

    /// The default btree operator class OID for `type_oid` (domain -> base type, enum -> `enum_ops`,
    /// arrays -> `array_ops`, composite -> `record_ops`), or None. Reads `builtin::OPCLASSES`.
    // CONTRACT
    pub fn default_btree_opclass(&mut self, type_oid: Oid) -> Result<Option<Oid>> {
        let env = self.env()?;
        Ok(default_btree_opclass_in(&env, type_oid))
    }

    /// Re-derive the `IndexKey` list entry for plain column `attnum` of `table` (type, collation,
    /// default opclass, name = column name).
    pub(crate) fn plain_index_key(&mut self, table: Oid, attnum: i16) -> Result<IndexKey> {
        let env = self.env()?;
        let rel = relation_of(&env, table)?;
        let (name, type_id, collation) = if attnum < 0 {
            let name = SYSTEM_COLUMNS
                .iter()
                .find(|(_, a)| *a == attnum)
                .map(|(n, _)| (*n).to_string())
                .ok_or_else(|| PgError::internal(format!("cache lookup failed for attribute {attnum}")))?;
            let ty = TypeId::of(system_column_type_oid(&name));
            (name, ty, 0)
        } else {
            let c = rel
                .column(attnum)
                .filter(|c| c.is_visible())
                .ok_or_else(|| PgError::internal(format!("cache lookup failed for attribute {attnum} of relation {table}")))?;
            (c.name.clone(), c.type_id, c.collation)
        };
        let opclass = btree_opclass_for(&env, type_id)?;
        Ok(IndexKey {
            column: Some(attnum),
            expr: None,
            name,
            type_id,
            collation,
            opclass,
            descending: false,
            nulls_first: false,
        })
    }

    /// Writes the `Index` object (`pg_class` relkind 'i' + `pg_index`) and returns its OID.
    #[allow(clippy::too_many_arguments)]
    fn write_index(
        &mut self,
        rel: &Relation,
        spec: &IndexSpec,
        name: String,
        keys: Vec<IndexKey>,
        n_key: u16,
        constraint: Option<Oid>,
        primary: bool,
        immediate: bool,
        options: Vec<String>,
    ) -> Result<Oid> {
        let oid = self.new_oid()?;
        let index = Index {
            oid,
            name,
            namespace: rel.namespace,
            table: rel.oid,
            owner: rel.owner,
            method: spec.method,
            keys,
            n_key_columns: n_key,
            unique: spec.unique || primary,
            nulls_not_distinct: spec.nulls_not_distinct,
            primary,
            exclusion: false,
            immediate,
            clustered: false,
            valid: true,
            replident: false,
            predicate: spec.predicate.clone(),
            constraint,
            options,
            persistence: rel.persistence,
        };
        self.put(CatalogObject::Index(index))?;
        Ok(oid)
    }
}

/// The relation a DDL operation targets (virtual system relations included).
fn relation_of(env: &CatalogEnv, oid: Oid) -> Result<Relation> {
    env.snapshot().relation(oid).cloned().ok_or_else(|| errors::cache_lookup_failed("relation", oid))
}

/// The index for ALTER INDEX: a user index, or 42501 for a compiled-in system index.
fn index_of(env: &CatalogEnv, index: Oid) -> Result<Index> {
    if let Some(i) = env.snapshot().index(index) {
        return Ok(i.clone());
    }
    if let Some(sr) = builtin::system_rel(index) {
        return Err(errors::system_catalog_permission(sr.name));
    }
    Err(errors::cache_lookup_failed("index", index))
}

/// PG `errdetail_relkind_not_supported` plural words.
fn relkind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table | RelKind::PartitionedTable => "tables",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::Toast => "TOAST tables",
    }
}

/// `format_type_be`-style name (no typmod) for error messages.
fn type_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    format_type(env, oid, -1)
}

/// Type OID of a system column by name (PG `SystemAttributeDefinition`).
fn system_column_type_oid(name: &str) -> Oid {
    match name {
        "ctid" => TID,
        "xmin" | "xmax" => XID,
        "cmin" | "cmax" => CID,
        _ => OID_TYPE,
    }
}

/// A plain (non-system) visible column by name, for INCLUDE.
fn plain_column(rel: &Relation, name: &str) -> Result<ColRef> {
    match rel.column_by_name(name) {
        Some(c) => Ok(ColRef { attnum: c.attnum, name: c.name.clone(), type_id: c.type_id, collation: c.collation }),
        None => Err(errors::column_does_not_exist(name)),
    }
}

/// A key column by name: user columns first, then system columns of tables (`xmin` -> attnum -2).
fn key_column(rel: &Relation, name: &str) -> Result<ColRef> {
    if let Some(c) = rel.column_by_name(name) {
        return Ok(ColRef { attnum: c.attnum, name: c.name.clone(), type_id: c.type_id, collation: c.collation });
    }
    if rel.kind.has_system_columns() {
        if let Some(attnum) = system_column_attnum(name) {
            return Ok(ColRef {
                attnum,
                name: name.to_string(),
                type_id: TypeId::of(system_column_type_oid(name)),
                collation: 0,
            });
        }
    }
    Err(errors::column_does_not_exist(name))
}

/// PG `type_is_collatable` check for an explicit COLLATE on an index key.
fn check_collatable(env: &CatalogEnv, ty: TypeId) -> Result<()> {
    if env.type_info(ty.oid).is_some_and(|i| i.collation != 0) {
        Ok(())
    } else {
        Err(PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("collations are not supported by type {}", type_name(env, ty.oid)?),
        ))
    }
}

/// Collation of a type's default (0 for non-collatable types).
fn type_default_collation(env: &CatalogEnv, oid: Oid) -> Oid {
    env.type_info(oid).map(|i| i.collation).unwrap_or(0)
}

/// Domain chain -> base type (PG `getBaseType`).
fn base_type_oid(env: &CatalogEnv, mut oid: Oid) -> Oid {
    for _ in 0..64 {
        match env.type_info(oid) {
            Some(info) if info.typtype == b'd' && info.basetype != 0 => oid = info.basetype,
            _ => break,
        }
    }
    oid
}

/// The btree default opclass whose input type is exactly `intype`.
fn find_default_btree(intype: Oid) -> Option<Oid> {
    builtin::OPCLASSES.rows.iter().find_map(|row| {
        let cell = |i: usize| row.get(i).copied().flatten();
        if cell(OPC_METHOD)?.parse::<Oid>().ok()? != BTREE_AM {
            return None;
        }
        if cell(OPC_DEFAULT)? != "t" {
            return None;
        }
        if cell(OPC_INTYPE)?.parse::<Oid>().ok()? != intype {
            return None;
        }
        cell(OPC_OID)?.parse::<Oid>().ok()
    })
}

/// PG `GetDefaultOpClass` for btree: exact default opclass of the (domain-stripped) type, else the
/// pseudo-type opclass that the type binary-coerces to (enum, array, composite), else `varchar` ->
/// `text_ops`.
fn default_btree_opclass_in(env: &CatalogEnv, type_oid: Oid) -> Option<Oid> {
    let base = base_type_oid(env, type_oid);
    if let Some(oc) = find_default_btree(base) {
        return Some(oc);
    }
    if let Some(info) = env.type_info(base) {
        if info.typtype == b'e' {
            return find_default_btree(ANYENUM);
        }
        if info.elem != 0 {
            return find_default_btree(ANYARRAY);
        }
        if info.typtype == b'c' {
            return find_default_btree(ANYRECORD);
        }
    }
    if base == VARCHAR {
        return find_default_btree(TEXT);
    }
    None
}

/// Default btree opclass or 42704 `data type X has no default operator class for access method "btree"`.
fn btree_opclass_for(env: &CatalogEnv, ty: TypeId) -> Result<Oid> {
    match default_btree_opclass_in(env, ty.oid) {
        Some(oc) => Ok(oc),
        None => Err(errors::no_default_opclass(&type_name(env, ty.oid)?, "btree")),
    }
}

/// PG `ComputeIndexAttrs` for one expression or column key (collation, opclass, sort options).
fn resolve_key(env: &CatalogEnv, rel: &Relation, ks: &IndexKeySpec) -> Result<PreKey> {
    let descending = ks.descending;
    let nulls_first = ks.nulls_first.unwrap_or(descending);
    if let Some(col) = &ks.column {
        let c = key_column(rel, col)?;
        let collation = match ks.collation {
            Some(explicit) => {
                check_collatable(env, c.type_id)?;
                explicit
            }
            None => c.collation,
        };
        let opclass = match ks.opclass {
            Some(oc) => oc,
            None => btree_opclass_for(env, c.type_id)?,
        };
        return Ok(PreKey {
            column: Some(c.attnum),
            expr: None,
            type_id: c.type_id,
            collation,
            opclass,
            descending,
            nulls_first,
            source: IndexKeySource::Column(c.name),
        });
    }
    if let Some(expr) = &ks.expr {
        let ty = TypeId::of(
            ks.expr_type
                .ok_or_else(|| PgError::internal("expression index key without a type"))?
                .oid,
        );
        let collation = match ks.collation {
            Some(explicit) => {
                check_collatable(env, ty)?;
                explicit
            }
            None => type_default_collation(env, ty.oid),
        };
        let opclass = match ks.opclass {
            Some(oc) => oc,
            None => btree_opclass_for(env, ty)?,
        };
        let source = match &ks.expr_func_name {
            Some(f) => IndexKeySource::FunctionCall(f.clone()),
            None => IndexKeySource::Expression,
        };
        return Ok(PreKey {
            column: None,
            expr: Some(expr.clone()),
            type_id: ty,
            collation,
            opclass,
            descending,
            nulls_first,
            source,
        });
    }
    Err(PgError::internal("index key has neither a column nor an expression"))
}

/// All index attributes (keys, then INCLUDE columns) with their names (PG `ChooseIndexColumnNames`
/// over `allIndexParams`). Returns the attributes and the key-column count (`indnkeyatts`).
fn resolve_index_keys(env: &CatalogEnv, rel: &Relation, spec: &IndexSpec) -> Result<(Vec<IndexKey>, u16)> {
    if spec.keys.len() + spec.include.len() > INDEX_MAX_KEYS {
        return Err(errors::too_many_index_columns());
    }
    let mut pre: Vec<PreKey> = Vec::with_capacity(spec.keys.len() + spec.include.len());
    for ks in &spec.keys {
        pre.push(resolve_key(env, rel, ks)?);
    }
    for name in &spec.include {
        let c = plain_column(rel, name)?;
        pre.push(PreKey {
            column: Some(c.attnum),
            expr: None,
            type_id: c.type_id,
            collation: c.collation,
            opclass: 0,
            descending: false,
            nulls_first: false,
            source: IndexKeySource::Column(c.name),
        });
    }
    let sources: Vec<IndexKeySource> = pre.iter().map(|p| p.source.clone()).collect();
    let names = naming::choose_index_column_names(&sources)?;
    if names.len() != pre.len() {
        return Err(PgError::internal("index attribute name count mismatch"));
    }
    let n_key = spec.keys.len() as u16;
    let keys = pre
        .into_iter()
        .zip(names)
        .map(|(p, name)| IndexKey {
            column: p.column,
            expr: p.expr,
            name,
            type_id: p.type_id,
            collation: p.collation,
            opclass: p.opclass,
            descending: p.descending,
            nulls_first: p.nulls_first,
        })
        .collect();
    Ok((keys, n_key))
}

// ---------------------------------------------------------------- btree reloptions

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptKind {
    Int,
    Bool,
    Real,
}

fn btree_option_kind(name: &str) -> Option<OptKind> {
    match name {
        "fillfactor" => Some(OptKind::Int),
        "deduplicate_items" => Some(OptKind::Bool),
        "vacuum_cleanup_index_scale_factor" => Some(OptKind::Real),
        _ => None,
    }
}

/// `name=value` -> (name, value); a bare name has no value.
fn split_option(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once('=') {
        Some((n, v)) => (n.trim(), Some(v.trim())),
        None => (raw.trim(), None),
    }
}

fn option_name(raw: &str) -> &str {
    split_option(raw).0
}

/// PG `parse_bool` (case-insensitive; accepts the usual spellings and unambiguous prefixes).
fn parse_bool_option(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "t" | "true" | "tr" | "tru" | "y" | "ye" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "fa" | "fal" | "fals" | "n" | "no" | "of" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Validates btree reloptions (PG `btoptions` via `index_reloptions`) and returns them normalized as
/// `name=value`. Unknown names -> 22023 `unrecognized parameter`; bad values -> 22023; duplicates ->
/// 22023 `parameter "x" specified more than once`.
fn validate_btree_options(opts: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(opts.len());
    let mut seen: Vec<&str> = Vec::with_capacity(opts.len());
    for raw in opts {
        let (name, value) = split_option(raw);
        let kind = btree_option_kind(name).ok_or_else(|| errors::unrecognized_parameter(name))?;
        if seen.contains(&name) {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("parameter \"{name}\" specified more than once"),
            ));
        }
        seen.push(name);
        let normalized = match kind {
            OptKind::Bool => {
                let b = match value {
                    None => true,
                    Some(v) => parse_bool_option(v).ok_or_else(|| {
                        PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("invalid value for boolean option \"{name}\": {v}"),
                        )
                    })?,
                };
                format!("{name}={b}")
            }
            OptKind::Int => {
                let v = value.ok_or_else(|| requires_value(name))?;
                let n: i64 = v.parse().map_err(|_| {
                    PgError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("invalid value for integer option \"{name}\": {v}"),
                    )
                })?;
                if !(10..=100).contains(&n) {
                    return Err(errors::option_out_of_bounds(&n.to_string(), name, "10", "100"));
                }
                format!("{name}={n}")
            }
            OptKind::Real => {
                let v = value.ok_or_else(|| requires_value(name))?;
                let x: f64 = v
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite())
                    .ok_or_else(|| {
                        PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("invalid value for floating point option \"{name}\": {v}"),
                        )
                    })?;
                if !(0.0..=1e10).contains(&x) {
                    return Err(errors::option_out_of_bounds(
                        &x.to_string(),
                        name,
                        "0.000000",
                        "10000000000.000000",
                    ));
                }
                format!("{name}={v}")
            }
        };
        out.push(normalized);
    }
    Ok(out)
}

fn requires_value(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("parameter \"{name}\" requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::spec::{ColumnSpec, CreateTableSpec};
    use pgarbor_common::MemKv;

    use crate::testing;

    fn key(col: &str) -> IndexKeySpec {
        IndexKeySpec {
            column: Some(col.to_string()),
            expr: None,
            expr_func_name: None,
            expr_type: None,
            opclass: None,
            collation: None,
            descending: false,
            nulls_first: None,
        }
    }

    fn index_spec(table: Oid, keys: Vec<IndexKeySpec>) -> IndexSpec {
        IndexSpec {
            table,
            name: None,
            unique: false,
            nulls_not_distinct: false,
            method: IndexMethod::Btree,
            keys,
            include: Vec::new(),
            predicate: None,
            options: Vec::new(),
            if_not_exists: false,
            concurrently: false,
        }
    }

    fn table_spec() -> CreateTableSpec {
        CreateTableSpec {
            namespace: 2200,
            name: "t".to_string(),
            owner: 10,
            kind: RelKind::Table,
            persistence: Persistence::Permanent,
            columns: vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("b", TypeId::TEXT)],
            constraints: Vec::new(),
            options: Vec::new(),
            if_not_exists: false,
            inherits: Vec::new(),
        }
    }

    /// Bootstrapped catalog with table `public.t(a int4, b text)`; returns the store, the next env and
    /// the table OID.
    fn setup() -> Result<(MemKv, CatalogEnv, Oid)> {
        let (mut kv, env) = testing::bootstrapped()?;
        let mut created: Option<Oid> = None;
        let spec = table_spec();
        let (_res, env) = testing::run(&mut kv, &env, |t| {
            created = t.create_table(&spec)?.map(|c| c.table);
            Ok(())
        })?;
        Ok((kv, env, created.expect("table oid")))
    }

    #[test]
    fn default_opclass_lookup() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        testing::run(&mut kv, &env, |t| {
            assert_eq!(t.default_btree_opclass(23)?, Some(1978));
            assert_eq!(t.default_btree_opclass(25)?, Some(3126));
            assert_eq!(t.default_btree_opclass(1043)?, Some(3126));
            assert_eq!(t.default_btree_opclass(28)?, None);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn create_index_names_and_keys() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;
        let spec = index_spec(tbl, vec![key("a")]);
        let mut out: Option<Oid> = None;
        let (res, env) = testing::run(&mut kv, &env, |t| {
            out = t.create_index(&spec)?;
            Ok(())
        })?;
        let oid = out.expect("index oid");
        let idx = res.snapshot.index(oid).expect("index in snapshot");
        assert_eq!(idx.name, "t_a_idx");
        assert_eq!(idx.keys[0].opclass, 1978);
        assert_eq!(idx.keys[0].collation, 0);
        assert!(res.outcome.follow_ups.contains(&FollowUp::BuildIndex { index: oid, table: tbl }));

        // A second identical index gets the numeric suffix.
        let (res2, env) = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))?;
        assert_eq!(res2.snapshot.indexes_of(tbl).len(), 2);
        assert!(res2.snapshot.indexes_of(tbl).iter().any(|i| i.name == "t_a_idx1"));
        let _ = env;
        Ok(())
    }

    #[test]
    fn text_key_gets_collation_and_opclass() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;
        let spec = index_spec(tbl, vec![key("b")]);
        let (res, _env) = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))?;
        let idx = res.snapshot.indexes_of(tbl)[0];
        assert_eq!(idx.name, "t_b_idx");
        assert_eq!(idx.keys[0].opclass, 3126);
        assert_eq!(idx.keys[0].collation, 100);
        Ok(())
    }

    #[test]
    fn descending_nulls_last_indoption() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;
        let mut k = key("a");
        k.descending = true;
        k.nulls_first = Some(false);
        let spec = index_spec(tbl, vec![k]);
        let (res, _env) = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))?;
        assert_eq!(res.snapshot.indexes_of(tbl)[0].keys[0].indoption(), 1);
        Ok(())
    }

    #[test]
    fn include_column_naming() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;
        let mut spec = index_spec(tbl, vec![key("a")]);
        spec.include = vec!["a".to_string()];
        let (res, _env) = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))?;
        assert_eq!(res.snapshot.indexes_of(tbl)[0].name, "t_a_a1_idx");
        Ok(())
    }

    #[test]
    fn create_index_errors() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;

        let err = testing::run(&mut kv, &env, |t| t.create_index(&index_spec(tbl, vec![key("nosuch")])).map(|_| ()))
            .err()
            .expect("unknown column must fail");
        assert_eq!(err.sqlstate.code(), "42703");

        let err = testing::run(&mut kv, &env, |t| t.create_index(&index_spec(tbl, vec![key("xmin")])).map(|_| ()))
            .err()
            .expect("xid has no btree opclass");
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "data type xid has no default operator class for access method \"btree\"");

        let mut k = key("a");
        k.collation = Some(100);
        let err = testing::run(&mut kv, &env, |t| t.create_index(&index_spec(tbl, vec![k])).map(|_| ()))
            .err()
            .expect("collation on integer must fail");
        assert_eq!(err.sqlstate.code(), "42804");
        assert_eq!(err.message, "collations are not supported by type integer");

        let mut spec = index_spec(tbl, vec![key("a")]);
        spec.method = IndexMethod::Hash;
        let err = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))
            .err()
            .expect("hash is not implemented");
        assert!(err.is_unimplemented());
        assert_eq!(err.sqlstate.code(), "0A000");

        let mut spec = index_spec(tbl, vec![key("a")]);
        spec.options = vec!["fillfactor=5".to_string()];
        let err = testing::run(&mut kv, &env, |t| t.create_index(&spec).map(|_| ()))
            .err()
            .expect("fillfactor out of range");
        assert_eq!(err.sqlstate.code(), "22023");
        assert_eq!(err.message, "value 5 out of bounds for option \"fillfactor\"");
        assert_eq!(err.detail.as_deref(), Some("Valid values are between \"10\" and \"100\"."));
        Ok(())
    }

    #[test]
    fn btree_option_validation() {
        let ok = validate_btree_options(&["fillfactor=70".to_string()]).ok().expect("valid");
        assert_eq!(ok, vec!["fillfactor=70".to_string()]);
        let bare = validate_btree_options(&["deduplicate_items".to_string()]).ok().expect("bare bool");
        assert_eq!(bare, vec!["deduplicate_items=true".to_string()]);
        let e = validate_btree_options(&["deduplicate_items=maybe".to_string()]).err().expect("bad bool");
        assert_eq!(e.sqlstate.code(), "22023");
        let e = validate_btree_options(&["nope=1".to_string()]).err().expect("unknown");
        assert_eq!(e.message, "unrecognized parameter \"nope\"");
        let e = validate_btree_options(&["fillfactor=5".to_string()]).err().expect("range");
        assert_eq!(e.detail.as_deref(), Some("Valid values are between \"10\" and \"100\"."));
    }

    #[test]
    fn rename_and_alter_options() -> Result<()> {
        let (mut kv, env, tbl) = setup()?;
        let spec = index_spec(tbl, vec![key("a")]);
        let mut out: Option<Oid> = None;
        let (_res, env) = testing::run(&mut kv, &env, |t| {
            out = t.create_index(&spec)?;
            Ok(())
        })?;
        let oid = out.expect("index oid");
        let set = vec!["fillfactor=80".to_string()];
        let (res, env) = testing::run(&mut kv, &env, |t| t.alter_index_options(oid, &set, &[]))?;
        assert_eq!(res.snapshot.index(oid).expect("index").options, vec!["fillfactor=80".to_string()]);
        let (res, _env) = testing::run(&mut kv, &env, |t| t.rename_index(oid, "t_a_renamed"))?;
        assert_eq!(res.snapshot.index(oid).expect("index").name, "t_a_renamed");
        Ok(())
    }
}
