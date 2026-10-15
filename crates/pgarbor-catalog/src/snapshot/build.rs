//! Snapshot construction: merge builtins with persisted objects and build every secondary index of
//! `SnapshotIndexes`. Lease CAT-02.
//!
//! ## `CatalogSnapshot::build(version, data)`
//! 1. **Namespaces**: insert the builtin schemas from `builtin::NAMESPACES` EXCEPT `public` (2200)
//!    as `Namespace{oid, name, owner, acl: parse_acl_text(acl)}`; then every `data.namespaces`
//!    entry (persisted `public` included). `namespace_by_name` maps name → oid.
//! 2. **Roles**: builtin `builtin::ROLES` except `postgres` (10) as `Role` (password None,
//!    valid_until None, config empty), then `data.roles`. `role_by_name`.
//! 3. **Databases**: builtin `builtin::DATABASES` except `postgres` (5) as `Database`
//!    (acl parsed, locale from the row, config empty), then `data.databases`. `database_by_name`.
//! 4. **pg_class names** (`class_by_name`): every `builtin::SYSTEM_RELS` row
//!    ((namespace, name) → `Relation(oid)` for kinds r/v, `SystemIndex(oid)` for kind i); then user
//!    relations, indexes, sequences. A duplicate (namespace, name) among persisted objects is
//!    corruption: XX001 `duplicate catalog entry for relation "<ns oid>.<name>"`.
//! 5. **User types** (`type_by_name`, `user_types`): for each `TypeObj`: (ns, name) → oid
//!    (`UserTypeRef::Type`), and its array (ns, `array_name`) → array_type (`TypeArray(oid)`), where
//!    the array name is `obj.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&name))`
//!    (CAT-06; the writer stores a non-canonical name only after a collision).
//!    For each relation with `kind.has_rowtype()`: (ns, relname) → rowtype (`RowType(rel oid)`) and
//!    the array name (same rule) → array_type (`RowTypeArray(rel oid)`). A duplicate (ns, typname)
//!    among persisted objects → XX001 `duplicate catalog entry for type "<ns oid>.<name>"`.
//! 6. **Per-table lists**: `indexes_by_table` (index.table), `constraints_by_table`
//!    (constraint.table != 0), `constraints_by_domain` (constraint.domain != 0), `fks_referencing`
//!    (`fk.ref_table`), `sequences_by_owner` (`owned_by`), all vectors sorted ascending by OID.
//! 7. **Functions**: `functions_by_name` ((namespace, name) → oids sorted).
//! 8. **Reverse maps**: `enum_label_owner` (each `EnumLabelDef.oid` → type oid), `attrdef_owner`
//!    (each `Column.default.oid` → (rel, attnum)).
//! 9. **Validation (cheap, fail closed)**: every object's `namespace` exists, every index/constraint
//!    `table` exists, otherwise XX001 `catalog is corrupt: <kind> <oid> references missing <what> <oid>`.
//!    Unknown roles as owners are NOT errors (PG prints `unknown (OID=n)`).
//!
//! Complexity: O(n log n) in the number of objects; called once per catalog version.
//!
//! ## `parse_acl_text`
//! Parses PG `aclitem[]` text (`{postgres=UC/postgres,=U/postgres}`, also with `*` grant options and
//! double-quoted role names) into `Acl`, resolving role names through `role_oid`. `None`/empty input →
//! Ok(None); `{}` → Some(empty). Unknown role name → XX000 `unknown role "<name>" in builtin acl`.
use super::core::*;
use crate::builtin;
use crate::data::CatalogData;
use crate::model::{Acl, AclItem, Database, Namespace, Role, TypeDef, ACL_ALL_RIGHTS_STR};
use crate::naming;
use crate::persist::CatalogVersion;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use std::collections::HashMap;

fn corrupt(kind: &str, oid: Oid, what: &str, target: Oid) -> PgError {
    PgError::new(
        SqlState::DATA_CORRUPTED,
        format!("catalog is corrupt: {kind} {oid} references missing {what} {target}"),
    )
}

fn insert_class(idx: &mut SnapshotIndexes, ns: Oid, name: &str, c: ClassRef) -> Result<()> {
    if idx.class_by_name.insert((ns, name.to_string()), c).is_some() {
        return Err(PgError::new(
            SqlState::DATA_CORRUPTED,
            format!("duplicate catalog entry for relation \"{ns}.{name}\""),
        ));
    }
    Ok(())
}

fn insert_type(idx: &mut SnapshotIndexes, ns: Oid, name: &str, oid: Oid, r: UserTypeRef) -> Result<()> {
    if idx.type_by_name.insert((ns, name.to_string()), oid).is_some() {
        return Err(PgError::new(
            SqlState::DATA_CORRUPTED,
            format!("duplicate catalog entry for type \"{ns}.{name}\""),
        ));
    }
    idx.user_types.insert(oid, r);
    Ok(())
}

// Bound all recursive metadata resolution, including domain -> array -> element edges.
// See accepted amendment pgarbor-catalog-06-domain-graph-safety.
const MAX_TYPE_RESOLUTION_DEPTH: usize = 64;

fn validate_type_graph(data: &CatalogData, idx: &SnapshotIndexes) -> Result<()> {
    for &start in idx.user_types.keys() {
        let mut seen = std::collections::HashSet::new();
        let mut oid = start;
        loop {
            if pgarbor_types::catalog_data::builtin_type(oid).is_some() {
                break;
            }
            if !seen.insert(oid) {
                return Err(PgError::corruption(format!("catalog is corrupt: type {start} has a cyclic base type")));
            }
            if seen.len() > MAX_TYPE_RESOLUTION_DEPTH {
                return Err(PgError::corruption(format!("catalog is corrupt: type {start} exceeds the type resolution depth limit")));
            }
            match idx.user_types.get(&oid) {
                Some(UserTypeRef::Type(t)) => {
                    let ty = data.types.get(t).ok_or_else(|| corrupt("type", start, "type", *t))?;
                    match &ty.def {
                        TypeDef::Domain { base, .. } => oid = base.oid,
                        TypeDef::Enum { .. } => break,
                    }
                }
                Some(UserTypeRef::TypeArray(element)) => oid = *element,
                Some(UserTypeRef::RowType(_)) | Some(UserTypeRef::RowTypeArray(_)) => break,
                None => return Err(corrupt("type", start, "base type", oid)),
            }
        }
    }
    Ok(())
}

fn sort_lists<K>(map: &mut HashMap<K, Vec<Oid>>) {
    for v in map.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
}

impl CatalogSnapshot {
    /// Build a snapshot (see module docs).
    // CONTRACT
    pub fn build(version: CatalogVersion, data: CatalogData) -> Result<CatalogSnapshot> {
        let builtin_role_oid = |name: &str| builtin::ROLES.iter().find(|r| r.name == name).map(|r| r.oid);
        let mut idx = SnapshotIndexes::default();

        // 1. namespaces
        for n in builtin::NAMESPACES.iter().filter(|n| n.oid != builtin::PUBLIC_NAMESPACE) {
            let acl = parse_acl_text(n.acl, &builtin_role_oid)?;
            idx.namespaces.insert(n.oid, Namespace { oid: n.oid, name: n.name.to_string(), owner: n.owner, acl });
        }
        for n in data.namespaces.values() {
            idx.namespaces.insert(n.oid, n.clone());
        }
        for n in idx.namespaces.values() {
            idx.namespace_by_name.insert(n.name.clone(), n.oid);
        }

        // 2. roles
        for r in builtin::ROLES.iter().filter(|r| r.oid != builtin::BOOTSTRAP_SUPERUSER) {
            idx.roles.insert(
                r.oid,
                Role {
                    oid: r.oid,
                    name: r.name.to_string(),
                    superuser: r.superuser,
                    inherit: r.inherit,
                    create_role: r.create_role,
                    create_db: r.create_db,
                    can_login: r.can_login,
                    replication: r.replication,
                    bypass_rls: r.bypass_rls,
                    conn_limit: r.conn_limit,
                    password: None,
                    valid_until: None,
                    config: Vec::new(),
                },
            );
        }
        for r in data.roles.values() {
            idx.roles.insert(r.oid, r.clone());
        }
        for r in idx.roles.values() {
            idx.role_by_name.insert(r.name.clone(), r.oid);
        }

        // 3. databases
        for d in builtin::DATABASES.iter().filter(|d| d.oid != builtin::POSTGRES_DATABASE) {
            let acl = parse_acl_text(d.acl, &builtin_role_oid)?;
            idx.databases.insert(
                d.oid,
                Database {
                    oid: d.oid,
                    name: d.name.to_string(),
                    owner: d.owner,
                    encoding: d.encoding,
                    locale_provider: d.locale_provider,
                    is_template: d.is_template,
                    allow_conn: d.allow_conn,
                    conn_limit: d.conn_limit,
                    collate: d.collate.to_string(),
                    ctype: d.ctype.to_string(),
                    locale: d.locale.map(|s| s.to_string()),
                    acl,
                    config: Vec::new(),
                },
            );
        }
        for d in data.databases.values() {
            idx.databases.insert(d.oid, d.clone());
        }
        for d in idx.databases.values() {
            idx.database_by_name.insert(d.name.clone(), d.oid);
        }

        // 4. pg_class names
        for r in builtin::SYSTEM_RELS.iter() {
            let c = if r.kind == b'i' { ClassRef::SystemIndex(r.oid) } else { ClassRef::Relation(r.oid) };
            idx.class_by_name.insert((r.namespace, r.name.to_string()), c);
        }
        for r in data.relations.values() {
            insert_class(&mut idx, r.namespace, &r.name, ClassRef::Relation(r.oid))?;
        }
        for i in data.indexes.values() {
            insert_class(&mut idx, i.namespace, &i.name, ClassRef::Index(i.oid))?;
        }
        for s in data.sequences.values() {
            insert_class(&mut idx, s.namespace, &s.name, ClassRef::Sequence(s.oid))?;
        }

        // 5. user types
        for t in data.types.values() {
            insert_type(&mut idx, t.namespace, &t.name, t.oid, UserTypeRef::Type(t.oid))?;
            if t.array_type != 0 {
                let an = t.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&t.name));
                insert_type(&mut idx, t.namespace, &an, t.array_type, UserTypeRef::TypeArray(t.oid))?;
            }
        }
        for r in data.relations.values().filter(|r| r.kind.has_rowtype()) {
            if r.rowtype != 0 {
                insert_type(&mut idx, r.namespace, &r.name, r.rowtype, UserTypeRef::RowType(r.oid))?;
            }
            if r.array_type != 0 {
                let an = r.array_type_name.clone().unwrap_or_else(|| naming::array_type_name(&r.name));
                insert_type(&mut idx, r.namespace, &an, r.array_type, UserTypeRef::RowTypeArray(r.oid))?;
            }
        }

        // 6. per-table lists
        for i in data.indexes.values() {
            idx.indexes_by_table.entry(i.table).or_default().push(i.oid);
        }
        for c in data.constraints.values() {
            if c.table != 0 {
                idx.constraints_by_table.entry(c.table).or_default().push(c.oid);
            }
            if c.domain != 0 {
                idx.constraints_by_domain.entry(c.domain).or_default().push(c.oid);
            }
            if let Some(fk) = &c.fk {
                idx.fks_referencing.entry(fk.ref_table).or_default().push(c.oid);
            }
        }
        for s in data.sequences.values() {
            if let Some(owner) = s.owned_by {
                idx.sequences_by_owner.entry(owner).or_default().push(s.oid);
            }
        }
        sort_lists(&mut idx.indexes_by_table);
        sort_lists(&mut idx.constraints_by_table);
        sort_lists(&mut idx.constraints_by_domain);
        sort_lists(&mut idx.fks_referencing);
        sort_lists(&mut idx.sequences_by_owner);

        // 7. functions
        for f in data.functions.values() {
            idx.functions_by_name.entry((f.namespace, f.name.clone())).or_default().push(f.oid);
        }
        sort_lists(&mut idx.functions_by_name);

        // 8. reverse maps
        for t in data.types.values() {
            if let TypeDef::Enum { labels } = &t.def {
                for l in labels {
                    idx.enum_label_owner.insert(l.oid, t.oid);
                }
            }
        }
        for r in data.relations.values() {
            for c in &r.columns {
                if let Some(d) = &c.default {
                    idx.attrdef_owner.insert(d.oid, (r.oid, c.attnum));
                }
            }
        }

        // 9. validation
        for r in data.relations.values() {
            if !idx.namespaces.contains_key(&r.namespace) {
                return Err(corrupt("relation", r.oid, "namespace", r.namespace));
            }
        }
        for i in data.indexes.values() {
            if !idx.namespaces.contains_key(&i.namespace) {
                return Err(corrupt("index", i.oid, "namespace", i.namespace));
            }
            if !data.relations.contains_key(&i.table) {
                return Err(corrupt("index", i.oid, "table", i.table));
            }
        }
        for s in data.sequences.values() {
            if !idx.namespaces.contains_key(&s.namespace) {
                return Err(corrupt("sequence", s.oid, "namespace", s.namespace));
            }
        }
        for t in data.types.values() {
            if !idx.namespaces.contains_key(&t.namespace) {
                return Err(corrupt("type", t.oid, "namespace", t.namespace));
            }
        }
        for f in data.functions.values() {
            if !idx.namespaces.contains_key(&f.namespace) {
                return Err(corrupt("function", f.oid, "namespace", f.namespace));
            }
        }
        for c in data.constraints.values() {
            if !idx.namespaces.contains_key(&c.namespace) {
                return Err(corrupt("constraint", c.oid, "namespace", c.namespace));
            }
            if c.table != 0 && !data.relations.contains_key(&c.table) {
                return Err(corrupt("constraint", c.oid, "table", c.table));
            }
            if c.domain != 0 && !data.types.contains_key(&c.domain) {
                return Err(corrupt("constraint", c.oid, "type", c.domain));
            }
        }

        validate_type_graph(&data, &idx)?;
        Ok(CatalogSnapshot::from_parts(version, data, idx))
    }

    /// A snapshot with only builtin objects (no persisted rows, version ZERO): useful before bootstrap
    /// and in unit tests.
    pub fn builtin_only() -> Result<CatalogSnapshot> {
        CatalogSnapshot::build(CatalogVersion::ZERO, CatalogData::default())
    }
}

fn malformed(text: &str) -> PgError {
    PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("malformed aclitem array literal: \"{text}\""))
}

/// Split the inside of `{...}` into raw elements (array-level quoting removed from fully quoted
/// elements; inner quotes of unquoted elements are kept for the item parser).
fn split_array_elements(inner: &str, whole: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = inner.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        // A fully quoted element: "..." followed by ',' or the end.
        if chars[i] == '"' {
            let mut j = i + 1;
            let mut s = String::new();
            let mut closed = false;
            while j < n {
                match chars[j] {
                    '\\' if j + 1 < n => {
                        s.push(chars[j + 1]);
                        j += 2;
                    }
                    '"' => {
                        closed = true;
                        j += 1;
                        break;
                    }
                    c => {
                        s.push(c);
                        j += 1;
                    }
                }
            }
            if closed {
                let mut k = j;
                while k < n && chars[k].is_whitespace() {
                    k += 1;
                }
                if k >= n || chars[k] == ',' {
                    out.push(s);
                    if k < n {
                        if k + 1 >= n {
                            return Err(malformed(whole));
                        }
                        i = k + 1;
                    } else {
                        i = k;
                    }
                    continue;
                }
            }
        }
        // An unquoted element: read to the next top-level comma.
        let mut s = String::new();
        let mut in_q = false;
        while i < n {
            let c = chars[i];
            if c == '"' {
                in_q = !in_q;
                s.push(c);
            } else if c == '\\' && i + 1 < n {
                s.push(chars[i + 1]);
                i += 1;
            } else if c == ',' && !in_q {
                break;
            } else {
                s.push(c);
            }
            i += 1;
        }
        if in_q {
            return Err(malformed(whole));
        }
        out.push(s.trim().to_string());
        if i < n {
            i += 1; // skip the comma
            if i >= n {
                return Err(malformed(whole));
            }
        }
    }
    Ok(out)
}

/// Read a role name, optionally double-quoted with `""` escapes. Unquoted names run up to `stop`
/// (or the end when `None`). Returns the name and the remaining text.
fn read_role_name(s: &str, stop: Option<char>) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut it = rest.char_indices().peekable();
        while let Some((pos, c)) = it.next() {
            if c == '"' {
                if let Some(&(_, '"')) = it.peek() {
                    name.push('"');
                    it.next();
                } else {
                    return Some((name, &rest[pos + 1..]));
                }
            } else {
                name.push(c);
            }
        }
        None
    } else if let Some(ch) = stop {
        let end = s.find(ch).unwrap_or(s.len());
        Some((s[..end].trim().to_string(), &s[end..]))
    } else {
        Some((s.trim().to_string(), ""))
    }
}

fn parse_acl_item(item: &str, whole: &str, role_oid: &dyn Fn(&str) -> Option<Oid>) -> Result<AclItem> {
    let lookup = |name: &str| -> Result<Oid> {
        role_oid(name)
            .ok_or_else(|| PgError::new(SqlState::INTERNAL_ERROR, format!("unknown role \"{name}\" in builtin acl")))
    };
    let (grantee_name, rest) = read_role_name(item, Some('=')).ok_or_else(|| malformed(whole))?;
    let rest = rest.trim_start().strip_prefix('=').ok_or_else(|| malformed(whole))?;
    let slash = rest.find('/').ok_or_else(|| malformed(whole))?;
    let (privs, grantor_part) = (&rest[..slash], &rest[slash + 1..]);
    let mut privileges = 0u32;
    let mut grant_options = 0u32;
    let mut last_bit = 0u32;
    for ch in privs.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '*' {
            if last_bit == 0 {
                return Err(malformed(whole));
            }
            grant_options |= last_bit;
            continue;
        }
        match ACL_ALL_RIGHTS_STR.chars().position(|c| c == ch) {
            Some(p) => {
                last_bit = 1u32 << p;
                privileges |= last_bit;
            }
            None => {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid mode character: must be one of \"{ACL_ALL_RIGHTS_STR}\""),
                ))
            }
        }
    }
    let (grantor_name, _) = read_role_name(grantor_part, None).ok_or_else(|| malformed(whole))?;
    if grantor_name.is_empty() {
        return Err(malformed(whole));
    }
    let grantee = if grantee_name.is_empty() { 0 } else { lookup(&grantee_name)? };
    let grantor = lookup(&grantor_name)?;
    Ok(AclItem { grantee, grantor, privileges, grant_options })
}

/// Parse PG aclitem[] text (see module docs).
// CONTRACT
pub fn parse_acl_text(text: Option<&str>, role_oid: &dyn Fn(&str) -> Option<Oid>) -> Result<Option<Acl>> {
    let t = match text {
        Some(t) => t.trim(),
        None => return Ok(None),
    };
    if t.is_empty() {
        return Ok(None);
    }
    let inner = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')).ok_or_else(|| malformed(t))?;
    let mut acl: Acl = Vec::new();
    for el in split_array_elements(inner, t)? {
        acl.push(parse_acl_item(&el, t, role_oid)?);
    }
    Ok(Some(acl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{acl_mode, Persistence, Sequence};

    fn roles(name: &str) -> Option<Oid> {
        match name {
            "postgres" => Some(10),
            "g" => Some(20),
            "quoted name" => Some(21),
            _ => None,
        }
    }

    fn seq(oid: Oid, name: &str, ns: Oid) -> Sequence {
        Sequence {
            oid,
            name: name.to_string(),
            namespace: ns,
            owner: 10,
            persistence: Persistence::Permanent,
            type_oid: 20,
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: None,
            identity: false,
            acl: None,
        }
    }

    fn public_ns() -> Namespace {
        Namespace { oid: 2200, name: "public".into(), owner: 6171, acl: None }
    }

    #[test]
    fn acl_basic() {
        let a = parse_acl_text(Some("{postgres=UC/postgres,=U/postgres}"), &roles).unwrap().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].grantee, 10);
        assert_eq!(a[0].privileges, acl_mode::USAGE | acl_mode::CREATE);
        assert_eq!(a[1].grantee, 0);
        assert_eq!(a[1].grantor, 10);
    }

    #[test]
    fn acl_grant_option_quoted_and_empty() {
        let a = parse_acl_text(Some("{g=r*w/postgres,\"quoted name\"=U/g}"), &roles).unwrap().unwrap();
        assert_eq!(a[0].privileges, acl_mode::SELECT | acl_mode::UPDATE);
        assert_eq!(a[0].grant_options, acl_mode::SELECT);
        assert_eq!(a[1].grantee, 21);
        assert_eq!(a[1].grantor, 20);
        let b = parse_acl_text(Some("{\"\\\"quoted name\\\"=U/g\"}"), &roles).unwrap().unwrap();
        assert_eq!(b[0].grantee, 21);
        assert_eq!(parse_acl_text(Some("{}"), &roles).unwrap(), Some(Vec::new()));
        assert_eq!(parse_acl_text(None, &roles).unwrap(), None);
        assert_eq!(parse_acl_text(Some(""), &roles).unwrap(), None);
    }

    #[test]
    fn acl_unknown_role() {
        let e = parse_acl_text(Some("{nobody=r/postgres}"), &roles).unwrap_err();
        assert_eq!(e.message, "unknown role \"nobody\" in builtin acl");
    }

    #[test]
    fn builtin_only_lookups() {
        let s = CatalogSnapshot::builtin_only().unwrap();
        assert_eq!(s.namespace_by_name("pg_catalog").map(|n| n.oid), Some(11));
        assert_eq!(s.namespace_by_name("information_schema").map(|n| n.oid), Some(13293));
        assert_eq!(s.namespace_by_name("pg_toast").map(|n| n.oid), Some(99));
        assert!(s.namespace_by_name("public").is_none());
        assert_eq!(s.class_in_namespace(11, "pg_class"), Some(ClassRef::Relation(1259)));
        assert!(matches!(s.class_in_namespace(11, "pg_class_oid_index"), Some(ClassRef::SystemIndex(_))));
        assert!(s.role_by_name("postgres").is_none());
        assert!(s.role_by_name("pg_monitor").is_some());
        assert!(s.database_by_name("template1").is_some());
        assert!(s.database_by_name("postgres").is_none());
    }

    #[test]
    fn duplicate_relation_is_corruption() {
        let mut data = CatalogData::default();
        data.namespaces.insert(2200, public_ns());
        data.sequences.insert(16384, seq(16384, "s", 2200));
        data.sequences.insert(16385, seq(16385, "s", 2200));
        let e = CatalogSnapshot::build(CatalogVersion::ZERO, data).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(e.message, "duplicate catalog entry for relation \"2200.s\"");
    }

    #[test]
    fn missing_namespace_is_corruption() {
        let mut data = CatalogData::default();
        data.sequences.insert(16384, seq(16384, "s", 2200));
        let e = CatalogSnapshot::build(CatalogVersion::ZERO, data).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        assert_eq!(e.message, "catalog is corrupt: sequence 16384 references missing namespace 2200");
    }

    #[test]
    fn owned_sequences_indexed_and_sorted() {
        let mut data = CatalogData::default();
        data.namespaces.insert(2200, public_ns());
        let mut s2 = seq(16390, "b", 2200);
        s2.owned_by = Some((16384, 1));
        let mut s1 = seq(16385, "a", 2200);
        s1.owned_by = Some((16384, 1));
        data.sequences.insert(16390, s2);
        data.sequences.insert(16385, s1);
        let s = CatalogSnapshot::build(CatalogVersion::ZERO, data).unwrap();
        assert_eq!(s.indexes().sequences_by_owner[&(16384, 1)], vec![16385, 16390]);
        assert_eq!(s.class_in_namespace(2200, "a"), Some(ClassRef::Sequence(16385)));
        assert!(s.namespace_by_name("public").is_some());
    }

    fn domain(oid: Oid, base: Oid) -> crate::model::TypeObj {
        crate::model::TypeObj {
            oid, name: format!("d{oid}"), namespace: 11, owner: 10,
            array_type: oid + 1, array_type_name: None, acl: None,
            def: TypeDef::Domain { base: pgarbor_types::TypeId::new(base, -1), not_null: false,
                default: None, collation: 0, ndims: 0 },
        }
    }

    fn check_graph_both_paths(types: Vec<crate::model::TypeObj>, valid: bool) {
        let mut data = CatalogData::default();
        let mut kv = pgarbor_common::MemKv::default();
        for ty in types {
            crate::persist::put_object(&mut kv, &crate::model::CatalogObject::Type(ty.clone())).expect("encoded row");
            data.types.insert(ty.oid, ty);
        }
        for result in [CatalogSnapshot::build(CatalogVersion::ZERO, data), CatalogSnapshot::load(&kv)] {
            if valid {
                let snap = result.expect("valid domain chain");
                let env = crate::CatalogEnv::with_default_path(snap);
                env.user_type_info(16384).expect("type metadata").expect("domain type");
            } else {
                assert_eq!(result.expect_err("invalid domain graph").sqlstate.code(), "XX001");
            }
        }
    }

    #[test]
    fn domain_graph_rejects_cycles_and_missing_bases() {
        check_graph_both_paths(vec![domain(16384, 16384)], false);
        check_graph_both_paths(vec![domain(16384, 16386), domain(16386, 16384)], false);
        check_graph_both_paths(vec![domain(16384, 999999)], false);
        // Resolving a domain's array metadata also resolves its element domain.
        check_graph_both_paths(vec![domain(16384, 16385)], false);
        check_graph_both_paths(vec![domain(16384, 16387), domain(16386, 16385)], false);
    }

    #[test]
    fn domain_graph_accepts_valid_chains_and_bounds_depth() {
        check_graph_both_paths(vec![domain(16384, 23)], true);
        check_graph_both_paths(vec![domain(16384, 16386), domain(16386, 23)], true);
        let chain = |len: usize| (0..len).map(|i| {
            domain(16384 + 2 * i as u32, if i + 1 == len { 23 } else { 16386 + 2 * i as u32 })
        }).collect();
        // The implicit array introduces one additional metadata-resolution edge.
        check_graph_both_paths(chain(MAX_TYPE_RESOLUTION_DEPTH - 1), true);
        check_graph_both_paths(chain(MAX_TYPE_RESOLUTION_DEPTH + 1), false);
    }

}
