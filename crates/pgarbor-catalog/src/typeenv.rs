//! `pgarbor_types::TypeEnv` for [`CatalogEnv`]: builtin types from the compiled-in registry plus
//! user types (enums, domains, table/view/composite rowtypes, and their array types) from the
//! snapshot; reg* name lookups. Lease CAT-05.
//!
//! The frozen `TypeEnv` trait returns `Option`, so the trait methods (implemented below, keep them)
//! delegate to the `Result`-returning inherent methods this lease implements and map `Err` to
//! `None` / `Err` as the trait allows. Builtin OIDs never reach the leased code
//! (`registry::lookup_type` answers them first).
//!
//! ## `user_type_info(oid)` — synthesize a `TypeInfo` exactly like PG's pg_type row
//! (oracle-verified shapes; see `contracts/reference/pg18/pg_type.csv` for builtin analogues such as
//! `pg_class` (83) / `_pg_class` (273)):
//! - **enum** (`TypeObj` Enum): name, namespace, len 4, byval true, typtype b'e', category b'E',
//!   preferred false, delim b',', relid 0, elem 0, array = obj.array_type, input `enum_in`, output
//!   `enum_out`, receive `enum_recv`, send `enum_send`, align b'i', storage b'p', basetype 0,
//!   typmod -1, ndims 0, collation 0.
//! - **domain**: typtype b'd'; category, len, byval, align, storage, delim, output and send copied
//!   from the BASE type's info (recursively resolved through `self.type_info(base)`); input
//!   `domain_in`, receive `domain_recv`; basetype = base.oid, typmod = base.typmod, ndims, collation =
//!   the domain's collation; array = obj.array_type; preferred false.
//!   (Oracle: `information_schema.sql_identifier` = len 64, byval f, category S, output `nameout`,
//!   send `namesend`, align c, storage p, basetype 19, collation 950.)
//! - **rowtype** of a relation: name = relname, namespace = rel namespace, len -1, byval false,
//!   typtype b'c', category b'C', relid = rel oid, elem 0, array = rel.array_type, input `record_in`,
//!   output `record_out`, receive `record_recv`, send `record_send`, align b'd', storage b'x',
//!   collation 0.
//! - **array** of any of the above: name = stored/canonical array name, len -1, byval false,
//!   typtype b'b', category b'A', delim b',', elem = element OID, array 0, input `array_in`,
//!   output `array_out`, receive `array_recv`, send `array_send`, align = b'd' if the element's align
//!   is b'd' else b'i', storage b'x', collation = element's collation.
//!   The `&'static str` routine names are string literals (`"enum_in"`, ...).
//! - Unknown OID → `Ok(None)`.
//!
//! ## `type_oid_by_name(schema, typname)` → `self.lookup_type(schema, typname)` (CAT-04).
//!
//! ## `enum_labels_of(oid)` → labels sorted by `sort_order` as `pgarbor_types::context::EnumLabel`;
//! `Ok(None)` if not an enum. `composite_fields_of(type_oid)`: the rowtype's relation columns in
//! attnum order as `CompositeField{name, type_id, dropped}` (builtin catalog rowtypes too: e.g.
//! type 83 `pg_class` → the 34 pg_class columns); `Ok(None)` if not a composite type.
//!
//! ## reg* (`reg_lookup_impl`, `reg_name_impl`), PG `utils/adt/regproc.c`
//! Input names are raw text as typed in the literal: `'public.t'::regclass`, `'"Mixed"'::regclass`.
//! Split with PG `stringToQualifiedNameList` rules (dots outside quotes separate, unquoted parts are
//! down-cased, quotes stripped; > 2 parts for objects in this database → 42601 / `improper
//! qualified name (too many dotted names): a.b.c.d` (42601)). Then:
//! - `Class` → `lookup_class` (any pg_class kind); `Type` → `pgarbor_types::names::parse_type_string(self, s)`
//!   (handles `integer`, `character varying(10)[]`, `public.mood`); `Proc` → unique function by name
//!   (more than one → 42725 `more than one function named "<name>"`); `Procedure` → `name(type, ...)`
//!   parsed with type names through `parse_type_string`, exact IN-arg match; `Oper` → unique builtin
//!   operator by name (42725 `more than one operator named <name>`); `Operator` → `op(left,right)`
//!   with `NONE` for a missing side; `Namespace` → schema; `Role` → role; `Collation` → collation;
//!   `Config`, `Dictionary` → `Err(PgError::unimplemented("CAT-05", "text search configuration lookup"))`
//!   (text search is not supported in v1).
//! - `Ok(None)` when not found (the types crate raises the PG error).
//! Output (`reg_name_impl`): `Class` → `quote_identifier(relname)`, schema-qualified when not
//! visible (`qualified_class_name`); `Type` → `pgarbor_types::names::format_type(self, oid, -1)`;
//! `Proc` → proname, qualified when not visible; `Procedure` → `name(argtype,argtype)` with
//! `format_type` names joined by `,` (no space, PG `format_procedure`); `Oper` → oprname;
//! `Operator` → `+(integer,integer)`; `Namespace`/`Role`/`Collation` → quoted name (collation
//! `"default"` is printed quoted as PG does). `Ok(None)` when the OID does not exist.
use crate::env::CatalogEnv;
use crate::model::{Column, Relation, TypeDef, TypeObj};
use crate::snapshot::UserTypeRef;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{builtin_operator, builtin_proc, operators_named};
use pgarbor_types::context::{CompositeField, EnumLabel, RegKind, TypeEnv};
use pgarbor_types::names::{display_name, parse_type_string};
use pgarbor_types::registry::{self, TypeInfo};
use pgarbor_types::text::format::quote_identifier;
use std::borrow::Cow;
use std::sync::Arc;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn trim_end_space(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_ascii() && is_space(c as u8))
}

/// PG `SplitIdentifierString(s, '.')` + `stringToQualifiedNameList`: `None` = invalid syntax.
fn split_qualified(s: &str) -> Option<Vec<String>> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out: Vec<String> = Vec::new();
    while i < n && is_space(b[i]) {
        i += 1;
    }
    loop {
        if i >= n {
            return None;
        }
        let ident: String;
        if b[i] == b'"' {
            i += 1;
            let mut cur: Vec<u8> = Vec::new();
            loop {
                if i >= n {
                    return None;
                }
                if b[i] == b'"' {
                    if i + 1 < n && b[i + 1] == b'"' {
                        cur.push(b'"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                cur.push(b[i]);
                i += 1;
            }
            ident = String::from_utf8(cur).ok()?;
        } else {
            let start = i;
            while i < n && b[i] != b'.' && !is_space(b[i]) {
                i += 1;
            }
            ident = crate::naming::truncate_identifier(&s[start..i].to_ascii_lowercase());
        }
        if ident.is_empty() {
            return None;
        }
        out.push(ident);
        while i < n && is_space(b[i]) {
            i += 1;
        }
        if i >= n {
            return Some(out);
        }
        if b[i] != b'.' {
            return None;
        }
        i += 1;
        while i < n && is_space(b[i]) {
            i += 1;
        }
    }
}

fn invalid_name_syntax() -> PgError {
    PgError::new(SqlState::INVALID_NAME, "invalid name syntax")
}

fn parse_names(s: &str) -> Result<Vec<String>> {
    split_qualified(s).ok_or_else(invalid_name_syntax)
}

fn too_many_dotted(names: &[String]) -> PgError {
    PgError::new(
        SqlState::SYNTAX_ERROR,
        format!("improper qualified name (too many dotted names): {}", names.join(".")),
    )
}

/// (schema, name) from a name list of length 1 or 2; longer lists are 42601.
fn schema_and_name(names: &[String]) -> Result<(Option<&str>, &str)> {
    match names.len() {
        1 => Ok((None, names[0].as_str())),
        2 => Ok((Some(names[0].as_str()), names[1].as_str())),
        _ => Err(too_many_dotted(names)),
    }
}

fn bad_text_repr(msg: &str) -> PgError {
    PgError::new(SqlState(*b"22P02"), msg)
}

/// PG `parseNameAndArgTypes`: `name(type, type, ...)`; `None` entries are `NONE` (when allowed).
fn split_name_and_args(s: &str, allow_none: bool) -> Result<(Vec<String>, Vec<Option<String>>)> {
    let b = s.as_bytes();
    let mut in_quote = false;
    let mut lp = None;
    for (i, &c) in b.iter().enumerate() {
        if c == b'"' {
            in_quote = !in_quote;
        } else if c == b'(' && !in_quote {
            lp = Some(i);
            break;
        }
    }
    let lp = lp.ok_or_else(|| bad_text_repr("expected a left parenthesis"))?;
    let names = parse_names(&s[..lp])?;
    let rest = trim_end_space(&s[lp + 1..]);
    let inner = match rest.strip_suffix(')') {
        Some(x) => x,
        None => return Err(bad_text_repr("expected a right parenthesis")),
    };
    let ib = inner.as_bytes();
    let mut args: Vec<Option<String>> = Vec::new();
    let mut i = 0;
    let mut had_comma = false;
    loop {
        while i < ib.len() && is_space(ib[i]) {
            i += 1;
        }
        if i >= ib.len() {
            if had_comma {
                return Err(bad_text_repr("expected a type name"));
            }
            break;
        }
        let start = i;
        let mut quote = false;
        let mut depth: i32 = 0;
        while i < ib.len() {
            let c = ib[i];
            if c == b'"' {
                quote = !quote;
            } else if c == b',' && !quote && depth == 0 {
                break;
            } else if !quote {
                match c {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    _ => {}
                }
            }
            i += 1;
        }
        if quote || depth != 0 {
            return Err(bad_text_repr("improper type name"));
        }
        let tn = trim_end_space(&inner[start..i]);
        if i < ib.len() && ib[i] == b',' {
            had_comma = true;
            i += 1;
        } else {
            had_comma = false;
        }
        if allow_none && tn.eq_ignore_ascii_case("none") {
            args.push(None);
        } else {
            args.push(Some(tn.to_string()));
        }
    }
    Ok((names, args))
}

fn rowtype_info(rel: &Relation) -> TypeInfo {
    TypeInfo {
        oid: rel.rowtype,
        name: Cow::Owned(rel.name.clone()),
        namespace: rel.namespace,
        len: -1,
        byval: false,
        typtype: b'c',
        category: b'C',
        preferred: false,
        delim: b',',
        relid: rel.oid,
        elem: 0,
        array: rel.array_type,
        input: "record_in",
        output: "record_out",
        receive: "record_recv",
        send: "record_send",
        align: b'd',
        storage: b'x',
        basetype: 0,
        typmod: -1,
        ndims: 0,
        collation: 0,
    }
}

fn array_info(oid: Oid, name: String, elem: &TypeInfo) -> TypeInfo {
    TypeInfo {
        oid,
        name: Cow::Owned(name),
        namespace: elem.namespace,
        len: -1,
        byval: false,
        typtype: b'b',
        category: b'A',
        preferred: false,
        delim: b',',
        relid: 0,
        elem: elem.oid,
        array: 0,
        input: "array_in",
        output: "array_out",
        receive: "array_recv",
        send: "array_send",
        align: if elem.align == b'd' { b'd' } else { b'i' },
        storage: b'x',
        basetype: 0,
        typmod: -1,
        ndims: 0,
        collation: elem.collation,
    }
}

fn composite_fields_from(rel: &Relation) -> Arc<[CompositeField]> {
    let mut cols: Vec<&Column> = rel.columns.iter().filter(|c| c.attnum > 0).collect();
    cols.sort_by_key(|c| c.attnum);
    let v: Vec<CompositeField> = cols
        .into_iter()
        .map(|c| CompositeField { name: c.name.clone(), type_id: c.type_id, dropped: c.dropped })
        .collect();
    v.into()
}

impl CatalogEnv {
    fn type_obj_info(&self, t: &TypeObj) -> Result<TypeInfo> {
        match &t.def {
            TypeDef::Enum { .. } => Ok(TypeInfo {
                oid: t.oid,
                name: Cow::Owned(t.name.clone()),
                namespace: t.namespace,
                len: 4,
                byval: true,
                typtype: b'e',
                category: b'E',
                preferred: false,
                delim: b',',
                relid: 0,
                elem: 0,
                array: t.array_type,
                input: "enum_in",
                output: "enum_out",
                receive: "enum_recv",
                send: "enum_send",
                align: b'i',
                storage: b'p',
                basetype: 0,
                typmod: -1,
                ndims: 0,
                collation: 0,
            }),
            TypeDef::Domain { base, collation, ndims, .. } => {
                let b = self
                    .type_info(base.oid)
                    .ok_or_else(|| crate::errors::cache_lookup_failed("type", base.oid))?;
                Ok(TypeInfo {
                    oid: t.oid,
                    name: Cow::Owned(t.name.clone()),
                    namespace: t.namespace,
                    len: b.len,
                    byval: b.byval,
                    typtype: b'd',
                    category: b.category,
                    preferred: false,
                    delim: b.delim,
                    relid: 0,
                    elem: 0,
                    array: t.array_type,
                    input: "domain_in",
                    output: b.output,
                    receive: "domain_recv",
                    send: b.send,
                    align: b.align,
                    storage: b.storage,
                    basetype: b.oid,
                    typmod: base.typmod,
                    ndims: *ndims,
                    collation: *collation,
                })
            }
        }
    }

    fn namespace_name_of(&self, oid: Oid) -> Option<String> {
        if let Some(n) = self.snapshot().namespace(oid) {
            return Some(n.name.clone());
        }
        crate::builtin::builtin_namespace(oid).map(|n| n.name.to_string())
    }

    fn role_name_of(&self, oid: Oid) -> Option<String> {
        if let Some(n) = self.snapshot().role_name(oid) {
            return Some(n.to_string());
        }
        crate::builtin::builtin_role(oid).map(|r| r.name.to_string())
    }

    /// Routine identity: (name, namespace, IN argument types).
    fn routine_identity(&self, oid: Oid) -> Option<(String, Oid, Vec<Oid>)> {
        if let Some(p) = builtin_proc(oid) {
            return Some((p.name.to_string(), p.namespace, p.arg_types.to_vec()));
        }
        self.snapshot().function(oid).map(|f| (f.name.clone(), f.namespace, f.in_arg_types()))
    }

    fn qualified_if_invisible(&self, namespace: Oid, name: &str, visible: bool) -> Result<String> {
        let q = quote_identifier(name)?;
        if visible {
            return Ok(q);
        }
        match self.namespace_name_of(namespace) {
            Some(n) => Ok(format!("{}.{}", quote_identifier(&n)?, q)),
            None => Ok(q),
        }
    }

    /// `TypeInfo` of a USER type OID (module docs). Builtins are not passed here.
    // CONTRACT
    pub fn user_type_info(&self, oid: Oid) -> Result<Option<TypeInfo>> {
        let snap = self.snapshot();
        let Some(r) = snap.user_type(oid) else {
            return Ok(None);
        };
        match r {
            UserTypeRef::Type(o) => match snap.type_obj(o) {
                Some(t) => Ok(Some(self.type_obj_info(t)?)),
                None => Ok(None),
            },
            UserTypeRef::TypeArray(elem) => {
                let Some(t) = snap.type_obj(elem) else {
                    return Ok(None);
                };
                let ei = self.type_obj_info(t)?;
                let name = t.array_type_name.clone().unwrap_or_else(|| crate::naming::array_type_name(&t.name));
                Ok(Some(array_info(oid, name, &ei)))
            }
            UserTypeRef::RowType(relid) => Ok(snap.relation(relid).map(rowtype_info)),
            UserTypeRef::RowTypeArray(relid) => {
                let Some(rel) = snap.relation(relid) else {
                    return Ok(None);
                };
                let ei = rowtype_info(rel);
                let name = rel.array_type_name.clone().unwrap_or_else(|| crate::naming::array_type_name(&rel.name));
                Ok(Some(array_info(oid, name, &ei)))
            }
        }
    }

    /// Enum labels in sort order, or None if `oid` is not a (user) enum.
    // CONTRACT
    pub fn enum_labels_of(&self, oid: Oid) -> Result<Option<Arc<[EnumLabel]>>> {
        let Some(t) = self.snapshot().type_obj(oid) else {
            return Ok(None);
        };
        match &t.def {
            TypeDef::Enum { labels } => {
                let mut v: Vec<EnumLabel> = labels
                    .iter()
                    .map(|l| EnumLabel { oid: l.oid, label: l.label.clone(), sort_order: l.sort_order })
                    .collect();
                v.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
                Ok(Some(v.into()))
            }
            TypeDef::Domain { .. } => Ok(None),
        }
    }

    /// Fields of a composite type (user or builtin rowtype), or None.
    // CONTRACT
    pub fn composite_fields_of(&self, type_oid: Oid) -> Result<Option<Arc<[CompositeField]>>> {
        if let Some(UserTypeRef::RowType(relid)) = self.snapshot().user_type(type_oid) {
            return Ok(self.snapshot().relation(relid).map(composite_fields_from));
        }
        if let Some(rel) = crate::builtin::builtin_relations().iter().find(|r| r.rowtype == type_oid) {
            return Ok(Some(composite_fields_from(rel)));
        }
        Ok(None)
    }

    /// reg* input lookup (module docs).
    // CONTRACT
    pub fn reg_lookup_impl(&self, kind: RegKind, name: &str) -> Result<Option<Oid>> {
        match kind {
            RegKind::Class => {
                let names = parse_names(name)?;
                let (schema, rel) = match names.len() {
                    1 | 2 => schema_and_name(&names)?,
                    3 => {
                        let db = self.snapshot().database(self.database()).map(|d| d.name.as_str());
                        if db == Some(names[0].as_str()) {
                            (Some(names[1].as_str()), names[2].as_str())
                        } else {
                            return Err(too_many_dotted(&names));
                        }
                    }
                    _ => return Err(too_many_dotted(&names)),
                };
                Ok(self.lookup_class(schema, rel)?.map(|c| c.oid()))
            }
            RegKind::Type => match parse_type_string(self, name) {
                Ok(t) => Ok(Some(t.oid)),
                Err(e) if e.sqlstate == SqlState::UNDEFINED_OBJECT => Ok(None),
                Err(e) => Err(e),
            },
            RegKind::Proc => {
                let names = parse_names(name)?;
                let (schema, f) = schema_and_name(&names)?;
                let cands = match self.function_candidates(schema, f) {
                    Ok(c) => c,
                    Err(e) if e.sqlstate == SqlState(*b"3F000") => return Ok(None),
                    Err(e) => return Err(e),
                };
                match cands.len() {
                    0 => Ok(None),
                    1 => Ok(Some(cands[0].oid)),
                    _ => Err(PgError::new(
                        SqlState::AMBIGUOUS_FUNCTION,
                        format!("more than one function named \"{}\"", names.join(".")),
                    )),
                }
            }
            RegKind::Procedure => {
                let (names, args) = split_name_and_args(name, false)?;
                let (schema, f) = schema_and_name(&names)?;
                let mut argtypes: Vec<Oid> = Vec::with_capacity(args.len());
                for a in args.iter().flatten() {
                    argtypes.push(parse_type_string(self, a)?.oid);
                }
                let cands = match self.function_candidates(schema, f) {
                    Ok(c) => c,
                    Err(e) if e.sqlstate == SqlState(*b"3F000") => return Ok(None),
                    Err(e) => return Err(e),
                };
                Ok(cands.iter().filter(|c| c.arg_types == argtypes).min_by_key(|c| c.path_pos).map(|c| c.oid))
            }
            RegKind::Oper => {
                let names = parse_names(name)?;
                let (schema, op) = schema_and_name(&names)?;
                if schema.is_some_and(|s| s != "pg_catalog") {
                    return Ok(None);
                }
                let found: Vec<Oid> = operators_named(op).map(|o| o.oid).collect();
                match found.len() {
                    0 => Ok(None),
                    1 => Ok(Some(found[0])),
                    _ => Err(PgError::new(
                        SqlState::AMBIGUOUS_FUNCTION,
                        format!("more than one operator named {name}"),
                    )),
                }
            }
            RegKind::Operator => {
                let (names, args) = split_name_and_args(name, true)?;
                let (schema, op) = schema_and_name(&names)?;
                if args.len() != 2 {
                    return Err(PgError::new(SqlState(*b"42P02"), "missing argument")
                        .with_hint("Use NONE to denote the missing argument of a unary operator."));
                }
                let mut ids: [Oid; 2] = [0, 0];
                for (k, a) in args.iter().enumerate() {
                    if let Some(a) = a {
                        ids[k] = parse_type_string(self, a)?.oid;
                    }
                }
                if schema.is_some_and(|s| s != "pg_catalog") {
                    return Ok(None);
                }
                Ok(operators_named(op).find(|o| o.left == ids[0] && o.right == ids[1]).map(|o| o.oid))
            }
            RegKind::Namespace => {
                let names = parse_names(name)?;
                if names.len() != 1 {
                    return Err(invalid_name_syntax());
                }
                Ok(self.lookup_namespace(&names[0]))
            }
            RegKind::Role => {
                let names = parse_names(name)?;
                if names.len() != 1 {
                    return Err(invalid_name_syntax());
                }
                Ok(self.snapshot().role_by_name(&names[0]).map(|r| r.oid))
            }
            RegKind::Collation => {
                let names = parse_names(name)?;
                let (schema, c) = schema_and_name(&names)?;
                match self.collation_oid(schema, c) {
                    Ok(o) => Ok(Some(o)),
                    Err(e) if e.sqlstate == SqlState::UNDEFINED_OBJECT || e.sqlstate == SqlState(*b"3F000") => {
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            RegKind::Config | RegKind::Dictionary => {
                Err(PgError::unimplemented("CAT-05", "text search configuration lookup"))
            }
        }
    }

    /// reg* output name (module docs).
    // CONTRACT
    pub fn reg_name_impl(&self, kind: RegKind, oid: Oid) -> Result<Option<String>> {
        match kind {
            RegKind::Class => {
                if self.snapshot().class(oid).is_none() {
                    return Ok(None);
                }
                Ok(Some(self.qualified_class_name(oid)?))
            }
            RegKind::Type => {
                if self.type_info(oid).is_none() {
                    return Ok(None);
                }
                Ok(Some(display_name(self, oid)))
            }
            RegKind::Proc => {
                let Some((name, ns, _)) = self.routine_identity(oid) else {
                    return Ok(None);
                };
                let vis = self.function_is_visible(oid)?;
                Ok(Some(self.qualified_if_invisible(ns, &name, vis)?))
            }
            RegKind::Procedure => {
                let Some((name, ns, args)) = self.routine_identity(oid) else {
                    return Ok(None);
                };
                let vis = self.function_is_visible(oid)?;
                let mut out = self.qualified_if_invisible(ns, &name, vis)?;
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&display_name(self, *a));
                }
                out.push(')');
                Ok(Some(out))
            }
            RegKind::Oper => Ok(builtin_operator(oid).map(|o| o.name.to_string())),
            RegKind::Operator => {
                let Some(o) = builtin_operator(oid) else {
                    return Ok(None);
                };
                let mut out = String::from(o.name);
                out.push('(');
                if o.left != 0 {
                    out.push_str(&display_name(self, o.left));
                } else {
                    out.push_str("NONE");
                }
                out.push(',');
                if o.right != 0 {
                    out.push_str(&display_name(self, o.right));
                } else {
                    out.push_str("NONE");
                }
                out.push(')');
                Ok(Some(out))
            }
            RegKind::Namespace => match self.namespace_name_of(oid) {
                Some(n) => Ok(Some(quote_identifier(&n)?)),
                None => Ok(None),
            },
            RegKind::Role => match self.role_name_of(oid) {
                Some(n) => Ok(Some(quote_identifier(&n)?)),
                None => Ok(None),
            },
            RegKind::Collation => match crate::builtin::builtin_collation(oid) {
                Some(c) => Ok(Some(quote_identifier(c.name)?)),
                None => Ok(None),
            },
            RegKind::Config | RegKind::Dictionary => Ok(None),
        }
    }
}

impl TypeEnv for CatalogEnv {
    fn type_info(&self, oid: Oid) -> Option<TypeInfo> {
        if let Some(t) = registry::lookup_type(oid) {
            return Some(TypeInfo::from(t));
        }
        self.user_type_info(oid).ok().flatten()
    }
    fn type_by_name(&self, schema: Option<&str>, typname: &str) -> Option<Oid> {
        self.lookup_type(schema, typname).ok().flatten()
    }
    fn enum_labels(&self, oid: Oid) -> Option<Arc<[EnumLabel]>> {
        self.enum_labels_of(oid).ok().flatten()
    }
    fn composite_fields(&self, type_oid: Oid) -> Option<Arc<[CompositeField]>> {
        self.composite_fields_of(type_oid).ok().flatten()
    }
    fn reg_lookup(&self, kind: RegKind, name: &str) -> Result<Option<Oid>> {
        self.reg_lookup_impl(kind, name)
    }
    fn reg_name(&self, kind: RegKind, oid: Oid) -> Option<String> {
        self.reg_name_impl(kind, oid).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn split_names() {
        assert_eq!(split_qualified("public.t").unwrap(), vec!["public", "t"]);
        assert_eq!(split_qualified("\"Mixed\"").unwrap(), vec!["Mixed"]);
        assert_eq!(split_qualified("  A . \"b.c\" ").unwrap(), vec!["a", "b.c"]);
        assert_eq!(split_qualified("\"a\"\"b\"").unwrap(), vec!["a\"b"]);
        assert!(split_qualified("").is_none());
        assert!(split_qualified("a.").is_none());
        assert!(split_qualified(".a").is_none());
        assert!(split_qualified("\"abc").is_none());
        assert!(split_qualified("a b").is_none());
    }

    #[test]
    fn name_and_args() {
        let (n, a) = split_name_and_args("f(integer, numeric(10,2), text[])", false).unwrap();
        assert_eq!(n, vec!["f"]);
        assert_eq!(a.len(), 3);
        assert_eq!(a[1].as_deref(), Some("numeric(10,2)"));
        let (_, a) = split_name_and_args("-(NONE,integer)", true).unwrap();
        assert_eq!(a[0], None);
        assert!(split_name_and_args("f", false).is_err());
        assert!(split_name_and_args("f(integer", false).is_err());
        assert!(split_name_and_args("f(integer,)", false).is_err());
    }

    #[test]
    fn too_many_dotted_names() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let e = env.reg_lookup_impl(RegKind::Class, "a.b.c.d").unwrap_err();
        assert!(e.sqlstate == SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "improper qualified name (too many dotted names): a.b.c.d");
        Ok(())
    }

    #[test]
    fn builtin_composite_fields() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let f = env.composite_fields_of(83)?.expect("pg_class rowtype");
        assert_eq!(f.len(), 34);
        assert_eq!(f[0].name, "oid");
        assert!(env.composite_fields_of(23)?.is_none());
        Ok(())
    }

    #[test]
    fn reg_namespace_role_and_oper() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let pg_catalog = env.reg_lookup_impl(RegKind::Namespace, "pg_catalog")?.expect("pg_catalog");
        assert_eq!(env.reg_name_impl(RegKind::Namespace, pg_catalog)?.as_deref(), Some("pg_catalog"));
        assert_eq!(env.reg_lookup_impl(RegKind::Namespace, "nosuch")?, None);
        assert_eq!(env.reg_lookup_impl(RegKind::Role, "nosuchrole")?, None);
        let op = env.reg_lookup_impl(RegKind::Operator, "+(integer,integer)")?.expect("int4pl op");
        assert_eq!(env.reg_name_impl(RegKind::Operator, op)?.as_deref(), Some("+(integer,integer)"));
        assert_eq!(env.reg_name_impl(RegKind::Oper, op)?.as_deref(), Some("+"));
        Ok(())
    }

    #[test]
    fn reg_class_roundtrip_builtin() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let oid = env.reg_lookup_impl(RegKind::Class, "pg_class")?.expect("pg_class");
        assert_eq!(oid, 1259);
        assert_eq!(env.reg_name_impl(RegKind::Class, oid)?.as_deref(), Some("pg_class"));
        assert_eq!(env.reg_lookup_impl(RegKind::Class, "no_such_rel")?, None);
        assert_eq!(env.reg_name_impl(RegKind::Class, 4_000_000)?, None);
        Ok(())
    }
}
