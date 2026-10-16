//! Schema diff between two catalog snapshots (branch compare / merge preview). Lease CAT-27.
//! This is a PGArbor feature, not a PG one: `pgarbor branch diff a b` and the embedded
//! `Database::compare_branches` call it with the snapshots of two branch heads.
//!
//! Matching is by NAME, not OID (OIDs are per-branch counters): objects are identified by
//! `(kind, schema name, object name[, parent])`:
//! schemas by name; relations/sequences/indexes by `(schema, name)`; columns by `(relation, name)`;
//! constraints by `(table, name)`; types by `(schema, name)`; functions by `(schema, name,
//! IN-arg types as type NAMES)`; roles by name.
//!
//! The result is a flat, deterministic list ordered by (kind rank, qualified name) where kind rank
//! follows creation order (schema, type, sequence, table, column, index, constraint, view, function,
//! role) so a consumer can print or replay it:
//! - `Added` / `Removed`: the object exists only in `b` / only in `a`;
//! - `Modified`: same identity in both with differences; `details` lists them in a stable order, e.g.
//!   `type integer -> bigint`, `NOT NULL added`, `default (none) -> 0`, `owner postgres -> app`,
//!   `renamed`-style changes are NOT detected (a rename is Removed + Added).
//! Comparison inputs per kind: Relation (kind, persistence, owner, options, view query text,
//! columns via the column rules, `acl`), Column (type + typmod via `format_type`, not-null, default
//! text, identity/generated kind, collation name), Constraint (kind, columns by NAME, check text,
//! fk target by name and actions, deferrability), Index (method, uniqueness, key texts, predicate,
//! options), Sequence (all parameters), Type (enum labels in order / domain base+checks), Function
//! (everything but the OID), Role (attributes except password hash: only whether a password is set),
//! Schema (owner). Descriptions (comments) are compared as `comment` details.
//!
//! Implementation notes (CAT-27):
//! - Objects that exist only on one side are reported once. Their children (columns, indexes,
//!   constraints) are NOT listed separately when the parent table exists on one side only: replaying
//!   the parent's Added/Removed entry (CREATE/DROP TABLE) already creates or drops them.
//! - Column `NOT NULL` lives on the column (`attnotnull`); the implicit `pg_constraint` rows of kind
//!   'n' are therefore not reported as constraints.
//! - Expression texts (defaults, checks, predicates, view queries) are compared as stored; they are
//!   not re-deparsed here.
//! - Builtin objects are never part of `CatalogData`, so they are ignored by construction.
use crate::env::CatalogEnv;
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};
use pgarbor_types::names::format_type;
use std::collections::BTreeMap;

/// Kind of difference.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// One difference.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaChange {
    pub kind: ChangeKind,
    /// `schema`, `table`, `view`, `materialized view`, `sequence`, `index`, `column`, `constraint`,
    /// `type`, `domain`, `function`, `procedure`, `role` (words as `describe::type_description`).
    pub object_type: &'static str,
    /// Qualified identity: `public.t`, `public.t.col`, `public.t.t_pkey`, `public.f(integer)`.
    pub name: String,
    /// Human-readable details (module docs); empty for Added/Removed.
    pub details: Vec<String>,
    /// The address in snapshot `b` (Added/Modified) or `a` (Removed).
    pub address: ObjectAddress,
}

/// Differences turning snapshot `a` into snapshot `b` (module docs). Builtin objects are ignored.
// CONTRACT
pub fn diff_schemas(a: &CatalogSnapshot, b: &CatalogSnapshot) -> Result<Vec<SchemaChange>> {
    let env_a = CatalogEnv::with_default_path(a.clone());
    let env_b = CatalogEnv::with_default_path(b.clone());
    let sa = Side { snap: a, env: &env_a };
    let sb = Side { snap: b, env: &env_b };
    let mut out: Vec<Entry> = Vec::new();

    diff_namespaces(sa, sb, &mut out);
    diff_roles(sa, sb, &mut out);
    diff_types(sa, sb, &mut out)?;
    diff_sequences(sa, sb, &mut out)?;
    diff_relations(sa, sb, &mut out)?;
    diff_indexes(sa, sb, &mut out);
    diff_constraints(sa, sb, &mut out);
    diff_functions(sa, sb, &mut out)?;

    out.sort_by(|x, y| x.rank.cmp(&y.rank).then_with(|| x.change.name.cmp(&y.change.name)));
    Ok(out.into_iter().map(|e| e.change).collect())
}

/// One line per change (`+ table public.t`, `- column public.t.x`, `~ column public.t.y: type integer
/// -> bigint`) joined with `\n`; empty string when identical.
pub fn render_diff(changes: &[SchemaChange]) -> String {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let sigil = match c.kind {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
        };
        let mut line = format!("{sigil} {} {}", c.object_type, c.name);
        if !c.details.is_empty() {
            line.push_str(": ");
            line.push_str(&c.details.join("; "));
        }
        out.push(line);
    }
    out.join("\n")
}

// ----------------------------------------------------------------------------- plumbing

/// One side of the comparison: the snapshot plus an environment for `format_type`.
#[derive(Clone, Copy)]
struct Side<'a> {
    snap: &'a CatalogSnapshot,
    env: &'a CatalogEnv,
}

/// A pending output entry; `rank` is the kind rank used for the final sort.
struct Entry {
    rank: u8,
    change: SchemaChange,
}

/// Relation identity: (schema name, relation name).
type RelKey = (String, String);

fn emit(
    out: &mut Vec<Entry>,
    kind: ChangeKind,
    object_type: &'static str,
    name: String,
    details: Vec<String>,
    address: ObjectAddress,
) {
    out.push(Entry { rank: rank_of(object_type), change: SchemaChange { kind, object_type, name, details, address } });
}

/// Emit a `Modified` entry only when there is at least one detail.
fn emit_modified(
    out: &mut Vec<Entry>,
    object_type: &'static str,
    name: String,
    details: Vec<String>,
    address: ObjectAddress,
) {
    if !details.is_empty() {
        emit(out, ChangeKind::Modified, object_type, name, details, address);
    }
}

/// Kind rank (creation order) used for the output sort.
fn rank_of(object_type: &str) -> u8 {
    match object_type {
        "schema" => 0,
        "type" | "domain" => 1,
        "sequence" => 2,
        "table" => 3,
        "column" => 4,
        "index" => 5,
        "constraint" => 6,
        "view" | "materialized view" => 7,
        "function" | "procedure" => 8,
        "role" => 9,
        _ => 10,
    }
}

fn qualify(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

fn ns_name(snap: &CatalogSnapshot, oid: Oid) -> String {
    match snap.namespace(oid) {
        Some(n) => n.name.clone(),
        None => format!("schema {oid}"),
    }
}

/// Role name, or PG's `unknown (OID=n)` for a missing role.
fn role_label(snap: &CatalogSnapshot, oid: Oid) -> String {
    match snap.role_name(oid) {
        Some(n) => n.to_string(),
        None => format!("unknown (OID={oid})"),
    }
}

/// Qualified relation name (`schema.name`), or `<oid n>` for an unknown relation.
fn rel_qname(s: Side, oid: Oid) -> String {
    match s.snap.relation(oid) {
        Some(r) => qualify(&ns_name(s.snap, r.namespace), &r.name),
        None => format!("<oid {oid}>"),
    }
}

/// Key of the relation `oid` in its own snapshot, if it is a user relation.
fn rel_key_of(s: Side, oid: Oid) -> Option<RelKey> {
    s.snap.relation(oid).map(|r| (ns_name(s.snap, r.namespace), r.name.clone()))
}

fn comment_at(s: Side, classoid: Oid, objoid: Oid, objsubid: i32) -> Option<String> {
    s.snap.description(&DescriptionKey { objoid, classoid, objsubid }).map(str::to_string)
}

/// Name of a column by attnum (the attnum itself when unknown).
fn column_name(s: Side, table: Oid, attnum: i16) -> String {
    match s.snap.relation(table).and_then(|r| r.column(attnum)) {
        Some(c) => c.name.clone(),
        None => attnum.to_string(),
    }
}

/// `(a, b)` list of column names for attnums of `table`.
fn column_names(s: Side, table: Oid, attnums: &[i16]) -> String {
    let names: Vec<String> = attnums.iter().map(|n| column_name(s, table, *n)).collect();
    paren_text(&names)
}

/// Generic change detail `what old -> new`, only when the values differ.
fn change<T: PartialEq + std::fmt::Display>(d: &mut Vec<String>, what: &str, a: T, b: T) {
    if a != b {
        d.push(format!("{what} {a} -> {b}"));
    }
}

/// `what added` / `what removed` for a flag, only when it differs.
fn flag(d: &mut Vec<String>, what: &str, a: bool, b: bool) {
    if a != b {
        d.push(if b { format!("{what} added") } else { format!("{what} removed") });
    }
}

/// Comment detail, only when the comment differs.
fn comment_change(d: &mut Vec<String>, a: Option<&str>, b: Option<&str>) {
    if a != b {
        d.push(format!("comment {} -> {}", quote_comment(a), quote_comment(b)));
    }
}

fn quote_comment(c: Option<&str>) -> String {
    match c {
        Some(s) => format!("'{}'", s.replace('\'', "''")),
        None => "(none)".to_string(),
    }
}

fn owner_change(d: &mut Vec<String>, sa: Side, oa: Oid, sb: Side, ob: Oid) {
    change(d, "owner", role_label(sa.snap, oa), role_label(sb.snap, ob));
}

/// `(none)` for an empty list, else `(a, b)`.
fn paren_text(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        format!("({})", items.join(", "))
    }
}

/// `(none)` for an empty list, else `a, b`.
fn list_text(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

fn expr_text(e: Option<&StoredExpr>) -> String {
    match e {
        Some(e) => e.sql.clone(),
        None => "(none)".to_string(),
    }
}

/// PG `aclitem` role name quoting (`putid`): quoted unless all `[A-Za-z0-9_]`.
fn acl_role_text(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// ACL as text: `(default)` for NULL, `(none)` for an empty ACL, else `{grantee=privs/grantor,...}`.
fn acl_text(snap: &CatalogSnapshot, acl: Option<&Acl>) -> String {
    match acl {
        None => "(default)".to_string(),
        Some(items) if items.is_empty() => "(none)".to_string(),
        Some(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|i| i.to_text(&|o: Oid| acl_role_text(&role_label(snap, o))))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn persistence_word(p: Persistence) -> &'static str {
    match p {
        Persistence::Permanent => "permanent",
        Persistence::Unlogged => "unlogged",
        Persistence::Temp => "temporary",
    }
}

fn relation_word(kind: RelKind) -> &'static str {
    match kind {
        RelKind::View => "view",
        RelKind::MatView => "materialized view",
        RelKind::CompositeType => "type",
        RelKind::Sequence => "sequence",
        RelKind::Index | RelKind::PartitionedIndex => "index",
        _ => "table",
    }
}

fn collation_name(oid: Oid) -> String {
    match crate::builtin::builtin_collation(oid) {
        Some(c) => c.name.to_string(),
        None => oid.to_string(),
    }
}

fn language_name(oid: Oid) -> String {
    match crate::builtin::LANGUAGES.iter().find(|l| l.oid == oid) {
        Some(l) => l.name.to_string(),
        None => oid.to_string(),
    }
}

// ----------------------------------------------------------------------------- maps

fn ns_map(s: Side<'_>) -> BTreeMap<&str, &Namespace> {
    s.snap.data().namespaces.values().map(|n| (n.name.as_str(), n)).collect()
}

fn role_map(s: Side<'_>) -> BTreeMap<&str, &Role> {
    s.snap.data().roles.values().map(|r| (r.name.as_str(), r)).collect()
}

fn type_map(s: Side<'_>) -> BTreeMap<RelKey, &TypeObj> {
    s.snap
        .data()
        .types
        .values()
        .map(|t| ((ns_name(s.snap, t.namespace), t.name.clone()), t))
        .collect()
}

fn seq_map(s: Side<'_>) -> BTreeMap<RelKey, &Sequence> {
    s.snap
        .data()
        .sequences
        .values()
        .map(|q| ((ns_name(s.snap, q.namespace), q.name.clone()), q))
        .collect()
}

fn rel_map(s: Side<'_>) -> BTreeMap<RelKey, &Relation> {
    s.snap
        .data()
        .relations
        .values()
        .map(|r| ((ns_name(s.snap, r.namespace), r.name.clone()), r))
        .collect()
}

fn idx_map(s: Side<'_>) -> BTreeMap<RelKey, &Index> {
    s.snap
        .data()
        .indexes
        .values()
        .map(|i| ((ns_name(s.snap, i.namespace), i.name.clone()), i))
        .collect()
}

/// Table constraints (not NOT NULL, not domain constraints) keyed by (schema, table, name).
type ConKey = (String, String, String);

fn con_map(s: Side<'_>) -> BTreeMap<ConKey, &Constraint> {
    let mut m = BTreeMap::new();
    for c in s.snap.data().constraints.values() {
        if c.kind == ConstraintKind::NotNull || c.table == 0 {
            continue;
        }
        if let Some(r) = s.snap.relation(c.table) {
            m.insert((ns_name(s.snap, r.namespace), r.name.clone(), c.name.clone()), c);
        }
    }
    m
}

/// Function identity: (schema, `name(in-arg type names)`).
type FnKey = (String, String);

fn fn_map(s: Side<'_>) -> Result<BTreeMap<FnKey, &Function>> {
    let mut m = BTreeMap::new();
    for f in s.snap.data().functions.values() {
        let mut types: Vec<String> = Vec::new();
        for t in f.in_arg_types() {
            types.push(format_type(s.env, t, -1)?);
        }
        let sig = format!("{}({})", f.name, types.join(", "));
        m.entry((ns_name(s.snap, f.namespace), sig)).or_insert(f);
    }
    Ok(m)
}

// ----------------------------------------------------------------------------- schemas

fn diff_namespaces(sa: Side, sb: Side, out: &mut Vec<Entry>) {
    let ma = ns_map(sa);
    let mb = ns_map(sb);
    for (name, na) in &ma {
        let nb = match mb.get(name) {
            Some(n) => *n,
            None => {
                emit(out, ChangeKind::Removed, "schema", name.to_string(), Vec::new(), ObjectAddress::namespace(na.oid));
                continue;
            }
        };
        let mut d = Vec::new();
        owner_change(&mut d, sa, na.owner, sb, nb.owner);
        change(&mut d, "acl", acl_text(sa.snap, na.acl.as_ref()), acl_text(sb.snap, nb.acl.as_ref()));
        let ca = comment_at(sa, class_oid::PG_NAMESPACE, na.oid, 0);
        let cb = comment_at(sb, class_oid::PG_NAMESPACE, nb.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        emit_modified(out, "schema", name.to_string(), d, ObjectAddress::namespace(nb.oid));
    }
    for (name, nb) in &mb {
        if !ma.contains_key(name) {
            emit(out, ChangeKind::Added, "schema", name.to_string(), Vec::new(), ObjectAddress::namespace(nb.oid));
        }
    }
}

// ----------------------------------------------------------------------------- roles

fn diff_roles(sa: Side, sb: Side, out: &mut Vec<Entry>) {
    let ma = role_map(sa);
    let mb = role_map(sb);
    for (name, ra) in &ma {
        let rb = match mb.get(name) {
            Some(r) => *r,
            None => {
                emit(out, ChangeKind::Removed, "role", name.to_string(), Vec::new(), ObjectAddress::new(ObjClass::Role, ra.oid));
                continue;
            }
        };
        let mut d = Vec::new();
        flag(&mut d, "SUPERUSER", ra.superuser, rb.superuser);
        flag(&mut d, "INHERIT", ra.inherit, rb.inherit);
        flag(&mut d, "CREATEROLE", ra.create_role, rb.create_role);
        flag(&mut d, "CREATEDB", ra.create_db, rb.create_db);
        flag(&mut d, "LOGIN", ra.can_login, rb.can_login);
        flag(&mut d, "REPLICATION", ra.replication, rb.replication);
        flag(&mut d, "BYPASSRLS", ra.bypass_rls, rb.bypass_rls);
        change(&mut d, "connection limit", ra.conn_limit, rb.conn_limit);
        // Only whether a password is set is compared, never the hash.
        flag(&mut d, "password", ra.password.is_some(), rb.password.is_some());
        if ra.valid_until != rb.valid_until {
            d.push("valid until changed".to_string());
        }
        change(&mut d, "settings", list_text(&ra.config), list_text(&rb.config));
        let ca = comment_at(sa, class_oid::PG_AUTHID, ra.oid, 0);
        let cb = comment_at(sb, class_oid::PG_AUTHID, rb.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        emit_modified(out, "role", name.to_string(), d, ObjectAddress::new(ObjClass::Role, rb.oid));
    }
    for (name, rb) in &mb {
        if !ma.contains_key(name) {
            emit(out, ChangeKind::Added, "role", name.to_string(), Vec::new(), ObjectAddress::new(ObjClass::Role, rb.oid));
        }
    }
}

// ----------------------------------------------------------------------------- types

fn type_word(t: &TypeObj) -> &'static str {
    if t.is_domain() {
        "domain"
    } else {
        "type"
    }
}

fn type_kind_word(t: &TypeObj) -> &'static str {
    if t.is_domain() {
        "domain"
    } else {
        "enum"
    }
}

/// Domain CHECK constraints by name (NOT NULL is the domain's `not_null` flag).
fn named_checks(list: Vec<&Constraint>) -> BTreeMap<&str, &Constraint> {
    list.into_iter().filter(|c| c.kind != ConstraintKind::NotNull).map(|c| (c.name.as_str(), c)).collect()
}

fn domain_check_changes(sa: Side, da: Oid, sb: Side, db: Oid, d: &mut Vec<String>) {
    let ca = named_checks(sa.snap.domain_constraints(da));
    let cb = named_checks(sb.snap.domain_constraints(db));
    for (name, x) in &ca {
        match cb.get(name) {
            None => d.push(format!("check {name} removed")),
            Some(y) => change(d, &format!("check {name}"), expr_text(x.check.as_ref()), expr_text(y.check.as_ref())),
        }
    }
    for name in cb.keys() {
        if !ca.contains_key(name) {
            d.push(format!("check {name} added"));
        }
    }
}

fn diff_types(sa: Side, sb: Side, out: &mut Vec<Entry>) -> Result<()> {
    let ma = type_map(sa);
    let mb = type_map(sb);
    for (key, ta) in &ma {
        let tb = match mb.get(key) {
            Some(t) => *t,
            None => {
                emit(out, ChangeKind::Removed, type_word(ta), qualify(&key.0, &key.1), Vec::new(), ObjectAddress::type_(ta.oid));
                continue;
            }
        };
        let mut d = Vec::new();
        change(&mut d, "kind", type_kind_word(ta), type_kind_word(tb));
        owner_change(&mut d, sa, ta.owner, sb, tb.owner);
        change(&mut d, "acl", acl_text(sa.snap, ta.acl.as_ref()), acl_text(sb.snap, tb.acl.as_ref()));
        let ca = comment_at(sa, class_oid::PG_TYPE, ta.oid, 0);
        let cb = comment_at(sb, class_oid::PG_TYPE, tb.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        match (&ta.def, &tb.def) {
            (TypeDef::Enum { labels: la }, TypeDef::Enum { labels: lb }) => {
                let na: Vec<String> = la.iter().map(|l| l.label.clone()).collect();
                let nb: Vec<String> = lb.iter().map(|l| l.label.clone()).collect();
                change(&mut d, "labels", paren_text(&na), paren_text(&nb));
            }
            (
                TypeDef::Domain { base: ba, not_null: nn_a, default: da, .. },
                TypeDef::Domain { base: bb, not_null: nn_b, default: db, .. },
            ) => {
                let xa = format_type(sa.env, ba.oid, ba.typmod)?;
                let xb = format_type(sb.env, bb.oid, bb.typmod)?;
                change(&mut d, "base type", xa, xb);
                flag(&mut d, "NOT NULL", *nn_a, *nn_b);
                change(&mut d, "default", expr_text(da.as_ref()), expr_text(db.as_ref()));
                domain_check_changes(sa, ta.oid, sb, tb.oid, &mut d);
            }
            _ => {}
        }
        emit_modified(out, type_word(tb), qualify(&key.0, &key.1), d, ObjectAddress::type_(tb.oid));
    }
    for (key, tb) in &mb {
        if !ma.contains_key(key) {
            emit(out, ChangeKind::Added, type_word(tb), qualify(&key.0, &key.1), Vec::new(), ObjectAddress::type_(tb.oid));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------- sequences

fn owned_by_text(s: Side, owned: Option<(Oid, i16)>) -> String {
    match owned {
        None => "(none)".to_string(),
        Some((table, attnum)) => format!("{}.{}", rel_qname(s, table), column_name(s, table, attnum)),
    }
}

fn diff_sequences(sa: Side, sb: Side, out: &mut Vec<Entry>) -> Result<()> {
    let ma = seq_map(sa);
    let mb = seq_map(sb);
    for (key, x) in &ma {
        let y = match mb.get(key) {
            Some(q) => *q,
            None => {
                emit(out, ChangeKind::Removed, "sequence", qualify(&key.0, &key.1), Vec::new(), ObjectAddress::relation(x.oid));
                continue;
            }
        };
        let mut d = Vec::new();
        owner_change(&mut d, sa, x.owner, sb, y.owner);
        change(&mut d, "acl", acl_text(sa.snap, x.acl.as_ref()), acl_text(sb.snap, y.acl.as_ref()));
        change(&mut d, "persistence", persistence_word(x.persistence), persistence_word(y.persistence));
        let ta = format_type(sa.env, x.type_oid, -1)?;
        let tb = format_type(sb.env, y.type_oid, -1)?;
        change(&mut d, "data type", ta, tb);
        change(&mut d, "start", x.start, y.start);
        change(&mut d, "increment", x.increment, y.increment);
        change(&mut d, "minimum value", x.min, y.min);
        change(&mut d, "maximum value", x.max, y.max);
        change(&mut d, "cache", x.cache, y.cache);
        flag(&mut d, "CYCLE", x.cycle, y.cycle);
        flag(&mut d, "identity", x.identity, y.identity);
        change(&mut d, "owned by", owned_by_text(sa, x.owned_by), owned_by_text(sb, y.owned_by));
        let ca = comment_at(sa, class_oid::PG_CLASS, x.oid, 0);
        let cb = comment_at(sb, class_oid::PG_CLASS, y.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        emit_modified(out, "sequence", qualify(&key.0, &key.1), d, ObjectAddress::relation(y.oid));
    }
    for (key, y) in &mb {
        if !ma.contains_key(key) {
            emit(out, ChangeKind::Added, "sequence", qualify(&key.0, &key.1), Vec::new(), ObjectAddress::relation(y.oid));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------- relations and columns

/// Comparison of one column present in both relations.
fn column_details(sa: Side, x: &Column, sb: Side, y: &Column) -> Result<Vec<String>> {
    let mut d = Vec::new();
    let tx = format_type(sa.env, x.type_id.oid, x.type_id.typmod)?;
    let ty = format_type(sb.env, y.type_id.oid, y.type_id.typmod)?;
    change(&mut d, "type", tx, ty);
    flag(&mut d, "NOT NULL", x.not_null, y.not_null);
    change(&mut d, "default", expr_text(x.default.as_ref().map(|c| &c.expr)), expr_text(y.default.as_ref().map(|c| &c.expr)));
    change(&mut d, "identity", identity_word(x.identity.as_ref().map(|i| i.kind)), identity_word(y.identity.as_ref().map(|i| i.kind)));
    change(&mut d, "generated", generated_word(x.generated), generated_word(y.generated));
    change(&mut d, "collation", collation_name(x.collation), collation_name(y.collation));
    Ok(d)
}

fn identity_word(kind: Option<IdentityKind>) -> &'static str {
    match kind {
        None => "(none)",
        Some(IdentityKind::Always) => "always",
        Some(IdentityKind::ByDefault) => "by default",
    }
}

fn generated_word(kind: Option<GeneratedKind>) -> &'static str {
    match kind {
        None => "(none)",
        Some(GeneratedKind::Stored) => "stored",
        Some(GeneratedKind::Virtual) => "virtual",
    }
}

fn diff_relations(sa: Side, sb: Side, out: &mut Vec<Entry>) -> Result<()> {
    let ma = rel_map(sa);
    let mb = rel_map(sb);
    for (key, ra) in &ma {
        let rb = match mb.get(key) {
            Some(r) => *r,
            None => {
                emit(out, ChangeKind::Removed, relation_word(ra.kind), qualify(&key.0, &key.1), Vec::new(), ObjectAddress::relation(ra.oid));
                continue;
            }
        };
        let qname = qualify(&key.0, &key.1);
        let mut d = Vec::new();
        change(&mut d, "kind", ra.kind.description_word(), rb.kind.description_word());
        change(&mut d, "persistence", persistence_word(ra.persistence), persistence_word(rb.persistence));
        owner_change(&mut d, sa, ra.owner, sb, rb.owner);
        change(&mut d, "options", list_text(&ra.options), list_text(&rb.options));
        if let (Some(va), Some(vb)) = (&ra.view, &rb.view) {
            if va.query.sql != vb.query.sql {
                d.push("query changed".to_string());
            }
        }
        change(&mut d, "acl", acl_text(sa.snap, ra.acl.as_ref()), acl_text(sb.snap, rb.acl.as_ref()));
        let ca = comment_at(sa, class_oid::PG_CLASS, ra.oid, 0);
        let cb = comment_at(sb, class_oid::PG_CLASS, rb.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());

        let cols_a: BTreeMap<&str, &Column> = ra.columns.iter().filter(|c| !c.dropped).map(|c| (c.name.as_str(), c)).collect();
        let cols_b: BTreeMap<&str, &Column> = rb.columns.iter().filter(|c| !c.dropped).map(|c| (c.name.as_str(), c)).collect();
        for (name, x) in &cols_a {
            match cols_b.get(name) {
                None => {
                    d.push(format!("column {name} removed"));
                    emit(out, ChangeKind::Removed, "column", format!("{qname}.{name}"), Vec::new(), ObjectAddress::column(ra.oid, x.attnum));
                }
                Some(y) => {
                    let mut cd = column_details(sa, x, sb, y)?;
                    let cca = comment_at(sa, class_oid::PG_CLASS, ra.oid, x.attnum as i32);
                    let ccb = comment_at(sb, class_oid::PG_CLASS, rb.oid, y.attnum as i32);
                    comment_change(&mut cd, cca.as_deref(), ccb.as_deref());
                    emit_modified(out, "column", format!("{qname}.{name}"), cd, ObjectAddress::column(rb.oid, y.attnum));
                }
            }
        }
        for (name, y) in &cols_b {
            if !cols_a.contains_key(name) {
                d.push(format!("column {name} added"));
                emit(out, ChangeKind::Added, "column", format!("{qname}.{name}"), Vec::new(), ObjectAddress::column(rb.oid, y.attnum));
            }
        }
        emit_modified(out, relation_word(rb.kind), qname, d, ObjectAddress::relation(rb.oid));
    }
    for (key, rb) in &mb {
        if !ma.contains_key(key) {
            emit(out, ChangeKind::Added, relation_word(rb.kind), qualify(&key.0, &key.1), Vec::new(), ObjectAddress::relation(rb.oid));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------- indexes

fn parallel_word(p: u8) -> &'static str {
    match p {
        b's' => "safe",
        b'r' => "restricted",
        _ => "unsafe",
    }
}

/// One index key as text: expression text or column name, then DESC / NULLS per PG defaults.
fn index_key_text(s: Side, table: Oid, k: &IndexKey) -> String {
    let mut text = match (&k.expr, k.column) {
        (Some(e), _) => e.sql.clone(),
        (None, Some(col)) => column_name(s, table, col),
        (None, None) => k.name.clone(),
    };
    if k.descending {
        text.push_str(" DESC");
        if !k.nulls_first {
            text.push_str(" NULLS LAST");
        }
    } else if k.nulls_first {
        text.push_str(" NULLS FIRST");
    }
    text
}

/// `(key, key) INCLUDE (col)` text of an index.
fn index_keys_text(s: Side, idx: &Index) -> String {
    let texts: Vec<String> = idx.keys.iter().map(|k| index_key_text(s, idx.table, k)).collect();
    let n = (idx.n_key_columns as usize).min(texts.len());
    let (keys, include) = texts.split_at(n);
    let mut out = format!("({})", keys.join(", "));
    if !include.is_empty() {
        out.push_str(&format!(" INCLUDE ({})", include.join(", ")));
    }
    out
}

fn diff_indexes(sa: Side, sb: Side, out: &mut Vec<Entry>) {
    let ia = idx_map(sa);
    let ib = idx_map(sb);
    let ra = rel_map(sa);
    let rb = rel_map(sb);
    for (key, x) in &ia {
        let qname = qualify(&key.0, &key.1);
        let y = match ib.get(key) {
            Some(i) => *i,
            None => {
                // Only report when the parent table exists on both sides (else it is a child of an
                // Added/Removed table).
                let parent_in_both = rel_key_of(sa, x.table).is_some_and(|k| rb.contains_key(&k));
                if parent_in_both {
                    emit(out, ChangeKind::Removed, "index", qname, Vec::new(), ObjectAddress::relation(x.oid));
                }
                continue;
            }
        };
        let mut d = Vec::new();
        change(&mut d, "table", rel_qname(sa, x.table), rel_qname(sb, y.table));
        change(&mut d, "method", x.method.name(), y.method.name());
        flag(&mut d, "UNIQUE", x.unique, y.unique);
        flag(&mut d, "NULLS NOT DISTINCT", x.nulls_not_distinct, y.nulls_not_distinct);
        change(&mut d, "keys", index_keys_text(sa, x), index_keys_text(sb, y));
        change(&mut d, "predicate", expr_text(x.predicate.as_ref()), expr_text(y.predicate.as_ref()));
        change(&mut d, "options", list_text(&x.options), list_text(&y.options));
        let ca = comment_at(sa, class_oid::PG_CLASS, x.oid, 0);
        let cb = comment_at(sb, class_oid::PG_CLASS, y.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        emit_modified(out, "index", qname, d, ObjectAddress::relation(y.oid));
    }
    for (key, y) in &ib {
        if !ia.contains_key(key) {
            let parent_in_both = rel_key_of(sb, y.table).is_some_and(|k| ra.contains_key(&k));
            if parent_in_both {
                emit(out, ChangeKind::Added, "index", qualify(&key.0, &key.1), Vec::new(), ObjectAddress::relation(y.oid));
            }
        }
    }
}

// ----------------------------------------------------------------------------- constraints

fn con_kind_word(kind: ConstraintKind) -> &'static str {
    match kind {
        ConstraintKind::Check => "check",
        ConstraintKind::ForeignKey => "foreign key",
        ConstraintKind::NotNull => "not null",
        ConstraintKind::PrimaryKey => "primary key",
        ConstraintKind::Unique => "unique",
        ConstraintKind::Trigger => "trigger",
        ConstraintKind::Exclusion => "exclusion",
    }
}

/// `table(col, ...)` text of a foreign key's referenced side, or `(none)`.
fn fk_target_text(s: Side, fk: Option<&ForeignKey>) -> String {
    match fk {
        None => "(none)".to_string(),
        Some(f) => format!("{}{}", rel_qname(s, f.ref_table), column_names(s, f.ref_table, &f.ref_columns)),
    }
}

fn fk_action_word(a: Option<FkAction>) -> &'static str {
    match a {
        None => "(none)",
        Some(a) => a.sql(),
    }
}

fn fk_match_word(m: Option<FkMatch>) -> &'static str {
    match m {
        None => "(none)",
        Some(m) => m.info_schema(),
    }
}

fn diff_constraints(sa: Side, sb: Side, out: &mut Vec<Entry>) {
    let ca = con_map(sa);
    let cb = con_map(sb);
    let ra = rel_map(sa);
    let rb = rel_map(sb);
    for (key, x) in &ca {
        let qname = qualify3(key);
        let parent: RelKey = (key.0.clone(), key.1.clone());
        let y = match cb.get(key) {
            Some(c) => *c,
            None => {
                if rb.contains_key(&parent) {
                    emit(out, ChangeKind::Removed, "constraint", qname, Vec::new(), ObjectAddress::constraint(x.oid));
                }
                continue;
            }
        };
        let mut d = Vec::new();
        change(&mut d, "kind", con_kind_word(x.kind), con_kind_word(y.kind));
        change(&mut d, "columns", column_names(sa, x.table, &x.columns), column_names(sb, y.table, &y.columns));
        change(&mut d, "check", expr_text(x.check.as_ref()), expr_text(y.check.as_ref()));
        change(&mut d, "references", fk_target_text(sa, x.fk.as_ref()), fk_target_text(sb, y.fk.as_ref()));
        change(&mut d, "on update", fk_action_word(x.fk.as_ref().map(|f| f.on_update)), fk_action_word(y.fk.as_ref().map(|f| f.on_update)));
        change(&mut d, "on delete", fk_action_word(x.fk.as_ref().map(|f| f.on_delete)), fk_action_word(y.fk.as_ref().map(|f| f.on_delete)));
        change(&mut d, "match", fk_match_word(x.fk.as_ref().map(|f| f.match_type)), fk_match_word(y.fk.as_ref().map(|f| f.match_type)));
        flag(&mut d, "DEFERRABLE", x.deferrable, y.deferrable);
        flag(&mut d, "INITIALLY DEFERRED", x.initially_deferred, y.initially_deferred);
        let cmt_a = comment_at(sa, class_oid::PG_CONSTRAINT, x.oid, 0);
        let cmt_b = comment_at(sb, class_oid::PG_CONSTRAINT, y.oid, 0);
        comment_change(&mut d, cmt_a.as_deref(), cmt_b.as_deref());
        emit_modified(out, "constraint", qname, d, ObjectAddress::constraint(y.oid));
    }
    for (key, y) in &cb {
        if !ca.contains_key(key) {
            let parent: RelKey = (key.0.clone(), key.1.clone());
            if ra.contains_key(&parent) {
                emit(out, ChangeKind::Added, "constraint", qualify3(key), Vec::new(), ObjectAddress::constraint(y.oid));
            }
        }
    }
}

fn qualify3(key: &ConKey) -> String {
    format!("{}.{}.{}", key.0, key.1, key.2)
}

// ----------------------------------------------------------------------------- functions

fn func_kind_word(kind: FuncKind) -> &'static str {
    match kind {
        FuncKind::Function => "function",
        FuncKind::Aggregate => "aggregate",
        FuncKind::Window => "window",
        FuncKind::Procedure => "procedure",
    }
}

fn volatility_word(v: FuncVolatility) -> &'static str {
    match v {
        FuncVolatility::Immutable => "immutable",
        FuncVolatility::Stable => "stable",
        FuncVolatility::Volatile => "volatile",
    }
}

/// `(mode name type DEFAULT expr, ...)` of a function's full parameter list.
fn arg_list_text(s: Side, f: &Function) -> Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(f.args.len());
    for arg in &f.args {
        let mut p = String::new();
        p.push_str(match arg.mode {
            ArgMode::In => "",
            ArgMode::Out => "OUT ",
            ArgMode::InOut => "INOUT ",
            ArgMode::Variadic => "VARIADIC ",
            ArgMode::Table => "TABLE ",
        });
        if !arg.name.is_empty() {
            p.push_str(&arg.name);
            p.push(' ');
        }
        p.push_str(&format_type(s.env, arg.type_oid, -1)?);
        if let Some(def) = &arg.default {
            p.push_str(" DEFAULT ");
            p.push_str(&def.sql);
        }
        parts.push(p);
    }
    Ok(format!("({})", parts.join(", ")))
}

fn diff_functions(sa: Side, sb: Side, out: &mut Vec<Entry>) -> Result<()> {
    let ma = fn_map(sa)?;
    let mb = fn_map(sb)?;
    for (key, x) in &ma {
        let qname = qualify(&key.0, &key.1);
        let y = match mb.get(key) {
            Some(f) => *f,
            None => {
                let word = if x.kind == FuncKind::Procedure { "procedure" } else { "function" };
                emit(out, ChangeKind::Removed, word, qname, Vec::new(), ObjectAddress::proc(x.oid));
                continue;
            }
        };
        let mut d = Vec::new();
        change(&mut d, "kind", func_kind_word(x.kind), func_kind_word(y.kind));
        owner_change(&mut d, sa, x.owner, sb, y.owner);
        change(&mut d, "language", language_name(x.language), language_name(y.language));
        change(&mut d, "arguments", arg_list_text(sa, x)?, arg_list_text(sb, y)?);
        change(&mut d, "returns", format_type(sa.env, x.ret, -1)?, format_type(sb.env, y.ret, -1)?);
        flag(&mut d, "SETOF", x.retset, y.retset);
        change(&mut d, "volatility", volatility_word(x.volatility), volatility_word(y.volatility));
        flag(&mut d, "STRICT", x.strict, y.strict);
        flag(&mut d, "SECURITY DEFINER", x.security_definer, y.security_definer);
        flag(&mut d, "LEAKPROOF", x.leakproof, y.leakproof);
        change(&mut d, "parallel", parallel_word(x.parallel), parallel_word(y.parallel));
        change(&mut d, "cost", x.cost, y.cost);
        change(&mut d, "rows", x.rows, y.rows);
        if x.body != y.body {
            d.push("body changed".to_string());
        }
        if expr_text(x.sql_body.as_ref()) != expr_text(y.sql_body.as_ref()) {
            d.push("sql body changed".to_string());
        }
        change(&mut d, "settings", list_text(&x.config), list_text(&y.config));
        change(&mut d, "acl", acl_text(sa.snap, x.acl.as_ref()), acl_text(sb.snap, y.acl.as_ref()));
        let ca = comment_at(sa, class_oid::PG_PROC, x.oid, 0);
        let cb = comment_at(sb, class_oid::PG_PROC, y.oid, 0);
        comment_change(&mut d, ca.as_deref(), cb.as_deref());
        let word = if y.kind == FuncKind::Procedure { "procedure" } else { "function" };
        emit_modified(out, word, qname, d, ObjectAddress::proc(y.oid));
    }
    for (key, y) in &mb {
        if !ma.contains_key(key) {
            let word = if y.kind == FuncKind::Procedure { "procedure" } else { "function" };
            emit(out, ChangeKind::Added, word, qualify(&key.0, &key.1), Vec::new(), ObjectAddress::proc(y.oid));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::CatalogData;
    use crate::persist::CatalogVersion;
    use pgarbor_types::TypeId;

    const PUBLIC: Oid = 2200;

    fn snapshot(rels: Vec<Relation>) -> CatalogSnapshot {
        let mut data = CatalogData::default();
        data.insert(CatalogObject::Namespace(Namespace { oid: PUBLIC, name: "public".to_string(), owner: 6171, acl: None }));
        for r in rels {
            data.insert(CatalogObject::Relation(r));
        }
        CatalogSnapshot::build(CatalogVersion::ZERO, data).expect("build test snapshot")
    }

    fn col(attnum: i16, name: &str, t: TypeId) -> Column {
        Column::new(attnum, name, t, 100)
    }

    fn table(oid: Oid, name: &str, columns: Vec<Column>) -> Relation {
        Relation {
            oid,
            name: name.to_string(),
            namespace: PUBLIC,
            kind: RelKind::Table,
            owner: 10,
            persistence: Persistence::Permanent,
            columns,
            rowtype: oid + 1,
            array_type: oid + 2,
            array_type_name: None,
            options: Vec::new(),
            acl: None,
            replica_identity: ReplicaIdentity::Default,
            row_security: false,
            force_row_security: false,
            policies: Vec::new(),
            rules: Vec::new(),
            view: None,
            partition_key: None,
            partition_of: None,
            inherits: Vec::new(),
        }
    }

    #[test]
    fn identical_snapshots_have_no_changes() {
        let a = snapshot(vec![table(16384, "t", vec![col(1, "id", TypeId::INT4)])]);
        let b = snapshot(vec![table(16384, "t", vec![col(1, "id", TypeId::INT4)])]);
        assert!(diff_schemas(&a, &b).expect("diff").is_empty());
    }

    #[test]
    fn column_added_reports_table_and_column() {
        let a = snapshot(vec![table(16384, "t", vec![col(1, "id", TypeId::INT4)])]);
        let b = snapshot(vec![table(16384, "t", vec![col(1, "id", TypeId::INT4), col(2, "x", TypeId::INT4)])]);
        let changes = diff_schemas(&a, &b).expect("diff");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].object_type, "table");
        assert_eq!(changes[0].name, "public.t");
        assert_eq!(changes[0].details, vec!["column x added".to_string()]);
        assert_eq!(changes[1].kind, ChangeKind::Added);
        assert_eq!(changes[1].object_type, "column");
        assert_eq!(changes[1].name, "public.t.x");
        assert!(changes[1].details.is_empty());
        assert_eq!(changes[1].address, ObjectAddress::column(16384, 2));
    }

    #[test]
    fn removed_table_and_type_change_render() {
        let a = snapshot(vec![
            table(16384, "t", vec![col(1, "c", TypeId::INT4)]),
            table(16390, "u", vec![col(1, "c", TypeId::INT4)]),
        ]);
        let b = snapshot(vec![table(16384, "t", vec![col(1, "c", TypeId::INT8)])]);
        let changes = diff_schemas(&a, &b).expect("diff");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].kind, ChangeKind::Removed);
        assert_eq!(changes[0].object_type, "table");
        assert_eq!(changes[0].name, "public.u");
        assert_eq!(changes[1].kind, ChangeKind::Modified);
        assert_eq!(changes[1].object_type, "column");
        assert_eq!(changes[1].name, "public.t.c");
        assert_eq!(changes[1].details, vec!["type integer -> bigint".to_string()]);
        assert_eq!(render_diff(&changes), "- table public.u\n~ column public.t.c: type integer -> bigint");
    }

    #[test]
    fn not_null_added_is_a_column_detail() {
        let a = snapshot(vec![table(16384, "t", vec![col(1, "c", TypeId::INT4)])]);
        let mut nn = col(1, "c", TypeId::INT4);
        nn.not_null = true;
        let b = snapshot(vec![table(16384, "t", vec![nn])]);
        let changes = diff_schemas(&a, &b).expect("diff");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].object_type, "column");
        assert_eq!(changes[0].details, vec!["NOT NULL added".to_string()]);
    }

    #[test]
    fn added_tables_are_sorted_by_name() {
        let a = snapshot(Vec::new());
        let b = snapshot(vec![
            table(16390, "b", vec![col(1, "c", TypeId::INT4)]),
            table(16384, "a", vec![col(1, "c", TypeId::INT4)]),
        ]);
        let changes = diff_schemas(&a, &b).expect("diff");
        assert_eq!(render_diff(&changes), "+ table public.a\n+ table public.b");
    }
}
