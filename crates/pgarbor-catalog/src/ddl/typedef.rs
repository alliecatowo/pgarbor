//! User-defined types: enums, domains, composite types. Lease CAT-18. PG sources:
//! `commands/typecmds.c` (`DefineEnum`, `DefineDomain`, `AlterDomain*`, `DefineCompositeType`,
//! `AlterEnum`, `RenameType`, `AlterTypeNamespace`, `AlterTypeOwner`), `catalog/pg_enum.c`
//! (`EnumValuesCreate`, `AddEnumLabel`, `RenameEnumLabel`, `RenumberEnumType`), `catalog/pg_type.c`
//! (`TypeCreate`, `moveArrayTypeName`), `commands/tablecmds.c` (`DefineRelation` for composites).
//!
//! Type name space: types, domains, enums, relation rowtypes and ALL array types share one
//! (namespace, typname) space -> `ddl::common::resolve_type_name_clash` before creating
//! (42710 `type "x" already exists`; an auto array type in the way is renamed, oracle-verified).
//! Every type gets an array type (`TypeObj::array_type`, name `naming::choose_array_type_name`
//! stored in `array_type_name` only when non-canonical).
//!
//! ## Enums
//! `create_enum`: label validation - each label at most 63 bytes (42602
//! `errors::invalid_enum_label`, DETAIL `Labels must be 63 bytes or less.`); EMPTY labels are accepted (oracle: `create type e as enum ('')`
//! works); duplicate labels in the list -> 23505 `duplicate key value violates unique constraint
//! "pg_enum_typid_label_index"` DETAIL `Key (enumtypid, enumlabel)=(<type oid>, a) already exists.`
//! (PG relies on the unique index; replicate exactly). `sort_order` = 1, 2, 3, ... as f32; each label
//! gets its own `new_oid`.
//! `add_enum_value(type, label, position, if_not_exists)`: type must be an enum (42809 `"t" is not
//! an enum`: `errors::not_an_enum`), label length rule, existing label -> 42710
//! `errors::enum_label_already_exists` (NOTICE `..., skipping` + `Ok(false)` with `if_not_exists`);
//! position `Append` -> `max(sort_order) + 1` (1 when the enum is empty); `Before(x)`/`After(x)`: x must
//! exist (22023 `errors::not_an_existing_enum_label`); PG `AddEnumLabel`: let `nbr` be x's index and
//! `other` the adjacent label on the requested side; no `other` below -> `nbr.sort_order - 1`; no
//! `other` above -> `nbr.sort_order + 1`; otherwise the f32 midpoint `(nbr + other) / 2` computed in
//! f32; if the midpoint equals either neighbour (precision exhausted) renumber ALL labels 1.0..n
//! (`RenumberEnumType`) and retry. Oracle: labels `''(1), a(2)`, `add value 'z' before 'a'` ->
//! sort order 1.5; `add value 'ab' after 'a'` (a last) -> 3. Labels are kept sorted by `sort_order`.
//! `rename_enum_value(type, old, new)`: old must exist (22023 `"x" is not an existing enum label`),
//! new must not (42710 `enum label "b" already exists`), new length rule.
//!
//! ## Domains
//! `create_domain`: base type must exist (binder); base cannot be a pseudo-type (42804? PG:
//! `"x" is not a valid base type for a domain` 42804) or another domain's... (domains over domains
//! are allowed). Constraint naming (oracle PG 18.6): CHECKs `d_check`, `d_check1`, ... via
//! `naming::choose_constraint_name(ns, domain, None, "check")`; NOT NULL `d_not_null`; explicit names
//! kept. Domain constraints are `Constraint{domain = type oid, table = 0}`; `TypeDef::Domain.not_null`
//! mirrors the NOT NULL constraint. `typcollation` = explicit COLLATE or the base type's (only for
//! collatable bases; else 42804 `collations are not supported by type integer`).
//! `NOT VALID` domain CHECK -> `validated = false`.
//! `alter_domain_default(domain, Option<StoredExpr>)`, `alter_domain_not_null(domain, set)` (SET NOT NULL
//! on a domain used in tables queues a validation: `FollowUp::ValidateNotNull` is table/column
//! specific, so the catalog returns the list of (table, attnum) columns of this domain type via
//! `domain_columns` and the executor validates: PG error 23502 `column "c" of table "t" contains
//! null values`), `add_domain_constraint`, `drop_domain_constraint(domain, name, missing_ok,
//! behavior)` (42704 `constraint "nosuch" of domain "d" does not exist`; skipping NOTICE
//! `constraint "nosuch" of domain "d" does not exist, skipping`),
//! `rename_domain_constraint`, `validate_domain_constraint`. Duplicate constraint name on one domain ->
//! 42710 `constraint "c3" for domain "d" already exists`. A non-domain target ->
//! 42809 `e is not a domain` (`errors::not_a_domain`, unquoted).
//!
//! ## Composite types
//! `create_composite`: a `Relation{kind: CompositeType, columns, rowtype, array_type}` (no storage,
//! no system columns). Column rules as CREATE TABLE (names unique, <= 1600, not pseudo-types);
//! no defaults/constraints.
//! `ALTER TYPE c ADD/DROP/RENAME ATTRIBUTE` and `ALTER ATTRIBUTE ... TYPE` are
//! `alter_table::{add_column, drop_column, rename_column, alter_column_type}` on the composite's
//! relation OID. For ADD/DROP/ALTER TYPE they additionally queue `FollowUp::RequireEmptyTable` for every
//! table column declared with the composite type (oracle PG 18.6: `create type c as (a int, b text);
//! create table u(c c); alter type c add attribute z int; alter type c drop attribute a; alter type c
//! rename attribute b to bb` ALL succeed while `u` is empty; PG refuses with 0A000 `cannot alter type
//! "c" because column "u.c" uses it` only when `u` has rows, which only the executor can see).
//! RENAME ATTRIBUTE needs no check. `DROP TYPE c` with `u(c c)` present -> 2BP01 `cannot drop type c
//! because other objects depend on it` DETAIL `column c of table u depends on type c`.
//!
//! ## Renaming / moving / owner
//! `rename_type`, `set_type_schema`, `alter_type_owner` apply to enum/domain `TypeObj`s; for a
//! relation rowtype OID they delegate to the relation functions (ALTER TYPE on a table rowtype is
//! 42809 `"t" is a table's row type` HINT `Use ALTER TABLE instead.`; composite types OK).
//! Renaming re-chooses the array type name (`array_type_name` reset/recomputed after moving a
//! clashing array out of the way, PG `RenameTypeInternal`). Owner change rewrites the ACL
//! (`acl::aclnewowner`) and requires CREATE on the schema for the new owner. `SET SCHEMA` checks the
//! target schema for name clashes of the type and its array. Stored expressions mentioning the
//! type are rewritten through `ExprRewriter` (`NameChange::Type`).
use super::common::MAX_COLUMNS;
use super::spec::*;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::*;
use crate::naming;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::context::TypeEnv;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};

/// Maximum enum label length in bytes (`NAMEDATALEN - 1`).
const MAX_ENUM_LABEL_BYTES: usize = 63;

/// Where `ALTER TYPE ... ADD VALUE` puts the new label.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnumPosition {
    /// After the current last label.
    Append,
    Before(String),
    After(String),
}

// ---------------------------------------------------------------------------------------------
// Private pure helpers
// ---------------------------------------------------------------------------------------------

/// PG `EnumValuesCreate` / `AddEnumLabel` length rule: 42602 `invalid enum label`.
fn check_label_len(label: &str) -> Result<()> {
    if label.len() > MAX_ENUM_LABEL_BYTES {
        Err(errors::invalid_enum_label(label))
    } else {
        Ok(())
    }
}

/// Order enum labels by `enumsortorder` (PG `sort_order_cmp`).
fn cmp_sort_order(a: &EnumLabelDef, b: &EnumLabelDef) -> Ordering {
    a.sort_order.partial_cmp(&b.sort_order).unwrap_or(Ordering::Equal)
}

/// Result of the neighbour search of PG `AddEnumLabel`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum EnumSlot {
    /// Use this `enumsortorder` for the new label.
    Order(f32),
    /// The f32 midpoint collided with a neighbour: renumber everything (`RenumberEnumType`) and retry.
    Renumber,
}

/// The sort-order choice of PG `AddEnumLabel` over labels already sorted by `sort_order`.
fn compute_enum_slot(labels: &[EnumLabelDef], position: &EnumPosition) -> Result<EnumSlot> {
    let (neighbor, after) = match position {
        EnumPosition::Append => {
            let o = match labels.last() {
                Some(l) => l.sort_order + 1.0,
                None => 1.0,
            };
            return Ok(EnumSlot::Order(o));
        }
        EnumPosition::Before(n) => (n, false),
        EnumPosition::After(n) => (n, true),
    };
    let idx = labels
        .iter()
        .position(|l| l.label == *neighbor)
        .ok_or_else(|| errors::not_an_existing_enum_label(neighbor))?;
    let nbr = labels[idx].sort_order;
    let other: isize = idx as isize + if after { 1 } else { -1 };
    if other < 0 {
        return Ok(EnumSlot::Order(nbr - 1.0));
    }
    let other = other as usize;
    if other >= labels.len() {
        return Ok(EnumSlot::Order(nbr + 1.0));
    }
    let other_order = labels[other].sort_order;
    // The midpoint is computed (and rounded) in f32, exactly like PG's volatile float4.
    let midpoint: f32 = (nbr + other_order) / 2.0;
    if midpoint == nbr || midpoint == other_order {
        Ok(EnumSlot::Renumber)
    } else {
        Ok(EnumSlot::Order(midpoint))
    }
}

/// `Some(name)` only when `chosen` differs from the canonical array type name of `type_name`.
fn non_canonical_array_name(type_name: &str, chosen: String) -> Option<String> {
    if chosen == naming::array_type_name(type_name) {
        None
    } else {
        Some(chosen)
    }
}

/// `expr` depends on the catalog object (`class`, `oid`).
fn expr_mentions(e: &StoredExpr, class: ObjClass, oid: Oid) -> bool {
    e.depends_on.iter().any(|d| d.class == class && d.oid == oid)
}

/// Build the attribute list of a composite type (PG `DefineRelation` + `CheckAttributeNamesTypes`
/// for `RELKIND_COMPOSITE_TYPE`: system column names ARE allowed).
fn composite_columns(env: &CatalogEnv, attrs: &[ColumnSpec]) -> Result<Vec<Column>> {
    if attrs.len() > MAX_COLUMNS {
        return Err(errors::too_many_columns());
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(attrs.len());
    for a in attrs {
        if !seen.insert(a.name.as_str()) {
            return Err(errors::column_specified_more_than_once(&a.name));
        }
    }
    let mut cols = Vec::with_capacity(attrs.len());
    for (i, a) in attrs.iter().enumerate() {
        let info = env
            .type_info(a.type_id.oid)
            .ok_or_else(|| errors::cache_lookup_failed("type", a.type_id.oid))?;
        if info.is_pseudo() {
            return Err(errors::column_has_pseudo_type(
                &a.name,
                &pgarbor_types::names::display_name(env, a.type_id.oid),
            ));
        }
        if info.is_array() {
            if let Some(elem) = env.type_info(info.elem) {
                if elem.is_pseudo() {
                    return Err(errors::column_has_pseudo_type(
                        &a.name,
                        &pgarbor_types::names::display_name(env, info.elem),
                    ));
                }
            }
        }
        let collation = match a.collation {
            Some(c) => {
                if c != 0 && info.collation == 0 {
                    return Err(PgError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "collations are not supported by type {}",
                            pgarbor_types::names::display_name(env, a.type_id.oid)
                        ),
                    ));
                }
                c
            }
            None => info.collation,
        };
        let mut col = Column::new((i + 1) as i16, a.name.clone(), a.type_id, collation);
        col.ndims = a.ndims;
        cols.push(col);
    }
    Ok(cols)
}

/// What an `ALTER TYPE` target OID turned out to be.
enum Alterable {
    /// An enum or domain.
    Obj(TypeObj),
    /// A free-standing composite type: the relation OID.
    Composite(Oid),
}

impl<'a> CatalogTxn<'a> {
    // -----------------------------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------------------------

    /// `format_type_be` of a type OID (schema-qualified when not visible).
    fn type_name_for_msg(&mut self, oid: Oid) -> Result<String> {
        let env = self.env()?;
        Ok(pgarbor_types::names::display_name(&env, oid))
    }

    /// `resolve_type_name_clash` as used by CREATE TYPE/DOMAIN: PG's `DefineEnum`/`DefineDomain`/
    /// `DefineCompositeType` raise a plain `type "x" already exists` (the relation HINT belongs to
    /// `heap_create_with_catalog` only).
    fn check_type_name_free(&mut self, ns: Oid, name: &str) -> Result<()> {
        self.resolve_type_name_clash(ns, name).map_err(|mut e| {
            if e.sqlstate == SqlState::DUPLICATE_OBJECT {
                e.hint = None;
            }
            e
        })
    }

    /// Enum target of `ALTER TYPE ... ADD/RENAME VALUE` (PG `checkEnumOwner`): kind check, then owner.
    fn enum_for_alter(&mut self, ty: Oid) -> Result<TypeObj> {
        let found: Option<TypeObj> = self.data.types.get(&ty).filter(|t| t.is_enum()).cloned();
        match found {
            Some(t) => {
                self.require_owner(&ObjectAddress::type_(ty))?;
                Ok(t)
            }
            None => {
                let name = self.type_name_for_msg(ty)?;
                Err(errors::not_an_enum(&name))
            }
        }
    }

    /// Domain target of `ALTER DOMAIN` (PG `checkDomainOwner`): kind check, then owner.
    fn domain_for_alter(&mut self, ty: Oid) -> Result<TypeObj> {
        let found: Option<TypeObj> = self.data.types.get(&ty).filter(|t| t.is_domain()).cloned();
        match found {
            Some(t) => {
                self.require_owner(&ObjectAddress::type_(ty))?;
                Ok(t)
            }
            None => {
                let name = self.type_name_for_msg(ty)?;
                Err(errors::not_a_domain(&name))
            }
        }
    }

    /// Classify the target of ALTER TYPE RENAME / SET SCHEMA / OWNER (PG's checks on table row types
    /// and array types).
    fn alterable_type(&mut self, ty: Oid) -> Result<Alterable> {
        if let Some(t) = self.data.types.get(&ty) {
            return Ok(Alterable::Obj(t.clone()));
        }
        let row = self.data.relations.values().find(|r| r.rowtype != 0 && r.rowtype == ty).map(|r| (r.oid, r.kind));
        if let Some((rel, kind)) = row {
            if kind == RelKind::CompositeType {
                return Ok(Alterable::Composite(rel));
            }
            let name = self.type_name_for_msg(ty)?;
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("{name} is a table's row type"))
                .with_hint("Use ALTER TABLE instead."));
        }
        let elem = self
            .data
            .types
            .values()
            .find(|t| t.array_type != 0 && t.array_type == ty)
            .map(|t| t.oid)
            .or_else(|| self.data.relations.values().find(|r| r.array_type != 0 && r.array_type == ty).map(|r| r.rowtype));
        if let Some(elem) = elem {
            let an = self.type_name_for_msg(ty)?;
            let en = self.type_name_for_msg(elem)?;
            return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot alter array type {an}"))
                .with_hint(format!("You can alter type {en}, which will alter the array type as well.")));
        }
        if ty < pgarbor_common::FIRST_NORMAL_OID {
            let name = self.type_name_for_msg(ty)?;
            return Err(errors::cannot_rename_system_object(&name));
        }
        Err(errors::cache_lookup_failed("type", ty))
    }

    /// Store one domain constraint row.
    fn put_domain_constraint(
        &mut self,
        domain: Oid,
        ns: Oid,
        name: String,
        kind: ConstraintKind,
        check: Option<StoredExpr>,
        validated: bool,
    ) -> Result<Oid> {
        let oid = self.new_oid()?;
        let con = Constraint {
            oid,
            name,
            namespace: ns,
            kind,
            table: 0,
            domain,
            columns: Vec::new(),
            index: 0,
            deferrable: false,
            initially_deferred: false,
            validated,
            enforced: true,
            is_local: true,
            inh_count: 0,
            no_inherit: false,
            period: false,
            fk: None,
            check,
            parent: 0,
        };
        self.put(CatalogObject::Constraint(con))?;
        self.record_created(ObjectAddress::constraint(oid));
        Ok(oid)
    }

    /// Is `name` already used by a constraint of `domain`?
    fn domain_constraint_named(&self, domain: Oid, name: &str) -> Option<Constraint> {
        self.data.constraints.values().find(|c| c.domain == domain && c.name == name).cloned()
    }

    /// Re-deparse every stored expression that depends on `target` (module docs: `NameChange`).
    /// Call AFTER the rename/move was written so `env` shows the new names.
    fn rewrite_dependent_exprs(
        &mut self,
        target: ObjectAddress,
        change: &NameChange,
        rewriter: &dyn ExprRewriter,
    ) -> Result<()> {
        let env = self.env()?;
        let hit = |e: &StoredExpr| expr_mentions(e, target.class, target.oid);

        let rel_oids: Vec<Oid> = self
            .data
            .relations
            .values()
            .filter(|r| {
                r.view.as_ref().is_some_and(|v| hit(&v.query))
                    || r.columns.iter().any(|c| c.default.as_ref().is_some_and(|d| hit(&d.expr)))
            })
            .map(|r| r.oid)
            .collect();
        for oid in rel_oids {
            self.update_relation(oid, |r| {
                if let Some(v) = r.view.as_mut() {
                    if hit(&v.query) {
                        let new = rewriter.rewrite(&v.query, change, &env)?;
                        v.query = new;
                    }
                }
                for c in r.columns.iter_mut() {
                    if let Some(d) = c.default.as_mut() {
                        if hit(&d.expr) {
                            let new = rewriter.rewrite(&d.expr, change, &env)?;
                            d.expr = new;
                        }
                    }
                }
                Ok(())
            })?;
        }

        let con_oids: Vec<Oid> = self
            .data
            .constraints
            .values()
            .filter(|c| c.check.as_ref().is_some_and(|e| hit(e)))
            .map(|c| c.oid)
            .collect();
        for oid in con_oids {
            self.update_constraint(oid, |c| {
                if let Some(e) = c.check.as_mut() {
                    let new = rewriter.rewrite(e, change, &env)?;
                    *e = new;
                }
                Ok(())
            })?;
        }

        let type_oids: Vec<Oid> = self
            .data
            .types
            .values()
            .filter(|t| matches!(&t.def, TypeDef::Domain { default: Some(e), .. } if hit(e)))
            .map(|t| t.oid)
            .collect();
        for oid in type_oids {
            self.update_type(oid, |t| {
                if let TypeDef::Domain { default: Some(e), .. } = &mut t.def {
                    let new = rewriter.rewrite(e, change, &env)?;
                    *e = new;
                }
                Ok(())
            })?;
        }

        let idx_oids: Vec<Oid> = self
            .data
            .indexes
            .values()
            .filter(|i| {
                i.predicate.as_ref().is_some_and(|e| hit(e)) || i.keys.iter().any(|k| k.expr.as_ref().is_some_and(|e| hit(e)))
            })
            .map(|i| i.oid)
            .collect();
        for oid in idx_oids {
            self.update_index(oid, |i| {
                if let Some(e) = i.predicate.as_mut() {
                    if hit(&*e) {
                        let new = rewriter.rewrite(e, change, &env)?;
                        *e = new;
                    }
                }
                for k in i.keys.iter_mut() {
                    if let Some(e) = k.expr.as_mut() {
                        if hit(&*e) {
                            let new = rewriter.rewrite(e, change, &env)?;
                            *e = new;
                        }
                    }
                }
                Ok(())
            })?;
        }

        let fn_oids: Vec<Oid> = self
            .data
            .functions
            .values()
            .filter(|f| {
                f.sql_body.as_ref().is_some_and(|e| hit(e)) || f.args.iter().any(|a| a.default.as_ref().is_some_and(|e| hit(e)))
            })
            .map(|f| f.oid)
            .collect();
        for oid in fn_oids {
            self.update_function(oid, |f| {
                if let Some(e) = f.sql_body.as_mut() {
                    if hit(&*e) {
                        let new = rewriter.rewrite(e, change, &env)?;
                        *e = new;
                    }
                }
                for a in f.args.iter_mut() {
                    if let Some(e) = a.default.as_mut() {
                        if hit(&*e) {
                            let new = rewriter.rewrite(e, change, &env)?;
                            *e = new;
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Enums
    // -----------------------------------------------------------------------------------------

    /// CREATE TYPE ... AS ENUM. Returns the type OID.
    // CONTRACT
    pub fn create_enum(&mut self, spec: &CreateEnumSpec) -> Result<Oid> {
        let ns = spec.namespace;
        self.require_schema_create(ns)?;
        self.check_creatable_namespace(ns, &spec.name)?;
        self.check_type_name_free(ns, &spec.name)?;

        let oid = self.new_oid()?;
        let array_oid = self.new_oid()?;

        // EnumValuesCreate: every label is length-checked, then the batch insert hits the unique
        // index (enumtypid, enumlabel).
        for l in &spec.labels {
            check_label_len(l)?;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(spec.labels.len());
        for l in &spec.labels {
            if !seen.insert(l.as_str()) {
                return Err(PgError::new(
                    SqlState::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
                )
                .with_detail(format!("Key (enumtypid, enumlabel)=({oid}, {l}) already exists."))
                .with_table("pg_catalog", "pg_enum")
                .with_constraint("pg_enum_typid_label_index"));
            }
        }

        let snap = self.snapshot()?;
        let arr = naming::choose_array_type_name(&snap, ns, &spec.name, &[])?;

        let mut labels = Vec::with_capacity(spec.labels.len());
        for (i, l) in spec.labels.iter().enumerate() {
            let label_oid = self.new_oid()?;
            labels.push(EnumLabelDef { oid: label_oid, label: l.clone(), sort_order: (i + 1) as f32 });
        }

        let obj = TypeObj {
            oid,
            name: spec.name.clone(),
            namespace: ns,
            owner: spec.owner,
            array_type: array_oid,
            array_type_name: non_canonical_array_name(&spec.name, arr),
            def: TypeDef::Enum { labels },
            acl: None,
        };
        self.put(CatalogObject::Type(obj))?;
        self.record_created(ObjectAddress::type_(oid));
        Ok(oid)
    }

    /// ALTER TYPE ... ADD VALUE. `Ok(false)` when skipped by `if_not_exists`.
    // CONTRACT
    pub fn add_enum_value(&mut self, ty: Oid, label: &str, position: &EnumPosition, if_not_exists: bool) -> Result<bool> {
        let t = self.enum_for_alter(ty)?;
        let TypeDef::Enum { labels } = t.def else {
            return Err(PgError::internal("enum type without labels"));
        };
        check_label_len(label)?;
        if labels.iter().any(|l| l.label == label) {
            if if_not_exists {
                self.notice(errors::enum_label_already_exists_skipping(label));
                return Ok(false);
            }
            return Err(errors::enum_label_already_exists(label));
        }

        let mut labels = labels;
        labels.sort_by(cmp_sort_order);
        let mut renumbered = false;
        let order = loop {
            match compute_enum_slot(&labels, position)? {
                EnumSlot::Order(o) => break o,
                EnumSlot::Renumber => {
                    if renumbered {
                        return Err(PgError::internal("could not assign a sort order to the new enum label"));
                    }
                    // PG RenumberEnumType: 1.0 .. n in the current order, then start over.
                    for (i, l) in labels.iter_mut().enumerate() {
                        l.sort_order = (i + 1) as f32;
                    }
                    renumbered = true;
                }
            }
        };
        let oid = self.new_oid()?;
        labels.push(EnumLabelDef { oid, label: label.to_string(), sort_order: order });
        labels.sort_by(cmp_sort_order);
        self.update_type(ty, |t| {
            t.def = TypeDef::Enum { labels };
            Ok(())
        })?;
        Ok(true)
    }

    /// ALTER TYPE ... RENAME VALUE.
    // CONTRACT
    pub fn rename_enum_value(&mut self, ty: Oid, old: &str, new: &str) -> Result<()> {
        let t = self.enum_for_alter(ty)?;
        let TypeDef::Enum { labels } = t.def else {
            return Err(PgError::internal("enum type without labels"));
        };
        check_label_len(new)?;
        if !labels.iter().any(|l| l.label == old) {
            return Err(errors::not_an_existing_enum_label(old));
        }
        if labels.iter().any(|l| l.label == new) {
            return Err(errors::enum_label_already_exists(new));
        }
        self.update_type(ty, |t| {
            if let TypeDef::Enum { labels } = &mut t.def {
                for l in labels.iter_mut() {
                    if l.label == old {
                        l.label = new.to_string();
                    }
                }
            }
            Ok(())
        })
    }

    // -----------------------------------------------------------------------------------------
    // Domains
    // -----------------------------------------------------------------------------------------

    /// CREATE DOMAIN. Returns the type OID.
    // CONTRACT
    pub fn create_domain(&mut self, spec: &CreateDomainSpec) -> Result<Oid> {
        let ns = spec.namespace;
        self.require_schema_create(ns)?;
        self.check_creatable_namespace(ns, &spec.name)?;
        self.check_type_name_free(ns, &spec.name)?;

        let env = self.env()?;
        let base_info = env
            .type_info(spec.base.oid)
            .ok_or_else(|| errors::cache_lookup_failed("type", spec.base.oid))?;
        if !matches!(base_info.typtype, b'b' | b'c' | b'd' | b'e' | b'r' | b'm') {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "\"{}\" is not a valid base type for a domain",
                    pgarbor_types::names::display_name(&env, spec.base.oid)
                ),
            ));
        }
        let base_coll = base_info.collation;
        let collation = spec.collation.unwrap_or(base_coll);
        if collation != 0 && base_coll == 0 {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "collations are not supported by type {}",
                    pgarbor_types::names::display_name(&env, spec.base.oid)
                ),
            ));
        }

        // A domain over a domain inherits the base domain's default unless it gives its own.
        let default = match &spec.default {
            Some(d) => Some(d.clone()),
            None => match self.data.types.get(&spec.base.oid) {
                Some(TypeObj { def: TypeDef::Domain { default, .. }, .. }) => default.clone(),
                _ => None,
            },
        };

        let oid = self.new_oid()?;
        let array_oid = self.new_oid()?;
        let arr = naming::choose_array_type_name(env.snapshot(), ns, &spec.name, &[])?;
        let obj = TypeObj {
            oid,
            name: spec.name.clone(),
            namespace: ns,
            owner: spec.owner,
            array_type: array_oid,
            array_type_name: non_canonical_array_name(&spec.name, arr),
            def: TypeDef::Domain {
                base: spec.base,
                not_null: spec.not_null.is_some(),
                default,
                collation,
                ndims: spec.ndims,
            },
            acl: None,
        };
        self.put(CatalogObject::Type(obj))?;
        self.record_created(ObjectAddress::type_(oid));

        // Constraints: CHECKs in order, then NOT NULL (names d_check, d_check1, ..., d_not_null).
        let snap = self.snapshot()?;
        let mut used: Vec<String> = Vec::new();
        for c in &spec.checks {
            let name = match &c.name {
                Some(n) => {
                    if used.iter().any(|u| u == n) {
                        return Err(PgError::new(
                            SqlState::DUPLICATE_OBJECT,
                            format!("constraint \"{}\" for domain \"{}\" already exists", n, spec.name),
                        ));
                    }
                    n.clone()
                }
                None => naming::choose_constraint_name(&snap, ns, &spec.name, None, "check", &used)?,
            };
            used.push(name.clone());
            self.put_domain_constraint(oid, ns, name, ConstraintKind::Check, Some(c.expr.clone()), !c.not_valid)?;
        }
        if let Some(nn) = &spec.not_null {
            let name = match nn {
                Some(n) => {
                    if used.iter().any(|u| u == n) {
                        return Err(PgError::new(
                            SqlState::DUPLICATE_OBJECT,
                            format!("constraint \"{}\" for domain \"{}\" already exists", n, spec.name),
                        ));
                    }
                    n.clone()
                }
                None => naming::choose_constraint_name(&snap, ns, &spec.name, None, "not_null", &used)?,
            };
            used.push(name.clone());
            self.put_domain_constraint(oid, ns, name, ConstraintKind::NotNull, None, true)?;
        }
        Ok(oid)
    }

    /// ALTER DOMAIN ... SET DEFAULT expr / DROP DEFAULT (`None`).
    // CONTRACT
    pub fn alter_domain_default(&mut self, domain: Oid, default: Option<StoredExpr>) -> Result<()> {
        self.domain_for_alter(domain)?;
        self.update_type(domain, |t| {
            if let TypeDef::Domain { default: d, .. } = &mut t.def {
                *d = default;
            }
            Ok(())
        })
    }

    /// ALTER DOMAIN ... SET NOT NULL / DROP NOT NULL. Returns, for SET NOT NULL, the (table, attnum)
    /// columns of this domain type that the executor must validate (module docs); empty otherwise.
    // CONTRACT
    pub fn alter_domain_not_null(&mut self, domain: Oid, set: bool) -> Result<Vec<(Oid, i16)>> {
        let t = self.domain_for_alter(domain)?;
        let currently = matches!(t.def, TypeDef::Domain { not_null: true, .. });
        if currently == set {
            return Ok(Vec::new());
        }
        if set {
            let snap = self.snapshot()?;
            let name = naming::choose_constraint_name(&snap, t.namespace, &t.name, None, "not_null", &[])?;
            self.put_domain_constraint(domain, t.namespace, name, ConstraintKind::NotNull, None, true)?;
            self.update_type(domain, |t| {
                if let TypeDef::Domain { not_null, .. } = &mut t.def {
                    *not_null = true;
                }
                Ok(())
            })?;
            self.domain_columns(domain)
        } else {
            let con = self
                .data
                .constraints
                .values()
                .find(|c| c.domain == domain && c.kind == ConstraintKind::NotNull)
                .map(|c| c.oid);
            let Some(con) = con else {
                return Err(PgError::internal(format!("could not find not-null constraint on domain \"{}\"", t.name)));
            };
            self.remove(ObjKind::Constraint, con)?;
            self.record_dropped(ObjectAddress::constraint(con));
            self.update_type(domain, |t| {
                if let TypeDef::Domain { not_null, .. } = &mut t.def {
                    *not_null = false;
                }
                Ok(())
            })?;
            Ok(Vec::new())
        }
    }

    /// ALTER DOMAIN ... ADD CONSTRAINT. Returns the constraint OID and, unless NOT VALID, the
    /// (table, attnum) columns to validate.
    // CONTRACT
    pub fn add_domain_constraint(&mut self, domain: Oid, spec: &DomainConstraintSpec) -> Result<(Oid, Vec<(Oid, i16)>)> {
        let t = self.domain_for_alter(domain)?;
        let name = match &spec.name {
            Some(n) => {
                if self.domain_constraint_named(domain, n).is_some() {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("constraint \"{}\" for domain \"{}\" already exists", n, t.name),
                    ));
                }
                n.clone()
            }
            None => {
                let snap = self.snapshot()?;
                naming::choose_constraint_name(&snap, t.namespace, &t.name, None, "check", &[])?
            }
        };
        let oid = self.put_domain_constraint(
            domain,
            t.namespace,
            name,
            ConstraintKind::Check,
            Some(spec.expr.clone()),
            !spec.not_valid,
        )?;
        let cols = if spec.not_valid { Vec::new() } else { self.domain_columns(domain)? };
        Ok((oid, cols))
    }

    /// ALTER DOMAIN ... DROP CONSTRAINT. `Ok(false)` when skipped by `missing_ok`.
    // CONTRACT
    pub fn drop_domain_constraint(&mut self, domain: Oid, name: &str, missing_ok: bool, behavior: DropBehavior) -> Result<bool> {
        // Nothing can depend on a domain constraint, so RESTRICT and CASCADE behave alike.
        let _ = behavior;
        self.domain_for_alter(domain)?;
        match self.domain_constraint_named(domain, name) {
            Some(con) => {
                if con.kind == ConstraintKind::NotNull {
                    self.update_type(domain, |t| {
                        if let TypeDef::Domain { not_null, .. } = &mut t.def {
                            *not_null = false;
                        }
                        Ok(())
                    })?;
                }
                self.remove(ObjKind::Constraint, con.oid)?;
                self.record_dropped(ObjectAddress::constraint(con.oid));
                Ok(true)
            }
            None => {
                let dn = self.type_name_for_msg(domain)?;
                if missing_ok {
                    self.notice(PgError::notice(format!(
                        "constraint \"{name}\" of domain \"{dn}\" does not exist, skipping"
                    )));
                    Ok(false)
                } else {
                    Err(PgError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("constraint \"{name}\" of domain \"{dn}\" does not exist"),
                    ))
                }
            }
        }
    }

    /// ALTER DOMAIN ... RENAME CONSTRAINT.
    // CONTRACT
    pub fn rename_domain_constraint(&mut self, domain: Oid, old: &str, new: &str) -> Result<()> {
        self.domain_for_alter(domain)?;
        let Some(con) = self.domain_constraint_named(domain, old) else {
            let dn = self.type_name_for_msg(domain)?;
            return Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{old}\" for domain {dn} does not exist"),
            ));
        };
        if self.domain_constraint_named(domain, new).is_some() {
            let dn = self.type_name_for_msg(domain)?;
            return Err(PgError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("constraint \"{new}\" for domain {dn} already exists"),
            ));
        }
        self.update_constraint(con.oid, |c| {
            c.name = new.to_string();
            Ok(())
        })
    }

    /// ALTER DOMAIN ... VALIDATE CONSTRAINT. Returns the columns to validate.
    // CONTRACT
    pub fn validate_domain_constraint(&mut self, domain: Oid, name: &str) -> Result<Vec<(Oid, i16)>> {
        self.domain_for_alter(domain)?;
        let dn = self.type_name_for_msg(domain)?;
        let Some(con) = self.domain_constraint_named(domain, name) else {
            return Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of domain \"{dn}\" does not exist"),
            ));
        };
        if con.kind != ConstraintKind::Check {
            return Err(PgError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("constraint \"{name}\" of domain \"{dn}\" is not a check constraint"),
            ));
        }
        self.update_constraint(con.oid, |c| {
            c.validated = true;
            Ok(())
        })?;
        self.domain_columns(domain)
    }

    // -----------------------------------------------------------------------------------------
    // Composite types
    // -----------------------------------------------------------------------------------------

    /// CREATE TYPE name AS (attrs). Returns the composite's relation OID (= its `typrelid`).
    // CONTRACT
    pub fn create_composite(&mut self, spec: &CreateCompositeSpec) -> Result<Oid> {
        let ns = spec.namespace;
        self.require_schema_create(ns)?;
        self.check_creatable_namespace(ns, &spec.name)?;
        // PG checks the type name first (`type "c" already exists`), then the relation name.
        self.check_type_name_free(ns, &spec.name)?;
        self.check_new_relation_name(ns, &spec.name, false)?;

        let env = self.env()?;
        let columns = composite_columns(&env, &spec.attributes)?;

        let oid = self.new_oid()?;
        let rowtype = self.new_oid()?;
        let array_type = self.new_oid()?;
        let arr = naming::choose_array_type_name(env.snapshot(), ns, &spec.name, &[])?;
        let rel = Relation {
            oid,
            name: spec.name.clone(),
            namespace: ns,
            kind: RelKind::CompositeType,
            owner: spec.owner,
            persistence: Persistence::Permanent,
            columns,
            rowtype,
            array_type,
            array_type_name: non_canonical_array_name(&spec.name, arr),
            options: Vec::new(),
            acl: None,
            replica_identity: ReplicaIdentity::Nothing,
            row_security: false,
            force_row_security: false,
            policies: Vec::new(),
            rules: Vec::new(),
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        };
        self.put(CatalogObject::Relation(rel))?;
        self.record_created(ObjectAddress::relation(oid));
        Ok(oid)
    }

    // -----------------------------------------------------------------------------------------
    // Rename / move / owner
    // -----------------------------------------------------------------------------------------

    /// ALTER TYPE/DOMAIN ... RENAME TO.
    // CONTRACT
    pub fn rename_type(&mut self, ty: Oid, new_name: &str, rewriter: &dyn ExprRewriter) -> Result<()> {
        match self.alterable_type(ty)? {
            Alterable::Composite(rel) => {
                let (rowtype, old_name) = match self.data.relations.get(&rel) {
                    Some(r) => (r.rowtype, r.name.clone()),
                    None => return Err(errors::cache_lookup_failed("relation", rel)),
                };
                self.rename_relation(rel, new_name, rewriter)?;
                let change = NameChange::Type { oid: rowtype, old: old_name, new: new_name.to_string() };
                self.rewrite_dependent_exprs(ObjectAddress::type_(rowtype), &change, rewriter)
            }
            Alterable::Obj(t) => {
                self.require_owner(&ObjectAddress::type_(ty))?;
                let ns = t.namespace;
                let old_array_name =
                    t.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&t.name));
                // Renaming `foo` to `_foo` (its own array's name): the array is moved out of the way
                // by the clash resolution and must keep that name afterwards (PG `RenameTypeInternal`).
                let own_array_clash = old_array_name == new_name;

                self.check_type_name_free(ns, new_name)?;
                self.update_type(ty, |t| {
                    t.name = new_name.to_string();
                    if !own_array_clash {
                        // The array keeps its current name until the new one is chosen below (PG renames
                        // the array only after the type). Otherwise the snapshot would already count
                        // `_<new_name>` as taken by the type's own canonical array name.
                        t.array_type_name = Some(old_array_name.clone());
                    }
                    Ok(())
                })?;
                if !own_array_clash {
                    let snap = self.snapshot()?;
                    let arr = naming::choose_array_type_name(&snap, ns, new_name, &[])?;
                    let stored = non_canonical_array_name(new_name, arr);
                    self.update_type(ty, |t| {
                        t.array_type_name = stored;
                        Ok(())
                    })?;
                }
                let change = NameChange::Type { oid: ty, old: t.name.clone(), new: new_name.to_string() };
                self.rewrite_dependent_exprs(ObjectAddress::type_(ty), &change, rewriter)
            }
        }
    }

    /// ALTER TYPE/DOMAIN ... SET SCHEMA.
    // CONTRACT
    pub fn set_type_schema(&mut self, ty: Oid, new_schema: Oid, rewriter: &dyn ExprRewriter) -> Result<()> {
        // PG: LookupCreationNamespace (CREATE privilege on the target) comes first.
        self.require_schema_create(new_schema)?;
        match self.alterable_type(ty)? {
            Alterable::Composite(rel) => {
                let (rowtype, name, old_ns) = match self.data.relations.get(&rel) {
                    Some(r) => (r.rowtype, r.name.clone(), r.namespace),
                    None => return Err(errors::cache_lookup_failed("relation", rel)),
                };
                self.set_relation_schema(rel, new_schema, rewriter)?;
                if old_ns != new_schema {
                    let change = NameChange::Type { oid: rowtype, old: name.clone(), new: name };
                    self.rewrite_dependent_exprs(ObjectAddress::type_(rowtype), &change, rewriter)?;
                }
                Ok(())
            }
            Alterable::Obj(t) => {
                self.require_owner(&ObjectAddress::type_(ty))?;
                if t.namespace == new_schema {
                    return Ok(());
                }
                self.check_creatable_namespace(new_schema, &t.name)?;
                let snap = self.snapshot()?;
                let ns_name = snap
                    .namespace(new_schema)
                    .map(|n| n.name.clone())
                    .ok_or_else(|| errors::cache_lookup_failed("schema", new_schema))?;
                if snap.user_type_in_namespace(new_schema, &t.name).is_some() {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("type \"{}\" already exists in schema \"{}\"", t.name, ns_name),
                    ));
                }
                let arr_name = t.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&t.name));
                if snap.user_type_in_namespace(new_schema, &arr_name).is_some() {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("type \"{}\" already exists in schema \"{}\"", arr_name, ns_name),
                    ));
                }
                self.update_type(ty, |t| {
                    t.namespace = new_schema;
                    Ok(())
                })?;
                // Domain constraints live in the domain's schema (PG `AlterConstraintNamespaces`).
                let cons: Vec<Oid> = self.data.constraints.values().filter(|c| c.domain == ty).map(|c| c.oid).collect();
                for c in cons {
                    self.update_constraint(c, |c| {
                        c.namespace = new_schema;
                        Ok(())
                    })?;
                }
                let change = NameChange::Type { oid: ty, old: t.name.clone(), new: t.name.clone() };
                self.rewrite_dependent_exprs(ObjectAddress::type_(ty), &change, rewriter)
            }
        }
    }

    /// ALTER TYPE/DOMAIN ... OWNER TO.
    // CONTRACT
    pub fn alter_type_owner(&mut self, ty: Oid, new_owner: Oid) -> Result<()> {
        match self.alterable_type(ty)? {
            Alterable::Composite(rel) => self.alter_relation_owner(rel, new_owner),
            Alterable::Obj(t) => {
                // Same owner: success (dump restoration), before any permission check.
                if t.owner == new_owner {
                    return Ok(());
                }
                let snap = self.snapshot()?;
                let new_owner_name = match snap.role_name(new_owner) {
                    Some(n) => n.to_string(),
                    None => return Err(errors::cache_lookup_failed("role", new_owner)),
                };
                let me = self.current_user();
                if !self.is_superuser(me)? {
                    self.require_owner(&ObjectAddress::type_(ty))?;
                    let graph = crate::acl::RoleGraph::build(&snap, self.env.database())?;
                    if !graph.can_set_role(me, new_owner)? {
                        return Err(PgError::new(
                            SqlState::INSUFFICIENT_PRIVILEGE,
                            format!("must be able to SET ROLE \"{new_owner_name}\""),
                        ));
                    }
                    let ns = snap
                        .namespace(t.namespace)
                        .cloned()
                        .ok_or_else(|| errors::cache_lookup_failed("schema", t.namespace))?;
                    let acl = crate::acl::effective_acl(crate::acl::AclObject::Schema, ns.owner, ns.acl.as_ref())?;
                    let mask = crate::acl::privilege_mask(&graph, new_owner, ns.owner, &acl, acl_mode::CREATE, false)?;
                    if mask & acl_mode::CREATE == 0 {
                        return Err(errors::permission_denied_for("schema", &ns.name));
                    }
                }
                let old_owner = t.owner;
                self.update_type(ty, |t| {
                    t.owner = new_owner;
                    let new_acl = match t.acl.as_ref() {
                        Some(a) => Some(crate::acl::aclnewowner(a, old_owner, new_owner)?),
                        None => None,
                    };
                    t.acl = new_acl;
                    Ok(())
                })
            }
        }
    }

    /// (table, attnum) of every non-dropped column whose declared type is the domain `domain`
    /// (or an array of it) - used for validation lists.
    // CONTRACT
    pub fn domain_columns(&mut self, domain: Oid) -> Result<Vec<(Oid, i16)>> {
        // The domain, its array type, and every domain (and array) layered on top of it.
        let mut tys: BTreeSet<Oid> = BTreeSet::new();
        tys.insert(domain);
        if let Some(t) = self.data.types.get(&domain) {
            tys.insert(t.array_type);
        }
        loop {
            let mut grew = false;
            for t in self.data.types.values() {
                if let TypeDef::Domain { base, .. } = &t.def {
                    if tys.contains(&base.oid) && !tys.contains(&t.oid) {
                        tys.insert(t.oid);
                        tys.insert(t.array_type);
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        let mut out = Vec::new();
        for r in self.data.relations.values() {
            if !matches!(r.kind, RelKind::Table | RelKind::MatView | RelKind::PartitionedTable) {
                continue;
            }
            for c in &r.columns {
                if !c.dropped && tys.contains(&c.type_id.oid) {
                    // typecmds.c get_rels_with_domain: constraints cannot be validated through
                    // an array container, even when the table currently contains no rows.
                    if let Some(element) = self.data.types.values().find(|t| t.array_type == c.type_id.oid) {
                        return Err(PgError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            format!("cannot alter type \"{}\" because column \"{}.{}\" uses it", element.name, r.name, c.name),
                        ));
                    }
                    out.push((r.oid, c.attnum));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use pgarbor_common::MemKv;
    use pgarbor_types::TypeId;

    const PUBLIC: Oid = crate::builtin::PUBLIC_NAMESPACE;
    const ME: Oid = crate::builtin::BOOTSTRAP_SUPERUSER;

    fn el(oid: Oid, label: &str, order: f32) -> EnumLabelDef {
        EnumLabelDef { oid, label: label.to_string(), sort_order: order }
    }

    fn enum_spec(name: &str, labels: &[&str]) -> CreateEnumSpec {
        CreateEnumSpec {
            namespace: PUBLIC,
            name: name.to_string(),
            owner: ME,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn mk_enum(kv: &mut MemKv, env: &CatalogEnv, name: &str, labels: &[&str]) -> Result<(Oid, CatalogEnv)> {
        let mut oid = 0;
        let spec = enum_spec(name, labels);
        let (_res, env) = testing::run(kv, env, |t| {
            oid = t.create_enum(&spec)?;
            Ok(())
        })?;
        Ok((oid, env))
    }

    fn orders(env: &CatalogEnv, ty: Oid) -> Vec<(String, f32)> {
        match &env.snapshot().type_obj(ty).unwrap().def {
            TypeDef::Enum { labels } => labels.iter().map(|l| (l.label.clone(), l.sort_order)).collect(),
            _ => panic!("not an enum"),
        }
    }

    #[test]
    fn slot_append_before_after() {
        let labels = vec![el(1, "a", 1.0), el(2, "b", 2.0)];
        assert_eq!(compute_enum_slot(&labels, &EnumPosition::Append).unwrap(), EnumSlot::Order(3.0));
        assert_eq!(compute_enum_slot(&[], &EnumPosition::Append).unwrap(), EnumSlot::Order(1.0));
        assert_eq!(
            compute_enum_slot(&labels, &EnumPosition::Before("a".into())).unwrap(),
            EnumSlot::Order(0.0)
        );
        assert_eq!(
            compute_enum_slot(&labels, &EnumPosition::After("b".into())).unwrap(),
            EnumSlot::Order(3.0)
        );
        assert_eq!(
            compute_enum_slot(&labels, &EnumPosition::Before("b".into())).unwrap(),
            EnumSlot::Order(1.5)
        );
        assert_eq!(
            compute_enum_slot(&labels, &EnumPosition::After("a".into())).unwrap(),
            EnumSlot::Order(1.5)
        );
        let e = compute_enum_slot(&labels, &EnumPosition::Before("zz".into())).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22023");
        assert_eq!(e.message, "\"zz\" is not an existing enum label");
    }

    #[test]
    fn slot_precision_exhausted_renumbers() {
        let a = 1.0f32;
        let b = f32::from_bits(a.to_bits() + 1);
        let labels = vec![el(1, "a", a), el(2, "b", b)];
        assert_eq!(compute_enum_slot(&labels, &EnumPosition::After("a".into())).unwrap(), EnumSlot::Renumber);
    }

    #[test]
    fn create_enum_orders_and_errors() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (e, env) = mk_enum(&mut kv, &env, "e", &["a", "b", "c"])?;
        assert_eq!(
            orders(&env, e),
            vec![("a".to_string(), 1.0), ("b".to_string(), 2.0), ("c".to_string(), 3.0)]
        );

        let dup = enum_spec("d", &["a", "b", "a"]);
        let err = testing::run(&mut kv, &env, |t| t.create_enum(&dup).map(|_| ())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "23505");
        assert_eq!(
            err.message,
            "duplicate key value violates unique constraint \"pg_enum_typid_label_index\""
        );

        let long = "x".repeat(64);
        let bad = enum_spec("l", &[long.as_str()]);
        let err = testing::run(&mut kv, &env, |t| t.create_enum(&bad).map(|_| ())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42602");

        // The empty label is legal.
        let (z, env) = mk_enum(&mut kv, &env, "z", &["", "a"])?;
        assert_eq!(orders(&env, z), vec![("".to_string(), 1.0), ("a".to_string(), 2.0)]);

        let again = enum_spec("e", &["q"]);
        let err = testing::run(&mut kv, &env, |t| t.create_enum(&again).map(|_| ())).unwrap_err();
        assert_eq!(err.message, "type \"e\" already exists");
        Ok(())
    }

    #[test]
    fn add_and_rename_enum_values() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (e, env) = mk_enum(&mut kv, &env, "e", &["a", "b"])?;

        let (_r, env) = testing::run(&mut kv, &env, |t| {
            assert!(t.add_enum_value(e, "z", &EnumPosition::Before("a".into()), false)?);
            Ok(())
        })?;
        assert_eq!(
            orders(&env, e),
            vec![("z".to_string(), 0.0), ("a".to_string(), 1.0), ("b".to_string(), 2.0)]
        );

        let (_r, env) = testing::run(&mut kv, &env, |t| {
            t.add_enum_value(e, "m", &EnumPosition::After("a".into()), false)?;
            t.add_enum_value(e, "end", &EnumPosition::Append, false)?;
            Ok(())
        })?;
        let o = orders(&env, e);
        assert_eq!(o.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["z", "a", "m", "b", "end"]);
        assert_eq!(o[2].1, 1.5);
        assert_eq!(o[4].1, 3.0);

        let (res, env) = testing::run(&mut kv, &env, |t| {
            assert!(!t.add_enum_value(e, "a", &EnumPosition::Append, true)?);
            Ok(())
        })?;
        assert_eq!(res.outcome.notices.len(), 1);
        assert_eq!(res.outcome.notices[0].message, "enum label \"a\" already exists, skipping");

        let err = testing::run(&mut kv, &env, |t| t.add_enum_value(e, "a", &EnumPosition::Append, false).map(|_| ()))
            .unwrap_err();
        assert_eq!(err.sqlstate.code(), "42710");
        let err = testing::run(&mut kv, &env, |t| {
            t.add_enum_value(e, "q", &EnumPosition::After("zz".into()), false).map(|_| ())
        })
        .unwrap_err();
        assert_eq!(err.sqlstate.code(), "22023");

        let err = testing::run(&mut kv, &env, |t| t.rename_enum_value(e, "a", "b")).unwrap_err();
        assert_eq!(err.message, "enum label \"b\" already exists");
        let err = testing::run(&mut kv, &env, |t| t.rename_enum_value(e, "nope", "x")).unwrap_err();
        assert_eq!(err.message, "\"nope\" is not an existing enum label");
        let (_r, env) = testing::run(&mut kv, &env, |t| t.rename_enum_value(e, "a", "aa"))?;
        assert!(orders(&env, e).iter().any(|(l, _)| l == "aa"));
        Ok(())
    }

    #[test]
    fn many_inserts_renumber() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (e, mut env) = mk_enum(&mut kv, &env, "e", &["a", "b"])?;
        // Keep inserting right after "a": the gap halves every time until f32 runs out.
        for i in 0..40 {
            let label = format!("n{i}");
            let (_r, next) = testing::run(&mut kv, &env, |t| {
                t.add_enum_value(e, &label, &EnumPosition::After("a".into()), false).map(|_| ())
            })?;
            env = next;
        }
        let o = orders(&env, e);
        assert_eq!(o.len(), 42);
        assert_eq!(o[0].0, "a");
        assert_eq!(o[41].0, "b");
        for w in o.windows(2) {
            assert!(w[0].1 < w[1].1);
        }
        Ok(())
    }

    #[test]
    fn domain_constraint_names() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateDomainSpec {
            namespace: PUBLIC,
            name: "d".into(),
            owner: ME,
            base: TypeId::INT4,
            collation: None,
            default: None,
            not_null: Some(None),
            checks: vec![
                DomainConstraintSpec { name: None, expr: StoredExpr::new("(VALUE > 0)"), not_valid: false },
                DomainConstraintSpec { name: None, expr: StoredExpr::new("(VALUE < 10)"), not_valid: true },
            ],
            ndims: 0,
        };
        let mut d = 0;
        let (_r, env) = testing::run(&mut kv, &env, |t| {
            d = t.create_domain(&spec)?;
            Ok(())
        })?;
        let snap = env.snapshot();
        let mut names: Vec<(String, bool)> =
            snap.domain_constraints(d).iter().map(|c| (c.name.clone(), c.validated)).collect();
        names.sort();
        assert_eq!(
            names,
            vec![("d_check".to_string(), true), ("d_check1".to_string(), false), ("d_not_null".to_string(), true)]
        );
        match &snap.type_obj(d).unwrap().def {
            TypeDef::Domain { not_null, base, .. } => {
                assert!(*not_null);
                assert_eq!(base.oid, 23);
            }
            _ => panic!("not a domain"),
        }

        // Duplicate explicit name, missing drop, enum is not a domain.
        let c3 = DomainConstraintSpec { name: Some("c3".into()), expr: StoredExpr::new("(VALUE <> 5)"), not_valid: false };
        let (_r, env) = testing::run(&mut kv, &env, |t| t.add_domain_constraint(d, &c3).map(|_| ()))?;
        let err = testing::run(&mut kv, &env, |t| t.add_domain_constraint(d, &c3).map(|_| ())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42710");
        assert_eq!(err.message, "constraint \"c3\" for domain \"d\" already exists");
        let err = testing::run(&mut kv, &env, |t| {
            t.drop_domain_constraint(d, "nosuch", false, DropBehavior::Restrict).map(|_| ())
        })
        .unwrap_err();
        assert_eq!(err.sqlstate.code(), "42704");
        assert_eq!(err.message, "constraint \"nosuch\" of domain \"d\" does not exist");
        let (res, env) = testing::run(&mut kv, &env, |t| {
            assert!(!t.drop_domain_constraint(d, "nosuch", true, DropBehavior::Restrict)?);
            Ok(())
        })?;
        assert_eq!(res.outcome.notices[0].message, "constraint \"nosuch\" of domain \"d\" does not exist, skipping");

        let (e, env) = mk_enum(&mut kv, &env, "e", &["a"])?;
        let err = testing::run(&mut kv, &env, |t| t.alter_domain_default(e, None)).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42809");
        assert_eq!(err.message, "e is not a domain");

        // DROP NOT NULL / SET NOT NULL round trip.
        let (_r, env) = testing::run(&mut kv, &env, |t| t.alter_domain_not_null(d, false).map(|_| ()))?;
        assert!(env.snapshot().domain_constraints(d).iter().all(|c| c.kind != ConstraintKind::NotNull));
        let (_r, env) = testing::run(&mut kv, &env, |t| t.alter_domain_not_null(d, true).map(|_| ()))?;
        assert!(env.snapshot().domain_constraints(d).iter().any(|c| c.name == "d_not_null"));
        Ok(())
    }

    #[test]
    fn composite_has_no_system_columns() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let spec = CreateCompositeSpec {
            namespace: PUBLIC,
            name: "c".into(),
            owner: ME,
            attributes: vec![
                ColumnSpec::new("a", TypeId::INT4),
                ColumnSpec::new("b", TypeId::TEXT),
                ColumnSpec::new("xmin", TypeId::INT4),
            ],
        };
        let mut rel = 0;
        let (_r, env) = testing::run(&mut kv, &env, |t| {
            rel = t.create_composite(&spec)?;
            Ok(())
        })?;
        let r = env.snapshot().relation(rel).unwrap();
        assert_eq!(r.kind, RelKind::CompositeType);
        assert_eq!(r.columns.len(), 3);
        assert!(r.columns.iter().all(|c| c.attnum > 0));
        assert!(r.rowtype != 0 && r.array_type != 0);

        let dup = CreateCompositeSpec {
            namespace: PUBLIC,
            name: "c2".into(),
            owner: ME,
            attributes: vec![ColumnSpec::new("a", TypeId::INT4), ColumnSpec::new("a", TypeId::INT4)],
        };
        let err = testing::run(&mut kv, &env, |t| t.create_composite(&dup).map(|_| ())).unwrap_err();
        assert_eq!(err.sqlstate.code(), "42701");
        Ok(())
    }

    #[test]
    fn array_type_moved_out_of_the_way() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (e, env) = mk_enum(&mut kv, &env, "e", &["a"])?;
        let (u, env) = mk_enum(&mut kv, &env, "_e", &["a"])?;
        let snap = env.snapshot();
        assert_eq!(snap.type_obj(e).unwrap().array_type_name.as_deref(), Some("__e"));
        assert_eq!(snap.type_obj(u).unwrap().array_type_name.as_deref(), Some("__e_1"));
        Ok(())
    }

    #[test]
    fn rename_type_rechooses_array_name() -> Result<()> {
        let (mut kv, env) = testing::bootstrapped()?;
        let (e, env) = mk_enum(&mut kv, &env, "e", &["a"])?;
        let (_r, env) = testing::run(&mut kv, &env, |t| t.rename_type(e, "f", &NoRewrite))?;
        let t = env.snapshot().type_obj(e).unwrap();
        assert_eq!(t.name, "f");
        assert_eq!(t.array_type_name, None);
        // Renaming to the name of one's own array keeps the moved array name.
        let (_r, env) = testing::run(&mut kv, &env, |t| t.rename_type(e, "_f", &NoRewrite))?;
        let t = env.snapshot().type_obj(e).unwrap();
        assert_eq!(t.name, "_f");
        assert_eq!(t.array_type_name.as_deref(), Some("__f"));
        Ok(())
    }
}
