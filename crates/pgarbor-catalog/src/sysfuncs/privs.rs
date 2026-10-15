//! Privilege inquiry functions. Lease CAT-47. PG sources: `utils/adt/acl.c` (`has_table_privilege_*`,
//! `has_any_column_privilege_*`, `has_column_privilege_*`, `has_database_privilege_*`,
//! `has_function_privilege_*`, `has_language_privilege_*`, `has_schema_privilege_*`,
//! `has_sequence_privilege_*`, `has_tablespace_privilege_*`, `has_type_privilege_*`, `pg_has_role_*`,
//! `convert_*_priv_string`), `utils/misc/rls.c` (`row_security_active`). Checks use `crate::acl`
//! (`RoleGraph`, `privilege_mask`) over `ctx.env.snapshot()`.
//!
//! ## Argument forms
//! Every `has_X_privilege` has forms `(user, object, privilege)` and `(object, privilege)`, where
//! `user` is a role name (`name`) or OID, `object` a name (text) or OID, `privilege` a text list.
//! The two-argument form uses `ctx.env.current_user()`. Resolution of names:
//! `table` via `reg_lookup_impl(Class)` -> 42P01 `relation "x" does not exist`; `schema` 3F000 `schema
//! "x" does not exist`; `database` 3D000 `database "x" does not exist`; `function` text signature via
//! `reg_lookup_impl(Procedure)` -> 42883; `type` via `names::parse_type_string` -> 42704; `sequence`
//! like table (and 42809 `"t" is not a sequence` for non-sequences); `language` 42704 `language "x"
//! does not exist`; role names 42704 `role "x" does not exist`. OID arguments for unknown objects
//! yield NULL (no error).
//!
//! ## Privilege strings (`convert_priv_string` family)
//! Comma-separated, case-insensitive, whitespace-trimmed words, each optionally suffixed ` WITH GRANT
//! OPTION`: tables `SELECT INSERT UPDATE DELETE TRUNCATE REFERENCES TRIGGER MAINTAIN`; sequences
//! `USAGE SELECT UPDATE`; schemas `CREATE USAGE`; databases `CREATE TEMPORARY TEMP CONNECT`;
//! functions `EXECUTE`; languages/types `USAGE`; tablespaces `CREATE`; columns `SELECT INSERT UPDATE
//! REFERENCES`. Unknown word -> 22023 `unrecognized privilege type: "foo"`. Result true iff the user
//! holds ANY of the listed privileges (PG semantic: `pg_*_aclmask(...) != 0`), where WITH GRANT OPTION
//! requires the grant-option bit.
//! - `has_table_privilege` on a column-less check also true when the user has the privilege on ALL
//!   columns? NO: table-level only; `has_any_column_privilege` = table-level OR any column ACL;
//!   `has_column_privilege(table, column|attnum, priv)` = table-level OR that column's ACL (unknown
//!   column name -> 42703; dropped attnum -> NULL).
//! - `pg_has_role(user, role, priv)` with `priv` in `USAGE | MEMBER | SET` (+ `WITH ADMIN OPTION`):
//!   USAGE = `has_privs_of_role`, MEMBER = `is_member_of_role`, SET = `can_set_role`, superuser true
//!   for all; unknown word 22023 `unrecognized privilege type: "x"`.
//! - `row_security_active(table)` -> false (policies unsupported; relrowsecurity without policies
//!   does not hide rows from owners; return `rowsecurity && !owner/bypass` = false in v1).
//! - Foreign data/server/large object/parameter variants: `has_foreign_data_wrapper_privilege`,
//!   `has_server_privilege`, `has_largeobject_privilege` -> NULL for any object (none exist);
//!   `has_parameter_privilege(name, priv)` true for superusers only.
use super::CatalogCallCtx;
use crate::acl::{self, AclObject, RoleGraph};
use crate::builtin;
use crate::env::CatalogEnv;
use crate::errors;
use crate::model::{acl_mode, Acl};
use crate::snapshot::{CatalogSnapshot, ClassRef, UserTypeRef};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::catalog_data::{builtin_proc, builtin_type, BuiltinProc};
use pgarbor_types::context::RegKind;
use pgarbor_types::Datum;
use std::collections::BTreeSet;

/// Procs handled here.
// CONTRACT
pub const NAMES: &[&str] = &[
    "has_table_privilege",
    "has_any_column_privilege",
    "has_column_privilege",
    "has_database_privilege",
    "has_function_privilege",
    "has_language_privilege",
    "has_schema_privilege",
    "has_sequence_privilege",
    "has_tablespace_privilege",
    "has_type_privilege",
    "has_foreign_data_wrapper_privilege",
    "has_server_privilege",
    "has_largeobject_privilege",
    "has_parameter_privilege",
    "pg_has_role",
    "row_security_active",
];

/// Privilege words per object kind (PG `*_priv_map`, without the WITH GRANT OPTION variants, which
/// are accepted for every word). Case-insensitive, matched on the whitespace-trimmed chunk.
const TABLE_WORDS: &[(&str, u32)] = &[
    ("SELECT", acl_mode::SELECT),
    ("INSERT", acl_mode::INSERT),
    ("UPDATE", acl_mode::UPDATE),
    ("DELETE", acl_mode::DELETE),
    ("TRUNCATE", acl_mode::TRUNCATE),
    ("REFERENCES", acl_mode::REFERENCES),
    ("TRIGGER", acl_mode::TRIGGER),
    ("MAINTAIN", acl_mode::MAINTAIN),
];
const SEQUENCE_WORDS: &[(&str, u32)] =
    &[("USAGE", acl_mode::USAGE), ("SELECT", acl_mode::SELECT), ("UPDATE", acl_mode::UPDATE)];
const COLUMN_WORDS: &[(&str, u32)] = &[
    ("SELECT", acl_mode::SELECT),
    ("INSERT", acl_mode::INSERT),
    ("UPDATE", acl_mode::UPDATE),
    ("REFERENCES", acl_mode::REFERENCES),
];
const DATABASE_WORDS: &[(&str, u32)] = &[
    ("CREATE", acl_mode::CREATE),
    ("TEMPORARY", acl_mode::CREATE_TEMP),
    ("TEMP", acl_mode::CREATE_TEMP),
    ("CONNECT", acl_mode::CONNECT),
];
const FUNCTION_WORDS: &[(&str, u32)] = &[("EXECUTE", acl_mode::EXECUTE)];
const USAGE_WORDS: &[(&str, u32)] = &[("USAGE", acl_mode::USAGE)];
const SCHEMA_WORDS: &[(&str, u32)] = &[("CREATE", acl_mode::CREATE), ("USAGE", acl_mode::USAGE)];
const TABLESPACE_WORDS: &[(&str, u32)] = &[("CREATE", acl_mode::CREATE)];
const LARGEOBJECT_WORDS: &[(&str, u32)] = &[("SELECT", acl_mode::SELECT), ("UPDATE", acl_mode::UPDATE)];
const PARAMETER_WORDS: &[(&str, u32)] = &[("SET", acl_mode::SET), ("ALTER SYSTEM", acl_mode::ALTER_SYSTEM)];
/// `pg_has_role` words: USAGE (has_privs_of_role), MEMBER (stored as ACL_CREATE, PG convention),
/// SET (can_set_role). The WITH GRANT/ADMIN OPTION variants mean "is admin".
const ROLE_WORDS: &[(&str, u32)] = &[
    ("USAGE", acl_mode::USAGE),
    ("MEMBER", acl_mode::CREATE),
    ("SET", acl_mode::SET),
];

/// Column names that PG resolves to system attributes (table-level privilege check only).
const SYSTEM_COLUMNS: &[&str] = &["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"];

/// Dispatch on `proc.name` (module docs).
// CONTRACT
pub fn call(ctx: &CatalogCallCtx, proc: &BuiltinProc, args: &[Datum]) -> Result<Datum> {
    // All of these procs are strict in PG: any NULL argument gives NULL.
    if args.iter().any(|a| a.is_null()) {
        return Ok(Datum::Null);
    }
    match proc.name {
        "has_table_privilege" => class_privilege(ctx, args, TABLE_WORDS, false),
        "has_sequence_privilege" => class_privilege(ctx, args, SEQUENCE_WORDS, true),
        "has_any_column_privilege" => has_any_column(ctx, args),
        "has_column_privilege" => has_column(ctx, args),
        "has_database_privilege" => simple_privilege(ctx, args, DATABASE_WORDS, resolve_database),
        "has_function_privilege" => simple_privilege(ctx, args, FUNCTION_WORDS, resolve_function),
        "has_language_privilege" => simple_privilege(ctx, args, USAGE_WORDS, resolve_language),
        "has_schema_privilege" => simple_privilege(ctx, args, SCHEMA_WORDS, resolve_schema),
        "has_tablespace_privilege" => simple_privilege(ctx, args, TABLESPACE_WORDS, resolve_tablespace),
        "has_type_privilege" => simple_privilege(ctx, args, USAGE_WORDS, resolve_type),
        "has_foreign_data_wrapper_privilege" | "has_server_privilege" => null_privilege(args, USAGE_WORDS),
        "has_largeobject_privilege" => null_privilege(args, LARGEOBJECT_WORDS),
        "has_parameter_privilege" => has_parameter(ctx, args),
        "pg_has_role" => pg_has_role(ctx, args),
        "row_security_active" => row_security(ctx, args),
        other => Err(PgError::internal(format!("function {other} is not a privilege function"))),
    }
}

// ------------------------------------------------------------------------------------------------
// Privilege masks and string conversion (PG convert_any_priv_string)
// ------------------------------------------------------------------------------------------------

/// Requested privileges: `plain` bits are checked with ANY semantics, `grant` bits (the base
/// privilege bit of a `... WITH GRANT OPTION` word) must be held WITH GRANT OPTION.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PrivMask {
    plain: u32,
    grant: u32,
}

fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Case-insensitive suffix strip; `None` when `s` does not end with `suffix`.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let at = s.len().checked_sub(suffix.len())?;
    if s.is_char_boundary(at) && s[at..].eq_ignore_ascii_case(suffix) {
        Some(&s[..at])
    } else {
        None
    }
}

/// PG `convert_any_priv_string`: split on commas, trim C whitespace per chunk, match each chunk
/// case-insensitively against `words` (optionally followed by ` WITH GRANT OPTION`, or ` WITH ADMIN
/// OPTION` when `admin_option` is set for role privileges). Any unmatched chunk (including the empty
/// string) is 22023 `unrecognized privilege type: "<chunk>"`.
fn parse_privs(text: &str, words: &[(&str, u32)], admin_option: bool) -> Result<PrivMask> {
    let mut mask = PrivMask::default();
    for raw in text.split(',') {
        let chunk = raw.trim_matches(is_c_space);
        let stripped = strip_suffix_ci(chunk, " WITH GRANT OPTION")
            .or_else(|| if admin_option { strip_suffix_ci(chunk, " WITH ADMIN OPTION") } else { None });
        let (base, grant) = match stripped {
            Some(base) => (base, true),
            None => (chunk, false),
        };
        let bit = words
            .iter()
            .find(|(word, _)| base.eq_ignore_ascii_case(word))
            .map(|(_, bit)| *bit)
            .ok_or_else(|| {
                PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("unrecognized privilege type: \"{chunk}\""),
                )
            })?;
        if grant {
            mask.grant |= bit;
        } else {
            mask.plain |= bit;
        }
    }
    Ok(mask)
}

/// True when `mask` holds for `user` on an object with the given effective ACL and owner (PG
/// `aclmask(...) != 0` with ACLMASK_ANY, grant-option bits counted only when held WITH GRANT OPTION).
fn mask_any(graph: &RoleGraph, user: Oid, owner: Oid, acl: &Acl, mask: PrivMask) -> Result<bool> {
    if mask.plain != 0 && acl::privilege_mask(graph, user, owner, acl, mask.plain, false)? != 0 {
        return Ok(true);
    }
    if mask.grant != 0 && acl::privilege_mask(graph, user, owner, acl, mask.grant, true)? != 0 {
        return Ok(true);
    }
    Ok(false)
}

// ------------------------------------------------------------------------------------------------
// Argument handling
// ------------------------------------------------------------------------------------------------

/// A name (`name`/`text`) or OID argument.
#[derive(Clone, Copy, Debug)]
enum Ref<'a> {
    Name(&'a str),
    Id(Oid),
}

/// `(user, obj..., priv)` or `(obj..., priv)` split of a privilege function's arguments.
struct Parts<'a> {
    user: Option<&'a Datum>,
    objs: &'a [Datum],
    priv_arg: &'a Datum,
}

fn split_args(args: &[Datum], nobj: usize) -> Result<Parts<'_>> {
    let n = args.len();
    if n == nobj + 1 {
        Ok(Parts { user: None, objs: &args[..nobj], priv_arg: &args[nobj] })
    } else if n == nobj + 2 {
        Ok(Parts { user: Some(&args[0]), objs: &args[1..1 + nobj], priv_arg: &args[n - 1] })
    } else {
        Err(PgError::internal(format!("privilege function called with {n} arguments")))
    }
}

fn text_arg(d: &Datum) -> Result<&str> {
    d.as_str().ok_or_else(|| PgError::internal("expected a text argument"))
}

fn obj_ref(d: &Datum) -> Result<Ref<'_>> {
    match d {
        Datum::Text(s) => Ok(Ref::Name(s.as_str())),
        Datum::Oid(o) => Ok(Ref::Id(*o)),
        _ => Err(PgError::internal("object argument has an unexpected type")),
    }
}

/// Role argument: a `name` (42704 `role "x" does not exist`; `public` maps to PUBLIC (0) only in the
/// `has_*_privilege` forms, `public_ok`) or an OID (taken as is).
fn role_arg(env: &CatalogEnv, d: &Datum, public_ok: bool) -> Result<Oid> {
    match d {
        Datum::Text(s) => {
            if public_ok && s.as_str() == "public" {
                Ok(0)
            } else {
                env.role_oid(s)
            }
        }
        Datum::Oid(o) => Ok(*o),
        _ => Err(PgError::internal("role argument has an unexpected type")),
    }
}

fn user_of(env: &CatalogEnv, arg: Option<&Datum>, public_ok: bool) -> Result<Oid> {
    match arg {
        Some(d) => role_arg(env, d, public_ok),
        None => Ok(env.current_user()),
    }
}

/// Evaluate two independent argument conversions in PG's order: the object first when it was given
/// by name (PG resolves names before parsing the privilege string), the privilege string first
/// otherwise.
fn ordered<A, B>(
    obj_first: bool,
    obj: impl FnOnce() -> Result<A>,
    priv_: impl FnOnce() -> Result<B>,
) -> Result<(A, B)> {
    if obj_first {
        let a = obj()?;
        let b = priv_()?;
        Ok((a, b))
    } else {
        let b = priv_()?;
        let a = obj()?;
        Ok((a, b))
    }
}

/// PG `textToQualifiedNameList`: dots outside double quotes separate parts, unquoted parts are
/// down-cased, `""` inside quotes is one quote. More than two parts is 42602.
fn split_qualified(text: &str) -> Result<Vec<String>> {
    fn finish_part(cur: &mut String, quoted: bool) -> Result<String> {
        let raw = std::mem::take(cur);
        let part = if quoted { raw } else { raw.trim_matches(is_c_space).to_string() };
        if part.is_empty() {
            return Err(PgError::new(SqlState::INVALID_NAME, "invalid name syntax"));
        }
        Ok(part)
    }

    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quote = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quote = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quote = true;
            quoted = true;
        } else if c == '.' {
            parts.push(finish_part(&mut cur, quoted)?);
            quoted = false;
        } else {
            cur.push(c.to_ascii_lowercase());
        }
    }
    parts.push(finish_part(&mut cur, quoted)?);
    if parts.len() > 2 {
        return Err(PgError::new(
            SqlState::INVALID_NAME,
            format!("improper qualified name (too many dotted names): {text}"),
        ));
    }
    Ok(parts)
}

/// Relation name (possibly qualified) to pg_class OID; 42P01 when not found.
fn class_by_text(env: &CatalogEnv, text: &str) -> Result<Oid> {
    if let Some(oid) = env.reg_lookup_impl(RegKind::Class, text)? {
        return Ok(oid);
    }
    let parts = split_qualified(text)?;
    match parts.as_slice() {
        [name] => Err(errors::relation_does_not_exist(None, name)),
        [schema, name] => Err(errors::relation_does_not_exist(Some(schema.as_str()), name)),
        _ => Err(PgError::new(SqlState::INVALID_NAME, "invalid name syntax")),
    }
}

/// Object argument as a pg_class OID. Names must exist (42P01); OIDs that are not a pg_class
/// object give `None` (NULL result).
fn class_target(env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Oid>> {
    match r {
        Ref::Name(s) => class_by_text(env, s).map(Some),
        Ref::Id(o) => Ok(env.snapshot().class(o).map(|_| o)),
    }
}

fn class_name(snap: &CatalogSnapshot, oid: Oid) -> String {
    snap.class_identity(oid).map(|(_, name, _)| name.to_string()).unwrap_or_default()
}

// ------------------------------------------------------------------------------------------------
// Owners and ACLs
// ------------------------------------------------------------------------------------------------

/// The owner and stored ACL of one object plus the kind used for its default ACL.
struct Owned {
    owner: Oid,
    acl: Option<Acl>,
    kind: AclObject,
}

impl Owned {
    fn check(&self, graph: &RoleGraph, user: Oid, mask: PrivMask) -> Result<bool> {
        let eff = acl::effective_acl(self.kind, self.owner, self.acl.as_ref())?;
        mask_any(graph, user, self.owner, &eff, mask)
    }
}

/// An object with no stored ACL: default privileges of `kind`, owned by `owner`.
fn builtin_owned(owner: Oid, kind: AclObject) -> Owned {
    Owned { owner, acl: None, kind }
}

fn class_owned(snap: &CatalogSnapshot, oid: Oid) -> Option<Owned> {
    Some(match snap.class(oid)? {
        ClassRef::Relation(o) => {
            let r = snap.relation(o)?;
            Owned { owner: r.owner, acl: r.acl.clone(), kind: AclObject::Table }
        }
        ClassRef::Index(o) => {
            let i = snap.index(o)?;
            builtin_owned(i.owner, AclObject::Table)
        }
        ClassRef::Sequence(o) => {
            let s = snap.sequence(o)?;
            Owned { owner: s.owner, acl: s.acl.clone(), kind: AclObject::Sequence }
        }
        ClassRef::SystemIndex(_) => builtin_owned(builtin::BOOTSTRAP_SUPERUSER, AclObject::Table),
    })
}

fn owned_namespace(snap: &CatalogSnapshot, oid: Oid) -> Option<Owned> {
    let n = snap.namespace(oid)?;
    Some(Owned { owner: n.owner, acl: n.acl.clone(), kind: AclObject::Schema })
}

fn owned_function(snap: &CatalogSnapshot, oid: Oid) -> Option<Owned> {
    if let Some(f) = snap.function(oid) {
        return Some(Owned { owner: f.owner, acl: f.acl.clone(), kind: AclObject::Function });
    }
    builtin_proc(oid).map(|_| builtin_owned(builtin::BOOTSTRAP_SUPERUSER, AclObject::Function))
}

fn owned_type(snap: &CatalogSnapshot, oid: Oid) -> Option<Owned> {
    if let Some(t) = snap.type_obj(oid) {
        return Some(Owned { owner: t.owner, acl: t.acl.clone(), kind: AclObject::Type });
    }
    match snap.user_type(oid) {
        Some(UserTypeRef::RowType(rel) | UserTypeRef::RowTypeArray(rel)) => {
            snap.relation(rel).map(|r| builtin_owned(r.owner, AclObject::Type))
        }
        Some(UserTypeRef::TypeArray(elem)) => snap.type_obj(elem).map(|t| builtin_owned(t.owner, AclObject::Type)),
        // Enum/domain types are answered by `type_obj` above.
        Some(UserTypeRef::Type(_)) => None,
        None => builtin_type(oid).map(|_| builtin_owned(builtin::BOOTSTRAP_SUPERUSER, AclObject::Type)),
    }
}

/// Column attnum by name (PG `convert_column_name`): dropped columns are NULL (`None`), unknown
/// columns of an existing relation are 42703, system column names map to a negative attnum.
fn column_attnum(env: &CatalogEnv, oid: Oid, name: &str) -> Result<Option<i16>> {
    let snap = env.snapshot();
    if let Some(rel) = snap.relation(oid) {
        if let Some(col) = rel.columns.iter().find(|c| c.name == name) {
            return Ok(if col.dropped { None } else { Some(col.attnum) });
        }
        if SYSTEM_COLUMNS.contains(&name) {
            return Ok(Some(-1));
        }
        return Err(errors::column_of_relation_does_not_exist(name, &rel.name));
    }
    match snap.class(oid) {
        None => Ok(None),
        Some(_) => Err(errors::column_of_relation_does_not_exist(name, &class_name(snap, oid))),
    }
}

/// Column argument (`name` or `int2` attnum) to attnum; `None` when the table is unknown.
fn resolve_attnum(env: &CatalogEnv, oid: Option<Oid>, col: &Datum) -> Result<Option<i16>> {
    let Some(oid) = oid else {
        return Ok(None);
    };
    match col {
        Datum::Text(name) => column_attnum(env, oid, name.as_str()),
        Datum::Int2(attnum) => Ok(Some(*attnum)),
        _ => Err(PgError::internal("column argument has an unexpected type")),
    }
}

/// PG `column_privilege_check`: column-level ACL first, then table-level. `None` = NULL (column
/// missing or dropped, or the table unknown). attnum 0 is never a column; negative attnums (system
/// columns) use the table-level check only.
fn column_check(
    env: &CatalogEnv,
    graph: &RoleGraph,
    user: Oid,
    oid: Oid,
    attnum: i16,
    mask: PrivMask,
) -> Result<Option<bool>> {
    let snap = env.snapshot();
    if attnum == 0 {
        return Ok(None);
    }
    if attnum > 0 {
        let Some(rel) = snap.relation(oid) else {
            return Ok(None);
        };
        let Some(col) = rel.columns.iter().find(|c| c.attnum == attnum && !c.dropped) else {
            return Ok(None);
        };
        let col_acl = acl::effective_acl(AclObject::Column, rel.owner, col.acl.as_ref())?;
        if mask_any(graph, user, rel.owner, &col_acl, mask)? {
            return Ok(Some(true));
        }
    }
    let Some(table) = class_owned(snap, oid) else {
        return Ok(None);
    };
    Ok(Some(table.check(graph, user, mask)?))
}

// ------------------------------------------------------------------------------------------------
// Object privilege functions
// ------------------------------------------------------------------------------------------------

/// has_table_privilege / has_sequence_privilege. Sequences additionally require a sequence object
/// (42809 `"x" is not a sequence` for other relations).
fn class_privilege(ctx: &CatalogCallCtx, args: &[Datum], words: &[(&str, u32)], sequence: bool) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 1)?;
    let user = user_of(env, p.user, true)?;
    let r = obj_ref(&p.objs[0])?;
    let obj_first = matches!(r, Ref::Name(_)) && !sequence;
    let (oid, mask) = ordered(
        obj_first,
        || class_target(env, r),
        || -> Result<PrivMask> { parse_privs(text_arg(p.priv_arg)?, words, false) },
    )?;
    let Some(oid) = oid else {
        return Ok(Datum::Null);
    };
    let snap = env.snapshot();
    if sequence && !matches!(snap.class(oid), Some(ClassRef::Sequence(_))) {
        let shown = match r {
            Ref::Name(s) => s.to_string(),
            Ref::Id(_) => class_name(snap, oid),
        };
        return Err(errors::not_a(&shown, "sequence"));
    }
    let Some(owned) = class_owned(snap, oid) else {
        return Ok(Datum::Null);
    };
    let graph = RoleGraph::build(snap, env.database())?;
    Ok(Datum::Bool(owned.check(&graph, user, mask)?))
}

/// has_any_column_privilege: table-level OR any non-dropped column's ACL.
fn has_any_column(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 1)?;
    let user = user_of(env, p.user, true)?;
    let r = obj_ref(&p.objs[0])?;
    let obj_first = matches!(r, Ref::Name(_));
    let (oid, mask) = ordered(
        obj_first,
        || class_target(env, r),
        || -> Result<PrivMask> { parse_privs(text_arg(p.priv_arg)?, COLUMN_WORDS, false) },
    )?;
    let Some(oid) = oid else {
        return Ok(Datum::Null);
    };
    let snap = env.snapshot();
    let Some(table) = class_owned(snap, oid) else {
        return Ok(Datum::Null);
    };
    let graph = RoleGraph::build(snap, env.database())?;
    if table.check(&graph, user, mask)? {
        return Ok(Datum::Bool(true));
    }
    if let Some(rel) = snap.relation(oid) {
        for col in rel.columns.iter().filter(|c| !c.dropped) {
            let col_acl = acl::effective_acl(AclObject::Column, rel.owner, col.acl.as_ref())?;
            if mask_any(&graph, user, rel.owner, &col_acl, mask)? {
                return Ok(Datum::Bool(true));
            }
        }
    }
    Ok(Datum::Bool(false))
}

/// has_column_privilege(table, column, priv) and the (user, table, column, priv) form.
fn has_column(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 2)?;
    let user = user_of(env, p.user, true)?;
    let r = obj_ref(&p.objs[0])?;
    let obj_first = matches!(r, Ref::Name(_));
    let col = &p.objs[1];
    let (resolved, mask) = ordered(
        obj_first,
        || -> Result<(Option<Oid>, Option<i16>)> {
            let oid = class_target(env, r)?;
            let attnum = resolve_attnum(env, oid, col)?;
            Ok((oid, attnum))
        },
        || -> Result<PrivMask> { parse_privs(text_arg(p.priv_arg)?, COLUMN_WORDS, false) },
    )?;
    let (oid, attnum) = resolved;
    let (Some(oid), Some(attnum)) = (oid, attnum) else {
        return Ok(Datum::Null);
    };
    let graph = RoleGraph::build(env.snapshot(), env.database())?;
    match column_check(env, &graph, user, oid, attnum, mask)? {
        Some(allowed) => Ok(Datum::Bool(allowed)),
        None => Ok(Datum::Null),
    }
}

/// Resolves an object of a non-relation kind (schema, database, function, language, type,
/// tablespace). `Ok(None)` for an OID that does not exist (NULL result).
type ResolveFn = for<'a, 'b> fn(&'a CatalogEnv, Ref<'b>) -> Result<Option<Owned>>;

fn simple_privilege(
    ctx: &CatalogCallCtx,
    args: &[Datum],
    words: &[(&str, u32)],
    resolve: ResolveFn,
) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 1)?;
    let user = user_of(env, p.user, true)?;
    let r = obj_ref(&p.objs[0])?;
    let obj_first = matches!(r, Ref::Name(_));
    let (owned, mask) = ordered(
        obj_first,
        || resolve(env, r),
        || -> Result<PrivMask> { parse_privs(text_arg(p.priv_arg)?, words, false) },
    )?;
    let Some(owned) = owned else {
        return Ok(Datum::Null);
    };
    let graph = RoleGraph::build(env.snapshot(), env.database())?;
    Ok(Datum::Bool(owned.check(&graph, user, mask)?))
}

fn resolve_schema(env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    let oid = match r {
        Ref::Name(s) => env.namespace_oid(s)?,
        Ref::Id(o) => o,
    };
    Ok(owned_namespace(env.snapshot(), oid))
}

fn resolve_database(env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    let snap = env.snapshot();
    let db = match r {
        Ref::Name(s) => match snap.database_by_name(s) {
            Some(db) => db,
            None => return Err(errors::database_does_not_exist(s)),
        },
        Ref::Id(o) => match snap.database(o) {
            Some(db) => db,
            None => return Ok(None),
        },
    };
    Ok(Some(Owned { owner: db.owner, acl: db.acl.clone(), kind: AclObject::Database }))
}

fn resolve_function(env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    let oid = match r {
        Ref::Name(s) => match env.reg_lookup_impl(RegKind::Procedure, s)? {
            Some(oid) => oid,
            None => {
                return Err(PgError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function \"{s}\" does not exist"),
                ))
            }
        },
        Ref::Id(o) => o,
    };
    Ok(owned_function(env.snapshot(), oid))
}

fn resolve_language(_env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    match r {
        Ref::Name(s) => match builtin::LANGUAGES.iter().find(|l| l.name == s) {
            Some(l) => Ok(Some(builtin_owned(l.owner, AclObject::Language))),
            None => Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("language \"{s}\" does not exist"))),
        },
        Ref::Id(o) => Ok(builtin::LANGUAGES
            .iter()
            .find(|l| l.oid == o)
            .map(|l| builtin_owned(l.owner, AclObject::Language))),
    }
}

fn resolve_tablespace(_env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    match r {
        Ref::Name(s) => match builtin::TABLESPACES.iter().find(|t| t.name == s) {
            Some(t) => Ok(Some(builtin_owned(t.owner, AclObject::Tablespace))),
            None => Err(PgError::new(SqlState::UNDEFINED_OBJECT, format!("tablespace \"{s}\" does not exist"))),
        },
        Ref::Id(o) => Ok(builtin::TABLESPACES
            .iter()
            .find(|t| t.oid == o)
            .map(|t| builtin_owned(t.owner, AclObject::Tablespace))),
    }
}

fn resolve_type(env: &CatalogEnv, r: Ref<'_>) -> Result<Option<Owned>> {
    let oid = match r {
        Ref::Name(s) => pgarbor_types::names::parse_type_string(env, s)?.oid,
        Ref::Id(o) => o,
    };
    Ok(owned_type(env.snapshot(), oid))
}

/// Foreign data wrapper / server / large object variants: the privilege string is validated, then
/// NULL is returned for every object (none exist in PGArbor).
fn null_privilege(args: &[Datum], words: &[(&str, u32)]) -> Result<Datum> {
    let p = split_args(args, 1)?;
    parse_privs(text_arg(p.priv_arg)?, words, false)?;
    Ok(Datum::Null)
}

/// has_parameter_privilege(name, priv) / (role, name, priv): true for superusers only (no
/// `pg_parameter_acl` grants exist in v1).
fn has_parameter(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 1)?;
    let user = user_of(env, p.user, true)?;
    let _parameter = text_arg(&p.objs[0])?;
    parse_privs(text_arg(p.priv_arg)?, PARAMETER_WORDS, false)?;
    let superuser = env.snapshot().role(user).is_some_and(|r| r.superuser);
    Ok(Datum::Bool(superuser))
}

// ------------------------------------------------------------------------------------------------
// Role membership functions
// ------------------------------------------------------------------------------------------------

/// PG `is_admin_of_role`: superusers; otherwise some role that `member` belongs to (any membership
/// edge, transitively, including `member`) holds the membership of `role` WITH ADMIN OPTION. A role
/// has no admin option on itself.
fn is_admin_of(snap: &CatalogSnapshot, graph: &RoleGraph, member: Oid, role: Oid) -> bool {
    if graph.is_superuser(member) {
        return true;
    }
    if member == role {
        return false;
    }
    let memberships = &snap.data().memberships;
    let mut seen: BTreeSet<Oid> = BTreeSet::new();
    seen.insert(member);
    let mut stack = vec![member];
    while let Some(cur) = stack.pop() {
        for m in memberships.values() {
            if m.member == cur && seen.insert(m.role) {
                stack.push(m.role);
            }
        }
    }
    memberships.values().any(|m| m.role == role && m.admin_option && seen.contains(&m.member))
}

/// PG `pg_role_aclcheck`: any requested bit suffices.
fn role_check(snap: &CatalogSnapshot, graph: &RoleGraph, member: Oid, role: Oid, mask: PrivMask) -> Result<bool> {
    let superuser = graph.is_superuser(member);
    if mask.grant != 0 && is_admin_of(snap, graph, member, role) {
        return Ok(true);
    }
    if (mask.plain & acl_mode::CREATE) != 0 && (member == role || superuser || graph.is_member_of_role(member, role)?) {
        return Ok(true);
    }
    if (mask.plain & acl_mode::USAGE) != 0 && (member == role || superuser || graph.has_privs_of_role(member, role)?) {
        return Ok(true);
    }
    if (mask.plain & acl_mode::SET) != 0 && (member == role || superuser || graph.can_set_role(member, role)?) {
        return Ok(true);
    }
    Ok(false)
}

/// pg_has_role(user, role, priv) / pg_has_role(role, priv). Role names must exist (42704); `public`
/// is not a role here.
fn pg_has_role(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    let p = split_args(args, 1)?;
    let member = user_of(env, p.user, false)?;
    let role = role_arg(env, &p.objs[0], false)?;
    let mask = parse_privs(text_arg(p.priv_arg)?, ROLE_WORDS, true)?;
    let snap = env.snapshot();
    let graph = RoleGraph::build(snap, env.database())?;
    Ok(Datum::Bool(role_check(snap, &graph, member, role, mask)?))
}

// ------------------------------------------------------------------------------------------------
// row_security_active
// ------------------------------------------------------------------------------------------------

/// row_security_active(table): false for an existing relation (no policies in v1), NULL for an
/// unknown OID, 42P01 for an unknown name.
fn row_security(ctx: &CatalogCallCtx, args: &[Datum]) -> Result<Datum> {
    let env = ctx.env;
    let first = args
        .first()
        .ok_or_else(|| PgError::internal("row_security_active requires one argument"))?;
    match obj_ref(first)? {
        Ref::Name(s) => {
            class_by_text(env, s)?;
            Ok(Datum::Bool(false))
        }
        Ref::Id(o) => {
            if env.snapshot().class(o).is_some() {
                Ok(Datum::Bool(false))
            } else {
                Ok(Datum::Null)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullSessionInfo;
    use crate::testing;
    use pgarbor_types::IoConfig;

    fn call_named(env: &CatalogEnv, name: &str, args: &[Datum]) -> Result<Datum> {
        let proc = pgarbor_types::catalog_data::procs_named(name).next().expect("builtin privilege proc");
        let io = IoConfig::default();
        let session = NullSessionInfo;
        let ctx = CatalogCallCtx { env, session: &session, io_config: &io };
        call(&ctx, proc, args)
    }

    fn t(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }

    #[test]
    fn parse_privs_words_and_grant_option() {
        let m = parse_privs("select, INSERT WITH GRANT OPTION", TABLE_WORDS, false).unwrap();
        assert_eq!(m.plain, acl_mode::SELECT);
        assert_eq!(m.grant, acl_mode::INSERT);
        let m = parse_privs("  update  ", TABLE_WORDS, false).unwrap();
        assert_eq!(m.plain, acl_mode::UPDATE);
        assert_eq!(m.grant, 0);
    }

    #[test]
    fn parse_privs_rejects_unknown_and_empty() {
        let e = parse_privs("foo", TABLE_WORDS, false).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "unrecognized privilege type: \"foo\"");
        assert!(parse_privs("", TABLE_WORDS, false).is_err());
        assert!(parse_privs("SELECT,", TABLE_WORDS, false).is_err());
        // Admin option is only a role word.
        assert!(parse_privs("SELECT WITH ADMIN OPTION", TABLE_WORDS, false).is_err());
        assert!(parse_privs("USAGE WITH ADMIN OPTION", ROLE_WORDS, true).unwrap().grant != 0);
    }

    #[test]
    fn parse_privs_alter_system_has_space() {
        let m = parse_privs("alter system, SET", PARAMETER_WORDS, false).unwrap();
        assert_eq!(m.plain, acl_mode::ALTER_SYSTEM | acl_mode::SET);
    }

    #[test]
    fn split_qualified_names() {
        assert_eq!(split_qualified("T").unwrap(), vec!["t".to_string()]);
        assert_eq!(split_qualified("\"Mixed\".T").unwrap(), vec!["Mixed".to_string(), "t".to_string()]);
        assert_eq!(split_qualified("\"a\"\"b\"").unwrap(), vec!["a\"b".to_string()]);
        assert_eq!(split_qualified("a.b.c").unwrap_err().sqlstate, SqlState::INVALID_NAME);
        assert!(split_qualified("").is_err());
    }

    #[test]
    fn superuser_owns_builtin_table_privileges() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(call_named(&env, "has_table_privilege", &[t("pg_class"), t("SELECT")]).unwrap(), Datum::Bool(true));
        assert_eq!(
            call_named(&env, "has_table_privilege", &[t("postgres"), t("pg_class"), t("select, insert")]).unwrap(),
            Datum::Bool(true)
        );
    }

    #[test]
    fn unknown_privilege_word_is_22023() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let e = call_named(&env, "has_table_privilege", &[t("pg_class"), t("foo")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "unrecognized privilege type: \"foo\"");
    }

    #[test]
    fn unknown_table_name_is_42p01_and_unknown_oid_is_null() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let e = call_named(&env, "has_table_privilege", &[t("nosuch"), t("SELECT")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_TABLE);
        assert_eq!(e.message, "relation \"nosuch\" does not exist");
        assert_eq!(
            call_named(&env, "has_table_privilege", &[Datum::Oid(u32::MAX), t("SELECT")]).unwrap(),
            Datum::Null
        );
    }

    #[test]
    fn unknown_user_name_is_42704() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let e = call_named(&env, "has_table_privilege", &[t("no_such_role"), t("pg_class"), t("SELECT")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "role \"no_such_role\" does not exist");
    }

    #[test]
    fn public_schema_usage_and_pg_has_role() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(call_named(&env, "has_schema_privilege", &[t("public"), t("USAGE")]).unwrap(), Datum::Bool(true));
        assert_eq!(
            call_named(&env, "pg_has_role", &[t("postgres"), t("pg_monitor"), t("USAGE")]).unwrap(),
            Datum::Bool(true)
        );
    }

    #[test]
    fn sequence_check_rejects_tables() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        let e = call_named(&env, "has_sequence_privilege", &[t("pg_class"), t("USAGE")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::WRONG_OBJECT_TYPE);
        assert_eq!(e.message, "\"pg_class\" is not a sequence");
    }

    #[test]
    fn column_privileges_and_unknown_column() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(
            call_named(&env, "has_column_privilege", &[t("pg_class"), t("relname"), t("SELECT")]).unwrap(),
            Datum::Bool(true)
        );
        let e = call_named(&env, "has_column_privilege", &[t("pg_class"), t("nosuchcol"), t("SELECT")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_COLUMN);
        assert_eq!(e.message, "column \"nosuchcol\" of relation \"pg_class\" does not exist");
    }

    #[test]
    fn row_security_is_false_for_relations() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(call_named(&env, "row_security_active", &[t("pg_class")]).unwrap(), Datum::Bool(false));
    }

    #[test]
    fn parameter_privilege_superuser_only() {
        let (_kv, env) = testing::bootstrapped().unwrap();
        assert_eq!(
            call_named(&env, "has_parameter_privilege", &[t("work_mem"), t("SET")]).unwrap(),
            Datum::Bool(true)
        );
    }
}
