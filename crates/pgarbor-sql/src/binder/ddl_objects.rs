//! CREATE SCHEMA / SEQUENCE / TYPE / DOMAIN / ROLE / DATABASE (SQL-35). PG references:
//! `schemacmds.c` (`CreateSchemaCommand`), `sequence.c` (`DefineSequence`, `init_params`),
//! `typecmds.c` (`DefineEnum`, `DefineCompositeType`, `DefineDomain`), `user.c` (`CreateRole`),
//! `dbcommands.c` (`createdb`), `gram.y`.
//!
//! * [`bind_create_schema`] -> `BoundStatement::CreateSchema { name, owner, if_not_exists }`; `CREATE
//!   SCHEMA s AUTHORIZATION r` (owner resolved by `env.role_oid`, default current user; name defaults to the
//!   role name when only AUTHORIZATION given); schema elements in the same statement (`CREATE SCHEMA s
//!   CREATE TABLE ...`): 0A000. `IF NOT EXISTS` + elements: 42601. Names starting with `pg_`: 42939
//!   `unacceptable schema name "pg_x"` DETAIL `The prefix "pg_" is reserved for system schemas.`
//! * [`bind_create_sequence`] -> `CreateSequence(Box<CreateSequenceSpec>)`: options `AS type`
//!   (smallint/integer/bigint else 22023? `sequence type must be smallint, integer, or bigint`), `INCREMENT [BY] n`
//!   (!= 0), `MINVALUE n|NO MINVALUE`, `MAXVALUE n|NO MAXVALUE`, `START [WITH] n`, `CACHE n` (>= 1),
//!   `[NO] CYCLE`, `OWNED BY t.c|NONE`; defaults by sign of increment and type; all range validations with
//!   the PG messages (22023 `MINVALUE (x) must be less than MAXVALUE (y)`, `START value (x) cannot be
//!   less than MINVALUE (y)`, `START value (x) cannot be greater than MAXVALUE (y)`, `CACHE (x) must be
//!   greater than zero`, `INCREMENT must not be zero`, `MAXVALUE (x) is out of range for sequence data
//!   type integer`). Numbers parsed as i128 text then range-checked (values beyond i64: 22003 `value "99999999999999999999" is out of
//!   range for type bigint`, oracle). `TEMP`: 0A000. `SequenceOptions`/`SeqBound` structures
//!   are in `ddl::spec`. Shared with identity columns via [`bind_sequence_options`].
//! * [`bind_create_type`] (`Statement::CreateType { name, representation }`): `AS ENUM ('a','b')` ->
//!   `CreateEnum(CreateEnumSpec)` (duplicate label 42710 `enum label "a" already exists`? at create:
//!   `22023`? check `DefineEnum`; label longer than 63 bytes 42622 `invalid enum label "x"` DETAIL `Labels
//!   must be 63 bytes or less.`; zero labels allowed), `AS (a int, b text)` -> `CreateComposite`
//!   (attribute names unique 42701; types resolved), range/base/shell types 0A000.
//! * [`bind_create_domain`] -> `CreateDomain(Box<CreateDomainSpec>)`: `AS base [COLLATE c] [DEFAULT e]
//!   [NOT NULL|NULL] [CONSTRAINT n CHECK (expr)]...`; CHECK expressions are written in terms of `VALUE`
//!   (bind with a synthetic one-column scope `VALUE` of the base type; stored canonical text uses
//!   `VALUE`); `base` may be an array/enum/domain; `NOT VALID` on CHECK allowed at creation? (PG: only via
//!   ALTER).
//! * [`bind_create_role`] (`CreateRole` and `CreateUser`, `CREATE USER` implies LOGIN): options
//!   `SUPERUSER|NOSUPERUSER, CREATEDB, CREATEROLE, INHERIT, LOGIN, REPLICATION, BYPASSRLS, CONNECTION
//!   LIMIT n, [ENCRYPTED] PASSWORD 'p'|NULL, VALID UNTIL 'ts', IN ROLE a,b, ROLE m, ADMIN a, SYSID
//!   (ignored), USER (member list)`; role names via `env.role_oid` (42704); reserved name prefix `pg_`:
//!   42939 `role name "pg_x" is reserved` DETAIL `Role names starting with "pg_" are reserved.`
//!   `password` is stored as given here; hashing (SCRAM) is the executor's.
//! * [`bind_create_database`] (`Statement::CreateDatabase { db_name, if_not_exists, location, ..}`
//!   plus sqlparser option list): `CREATE DATABASE n [OWNER r] [TEMPLATE t] [ENCODING 'e'] [LOCALE ..]
//!   [CONNECTION LIMIT n] [IS_TEMPLATE b] [ALLOW_CONNECTIONS b]` -> `CreateDatabase(BoundCreateDatabase)`.
//!   Only UTF8 encoding is accepted (22023 `new encoding (LATIN1) is incompatible with ...` otherwise).
//!   Cannot run inside a transaction block (25001 `CREATE DATABASE cannot run inside a transaction
//!   block`) — that check belongs to the session layer, not here.
use pgarbor_catalog::ddl::spec::{
    ColumnSpec, CreateCompositeSpec, CreateDomainSpec, CreateEnumSpec, CreateRoleSpec, CreateSequenceSpec,
    DomainConstraintSpec, SeqBound, SequenceOptions,
};
use pgarbor_catalog::errors;
use pgarbor_catalog::model::Persistence;
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::StoredExpr;
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::{oids, Datum, TypeId};
use sqlparser::ast::{
    Expr, Ident, ObjectName, ObjectNamePart, SchemaName, SequenceOptions as AstSequenceOption, Statement,
    TableConstraint, UnaryOperator, UserDefinedTypeRepresentation, Value,
};

use crate::parser::{DOMAIN_NOT_NULL_MARKER, DOMAIN_NULL_MARKER};

use super::context::{Binder, ExprCtx};
use super::stored;
use super::types;
use crate::diag;
use crate::extra_stmt::{RoleOptionAst, RoleOptionItem, RoleStmtKind, SequenceOptionAst};
use crate::ir::{BoundCreateDatabase, BoundStatement};

/// Column names that PG rejects in a composite type (`SystemAttributeByName`).
const SYSTEM_COLUMN_NAMES: [&str; 6] = ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"];

/// Position of the first identifier of an object name (error caret).
fn obj_pos(b: &Binder, name: &ObjectName) -> Option<u32> {
    match name.0.first() {
        Some(ObjectNamePart::Identifier(id)) => b.pos_span(id.span),
        _ => None,
    }
}

/// Position of the base type of `CREATE DOMAIN name [AS] type ...`: the first token after the domain
/// name and an optional `AS` (PG's `typeName->location`). `None` when the name has no source span.
fn domain_type_pos(b: &Binder, name: &ObjectName) -> Option<u32> {
    let Some(ObjectNamePart::Identifier(last)) = name.0.last() else {
        return None;
    };
    let bytes = b.src.text().as_bytes();
    let skip_blanks = |mut i: usize| {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    let mut i = skip_blanks(b.src.byte_offset(last.span.end)?);
    if bytes.get(i..i + 2).is_some_and(|w| w.eq_ignore_ascii_case(b"AS")) && bytes.get(i + 2).is_some_and(u8::is_ascii_whitespace) {
        i = skip_blanks(i + 2);
    }
    Some(b.src.pos_of_byte(i))
}

/// `Binder::pos` of an expression, falling back to `fallback`.
fn expr_pos(b: &Binder, e: &Expr, fallback: Option<u32>) -> Option<u32> {
    b.pos(e).or(fallback)
}

/// Syntax error at an expression (`syntax error at or near "<text>"`).
fn syntax_at_expr(b: &Binder, e: &Expr, fallback: Option<u32>) -> PgError {
    diag::syntax_error_near(&e.to_string(), expr_pos(b, e, fallback))
}

/// A construct PGArbor does not implement (0A000).
fn unsupported(what: &str, pos: Option<u32>) -> PgError {
    let mut e = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by PGArbor yet"));
    e.position = pos;
    e
}

/// 42601 `conflicting or redundant options` (PG `errorConflictingDefElem`).
fn conflicting_options(pos: Option<u32>) -> PgError {
    diag::syntax("conflicting or redundant options", pos)
}

/// Schema-qualified object name -> (schema, name, position). A catalog part that is not the
/// connected database is 0A000 (cross-database reference).
fn target_name(b: &mut Binder, name: &ObjectName) -> Result<(Option<String>, String, Option<u32>)> {
    let q = b.qual(name)?;
    if let Some(cat) = &q.catalog {
        if *cat != b.ctx.session.database {
            let full = format!("{}.{}.{}", cat, q.schema.as_deref().unwrap_or(""), q.name);
            return Err(diag::cross_database(&full, q.pos));
        }
    }
    Ok((q.schema, q.name, q.pos))
}

/// An unqualified name that must be a single identifier (schema, role, database names).
fn single_name(b: &mut Binder, name: &ObjectName) -> Result<String> {
    let q = b.qual(name)?;
    if q.catalog.is_some() || q.schema.is_some() {
        let dot = match name.0.first() {
            Some(sqlparser::ast::ObjectNamePart::Identifier(id)) => super::ddl_index::dot_position(b, id),
            _ => None,
        };
        return Err(diag::syntax_error_near(".", dot.or(q.pos)));
    }
    Ok(q.name)
}

/// Target namespace of CREATE for `name` (search path / explicit schema), with the position attached.
fn creation_namespace(b: &Binder, schema: Option<&str>, name: &str, pos: Option<u32>) -> Result<Oid> {
    let env = b.env;
    env.creation_namespace_for(schema, name).map_err(|e| diag::with_position(e, pos))
}

/// Role OID of an identifier (42704 `role "x" does not exist`).
fn role_oid(b: &mut Binder, id: &Ident) -> Result<Oid> {
    let name = b.ident(id);
    let env = b.env;
    env.role_oid(&name)
}

/// An integer literal with its sign folded in (`-5` is `UnaryOp(Minus, 5)`), if the expression is one.
fn signed_number(e: &Expr) -> Option<(bool, &str)> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => Some(match s.strip_prefix('-') { Some(digits) => (true, digits), None => (false, s.strip_prefix('+').unwrap_or(s.as_str())) }),
            _ => None,
        },
        Expr::UnaryOp { op: UnaryOperator::Minus, expr, .. } => signed_number(expr).map(|(neg, s)| (!neg, s)),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr, .. } => signed_number(expr),
        Expr::Nested(inner) => signed_number(inner),
        _ => None,
    }
}

/// PG `defGetInt64` on a number token: digits only (22P02 otherwise), range of bigint (22003).
fn int_from_text(neg: bool, text: &str, pos: Option<u32>) -> Result<i64> {
    let shown = if neg { format!("-{text}") } else { text.to_string() };
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        let e = PgError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type bigint: \"{shown}\""));
        return Err(diag::with_position(e, pos));
    }
    let range_err = || {
        diag::with_position(
            PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{shown}\" is out of range for type bigint")),
            pos,
        )
    };
    let magnitude: i128 = match text.parse::<i128>() {
        Ok(v) => v,
        Err(_) => return Err(range_err()),
    };
    let value = if neg { -magnitude } else { magnitude };
    match i64::try_from(value) {
        Ok(v) => Ok(v),
        Err(_) => Err(range_err()),
    }
}

/// An integer-valued option (PG `NumericOnly`): number literal -> i64, anything else is a syntax error.
fn seq_int(b: &Binder, e: &Expr, pos: Option<u32>) -> Result<i64> {
    match signed_number(e) {
        Some((neg, text)) => int_from_text(neg, text, expr_pos(b, e, pos)),
        None => Err(syntax_at_expr(b, e, pos)),
    }
}

/// Sequence type range (PG `init_params` maxima), `None` for a non-sequence type.
fn seq_type_range(type_oid: Oid) -> Option<(i64, i64)> {
    match type_oid {
        oids::INT2 => Some((i16::MIN as i64, i16::MAX as i64)),
        oids::INT4 => Some((i32::MIN as i64, i32::MAX as i64)),
        oids::INT8 => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// `format_type` name used in the sequence range messages.
fn seq_type_name(type_oid: Oid) -> &'static str {
    match type_oid {
        oids::INT2 => "smallint",
        oids::INT4 => "integer",
        _ => "bigint",
    }
}

/// Default MAXVALUE of a new sequence (PG `init_params`): ascending uses the type maximum, descending -1.
fn default_max(increment: i64, type_max: i64) -> i64 {
    if increment > 0 {
        type_max
    } else {
        -1
    }
}

/// Default MINVALUE of a new sequence (PG `init_params`): ascending 1, descending the type minimum.
fn default_min(increment: i64, type_min: i64) -> i64 {
    if increment < 0 {
        type_min
    } else {
        1
    }
}

/// `OWNED BY NONE`: the bare keyword NONE (an unquoted identifier spelled NONE).
fn is_none_name(name: &ObjectName) -> bool {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => id.quote_style.is_none() && id.value == "NONE",
        _ => false,
    }
}

/// `OWNED BY table.column` -> (table OID, attnum). `OWNED BY NONE` -> `None`. Relation kinds other than
/// tables/views are passed through with attnum 0: the catalog reports PG's 42809 for them.
fn bind_owned_by(b: &mut Binder, name: &ObjectName) -> Result<Option<(Oid, i16)>> {
    let pos = obj_pos(b, name);
    if is_none_name(name) {
        return Ok(None);
    }
    if name.0.len() < 2 {
        let e = PgError::new(SqlState::SYNTAX_ERROR, "invalid OWNED BY option")
            .with_hint("Specify OWNED BY table.column or OWNED BY NONE.");
        return Err(diag::with_position(e, pos));
    }
    let column = match &name.0[name.0.len() - 1] {
        ObjectNamePart::Identifier(id) => b.ident(id),
        ObjectNamePart::Function(_) => return Err(diag::syntax("syntax error", pos)),
    };
    let table_name = ObjectName(name.0[..name.0.len() - 1].to_vec());
    let (schema, table, tpos) = target_name(b, &table_name)?;
    let env = b.env;
    let class = env.resolve_class(schema.as_deref(), &table).map_err(|e| diag::with_position(e, tpos))?;
    match class {
        ClassRef::Relation(oid) => {
            let rel = env
                .snapshot()
                .relation(oid)
                .ok_or_else(|| diag::with_position(errors::cache_lookup_failed("relation", oid), tpos))?;
            match rel.columns.iter().find(|c| !c.dropped && c.name == column) {
                Some(col) => Ok(Some((oid, col.attnum))),
                None => Err(diag::column_of_relation_missing(&column, &table, None)),
            }
        }
        other => Ok(Some((other.oid(), 0))),
    }
}

/// `Statement::CreateSchema`.
pub fn bind_create_schema(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::CreateSchema { schema_name, or_replace, if_not_exists, with, options, default_collate_spec, clone } =
        stmt
    else {
        return Err(PgError::internal("binder: bind_create_schema called for another statement"));
    };
    if *or_replace {
        return Err(diag::syntax("syntax error at or near \"OR\"", None));
    }
    if with.is_some() || options.is_some() || default_collate_spec.is_some() || clone.is_some() {
        return Err(unsupported("CREATE SCHEMA with options, collation or clone", None));
    }
    let env = b.env;
    // `CREATE SCHEMA AUTHORIZATION r` names the schema after the role.
    let (name, owner) = match schema_name {
        SchemaName::Simple(n) => (single_name(b, n)?, env.current_user()),
        SchemaName::UnnamedAuthorization(auth) => {
            // CURRENT_USER / CURRENT_ROLE / SESSION_USER are role specifications, not role names.
            let special = if auth.quote_style.is_none() {
                match auth.value.to_ascii_lowercase().as_str() {
                    "current_user" | "current_role" => Some(env.current_user()),
                    "session_user" => Some(env.session_user()),
                    _ => None,
                }
            } else {
                None
            };
            match special {
                Some(owner) => {
                    let role = env
                        .snapshot()
                        .role_name(owner)
                        .map(|n| n.to_string())
                        .or_else(|| pgarbor_catalog::builtin::builtin_role(owner).map(|r| r.name.to_string()))
                        .ok_or_else(|| PgError::internal("role of the session not found"))?;
                    (role, owner)
                }
                None => {
                    let role = b.ident(auth);
                    let owner = env.role_oid(&role)?;
                    (role, owner)
                }
            }
        }
        SchemaName::NamedAuthorization(n, auth) => {
            let name = single_name(b, n)?;
            let owner = match auth.value.to_ascii_lowercase().as_str() {
                "current_user" | "current_role" if auth.quote_style.is_none() => env.current_user(),
                "session_user" if auth.quote_style.is_none() => env.session_user(),
                _ => role_oid(b, auth)?,
            };
            (name, owner)
        }
    };
    if name.starts_with("pg_") {
        return Err(errors::reserved_schema_name(&name));
    }
    Ok(BoundStatement::CreateSchema { name, owner, if_not_exists: *if_not_exists })
}

/// Positions of the options of `CREATE SEQUENCE name <options>`, in written order (PG's `DefElem`
/// locations, used by `errorConflictingDefElem`). Empty when the statement text cannot be lexed.
fn seq_option_starts(b: &Binder, name: &ObjectName) -> Vec<Option<u32>> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::tokenizer::{Token, Tokenizer};
    let Some(ObjectNamePart::Identifier(last)) = name.0.last() else {
        return Vec::new();
    };
    let dialect = PostgreSqlDialect {};
    let Ok(toks) = Tokenizer::new(&dialect, b.src.text()).tokenize_with_location() else {
        return Vec::new();
    };
    let after = (last.span.end.line, last.span.end.column);
    let mut out: Vec<Option<u32>> = Vec::new();
    let mut prev: Option<String> = None;
    let mut depth = 0i32;
    // 0 = not in an OWNED BY name, 1 = expecting a name part, 2 = after a name part.
    let mut owned = 0u8;
    for t in toks.iter().filter(|t| (t.span.start.line, t.span.start.column) >= after) {
        match &t.token {
            Token::Whitespace(_) => continue,
            Token::SemiColon if depth == 0 => break,
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ => {}
        }
        if owned == 2 {
            if matches!(t.token, Token::Period) {
                owned = 1;
                prev = None;
                continue;
            }
            owned = 0;
        }
        if let Token::Word(w) = &t.token {
            if owned == 1 {
                owned = 2;
                prev = None;
                continue;
            }
            if depth == 0 && w.quote_style.is_none() {
                let up = w.value.to_ascii_uppercase();
                let after_no = prev.as_deref() == Some("NO") && matches!(up.as_str(), "CYCLE" | "MINVALUE" | "MAXVALUE");
                if up == "BY" && prev.as_deref() == Some("OWNED") {
                    owned = 1;
                } else if !after_no
                    && matches!(
                        up.as_str(),
                        "AS" | "CACHE" | "CYCLE" | "NO" | "INCREMENT" | "MAXVALUE" | "MINVALUE" | "OWNED" | "START" | "RESTART"
                            | "LOGGED" | "UNLOGGED" | "SEQUENCE"
                    )
                {
                    out.push(b.pos_span(t.span));
                }
                prev = Some(up);
                continue;
            }
        }
        prev = None;
    }
    out
}

/// `ExtraStatement::CreateSequence` (PG `DefineSequence` + `init_params`). The options arrive in the
/// order written; PG accepts any order, and a repeated option is 42601 `conflicting or redundant
/// options`.
pub fn bind_create_sequence(
    b: &mut Binder,
    temporary: bool,
    unlogged: bool,
    if_not_exists: bool,
    name: &ObjectName,
    options: &[SequenceOptionAst],
) -> Result<BoundStatement> {
    let pos = obj_pos(b, name);
    if temporary {
        return Err(unsupported("TEMPORARY sequences", pos));
    }
    if unlogged {
        return Err(unsupported("UNLOGGED sequences", pos));
    }
    let (schema, seq_name, qpos) = target_name(b, name)?;
    let namespace = creation_namespace(b, schema.as_deref(), &seq_name, qpos)?;
    // The option loop of `init_params`: conflicts and the two options CREATE rejects, in written order.
    let starts = seq_option_starts(b, name);
    let mut seen: Vec<&'static str> = Vec::new();
    let mut note = |key: &'static str, opos: Option<u32>| -> Result<()> {
        if seen.contains(&key) {
            return Err(conflicting_options(opos));
        }
        seen.push(key);
        Ok(())
    };
    let mut data_type = None;
    let mut owned_by: Option<&Option<ObjectName>> = None;
    let mut ast: Vec<AstSequenceOption> = Vec::with_capacity(options.len());
    for (idx, o) in options.iter().enumerate() {
        let opos = starts.get(idx).copied().flatten().or(pos);
        match o {
            SequenceOptionAst::As(dt) => {
                note("as", opos)?;
                data_type = Some(dt);
            }
            SequenceOptionAst::IncrementBy(t) => {
                note("increment", opos)?;
                ast.push(AstSequenceOption::IncrementBy(number_expr(t), true));
            }
            SequenceOptionAst::MinValue(v) => {
                note("minvalue", opos)?;
                ast.push(AstSequenceOption::MinValue(v.as_deref().map(number_expr)));
            }
            SequenceOptionAst::MaxValue(v) => {
                note("maxvalue", opos)?;
                ast.push(AstSequenceOption::MaxValue(v.as_deref().map(number_expr)));
            }
            SequenceOptionAst::StartWith(t) => {
                note("start", opos)?;
                ast.push(AstSequenceOption::StartWith(number_expr(t), true));
            }
            // `RESTART` is accepted and has no effect on a new sequence.
            SequenceOptionAst::Restart(_) => note("restart", opos)?,
            SequenceOptionAst::Cache(t) => {
                note("cache", opos)?;
                ast.push(AstSequenceOption::Cache(number_expr(t)));
            }
            SequenceOptionAst::Cycle(c) => {
                note("cycle", opos)?;
                ast.push(AstSequenceOption::Cycle(*c));
            }
            SequenceOptionAst::OwnedBy(v) => {
                note("owned_by", opos)?;
                owned_by = Some(v);
            }
            SequenceOptionAst::Logged(l) => {
                let word = if *l { "logged" } else { "unlogged" };
                return Err(PgError::internal(format!("option \"{word}\" not recognized")));
            }
            SequenceOptionAst::SequenceName => {
                return Err(diag::syntax("invalid sequence option SEQUENCE NAME", pos));
            }
        }
    }
    // `AS type` picks the sequence type; without it the type is bigint (PG `init_params`).
    let as_type = match data_type {
        None => oids::INT8,
        Some(dt) => {
            let resolved = types::resolve_data_type(b, dt, pos)?;
            if seq_type_range(resolved.id.oid).is_none() {
                return Err(errors::sequence_bad_type());
            }
            resolved.id.oid
        }
    };
    let bound = bind_sequence_options(b, &ast, as_type, None)?;
    let owned = match owned_by {
        Some(Some(n)) => bind_owned_by(b, n)?,
        _ => None,
    };
    let owner = b.env.current_user();
    let spec = CreateSequenceSpec {
        namespace,
        name: seq_name,
        owner,
        persistence: Persistence::Permanent,
        options: bound,
        owned_by: owned,
        if_not_exists,
    };
    Ok(BoundStatement::CreateSequence(Box::new(spec)))
}

/// A numeric option value kept as text (`-5`, `+3`, `99999999999999999999`) as an expression, so the
/// shared option binder range-checks it like a parsed literal.
pub(super) fn number_expr(text: &str) -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: Value::Number(text.to_string(), false),
        span: sqlparser::tokenizer::Span::empty(),
    })
}

/// Resolve sequence option lists (CREATE SEQUENCE, identity columns) into `SequenceOptions`,
/// applying type-dependent defaults and the range validations (module docs). `data_type` is the
/// column/sequence type oid (int2/int4/int8).
///
/// The result is fully specified (PG `init_params` for a new sequence): `type_oid`, `increment`,
/// `minvalue`, `maxvalue`, `start`, `cache` and `cycle` are all `Some`, with absent clauses replaced
/// by their defaults. Callers that resolve an existing sequence (ALTER) must not use this function.
pub fn bind_sequence_options(b: &mut Binder, options: &[AstSequenceOption], data_type: Oid, pos: Option<u32>) -> Result<SequenceOptions> {
    let Some((type_min, type_max)) = seq_type_range(data_type) else {
        return Err(diag::with_position(errors::sequence_bad_type(), pos));
    };
    let type_name = seq_type_name(data_type);
    let mut increment: Option<i64> = None;
    let mut min_opt: Option<SeqBound> = None;
    let mut max_opt: Option<SeqBound> = None;
    let mut start: Option<i64> = None;
    let mut cache: Option<i64> = None;
    let mut cycle: Option<bool> = None;

    for opt in options {
        match opt {
            AstSequenceOption::IncrementBy(e, _) => {
                if increment.is_some() {
                    return Err(conflicting_options(pos));
                }
                let v = seq_int(b, e, pos)?;
                if v == 0 {
                    return Err(errors::sequence_increment_zero());
                }
                increment = Some(v);
            }
            AstSequenceOption::MinValue(v) => {
                if min_opt.is_some() {
                    return Err(conflicting_options(pos));
                }
                min_opt = Some(match v {
                    Some(e) => SeqBound::Value(seq_int(b, e, pos)?),
                    None => SeqBound::Default,
                });
            }
            AstSequenceOption::MaxValue(v) => {
                if max_opt.is_some() {
                    return Err(conflicting_options(pos));
                }
                max_opt = Some(match v {
                    Some(e) => SeqBound::Value(seq_int(b, e, pos)?),
                    None => SeqBound::Default,
                });
            }
            AstSequenceOption::StartWith(e, _) => {
                if start.is_some() {
                    return Err(conflicting_options(pos));
                }
                start = Some(seq_int(b, e, pos)?);
            }
            AstSequenceOption::Cache(e) => {
                if cache.is_some() {
                    return Err(conflicting_options(pos));
                }
                cache = Some(seq_int(b, e, pos)?);
            }
            AstSequenceOption::Cycle(v) => {
                if cycle.is_some() {
                    return Err(conflicting_options(pos));
                }
                cycle = Some(*v);
            }
        }
    }

    let inc = increment.unwrap_or(1);
    // PG checks MAXVALUE before MINVALUE, then MINVALUE < MAXVALUE, then START, then CACHE.
    let max = match max_opt {
        Some(SeqBound::Value(v)) => v,
        _ => default_max(inc, type_max),
    };
    if max < type_min || max > type_max {
        return Err(errors::sequence_bound_out_of_range("MAXVALUE", max, type_name));
    }
    let min = match min_opt {
        Some(SeqBound::Value(v)) => v,
        _ => default_min(inc, type_min),
    };
    if min < type_min || min > type_max {
        return Err(errors::sequence_bound_out_of_range("MINVALUE", min, type_name));
    }
    if min >= max {
        return Err(errors::sequence_min_not_less_than_max(min, max));
    }
    let start_value = match start {
        Some(v) => v,
        None => {
            if inc > 0 {
                min
            } else {
                max
            }
        }
    };
    if start_value < min {
        return Err(errors::sequence_start_below_min(start_value, min));
    }
    if start_value > max {
        return Err(errors::sequence_start_above_max(start_value, max));
    }
    let cache_value = cache.unwrap_or(1);
    if cache_value < 1 {
        return Err(errors::sequence_cache_not_positive(cache_value));
    }

    Ok(SequenceOptions {
        type_oid: Some(data_type),
        increment: Some(inc),
        minvalue: Some(SeqBound::Value(min)),
        maxvalue: Some(SeqBound::Value(max)),
        start: Some(start_value),
        restart: None,
        cache: Some(cache_value),
        cycle: Some(cycle.unwrap_or(false)),
    })
}

/// `Statement::CreateType`.
pub fn bind_create_type(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::CreateType { name, representation } = stmt else {
        return Err(PgError::internal("binder: bind_create_type called for another statement"));
    };
    let pos = obj_pos(b, name);
    let (schema, type_name, qpos) = target_name(b, name)?;
    let namespace = creation_namespace(b, schema.as_deref(), &type_name, qpos)?;
    let env = b.env;
    let owner = env.current_user();
    match representation {
        Some(UserDefinedTypeRepresentation::Enum { labels }) => {
            let mut out: Vec<String> = Vec::with_capacity(labels.len());
            for id in labels {
                let label = id.value.clone();
                // PG `EnumValuesCreate`: labels are stored in a NAMEDATALEN field (63 bytes).
                if label.len() > 63 {
                    return Err(errors::invalid_enum_label(&label));
                }
                out.push(label);
            }
            // PG `DefineEnum` does not check duplicates; the unique index on pg_enum rejects them.
            for (i, label) in out.iter().enumerate() {
                if out[..i].contains(label) {
                    return Err(PgError::new(
                        SqlState::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
                    ));
                }
            }
            Ok(BoundStatement::CreateEnum(CreateEnumSpec { namespace, name: type_name, owner, labels: out }))
        }
        Some(UserDefinedTypeRepresentation::Composite { attributes }) => {
            let mut cols: Vec<ColumnSpec> = Vec::with_capacity(attributes.len());
            for attr in attributes {
                let attr_name = b.ident(&attr.name);
                let apos = b.pos_span(attr.name.span);
                if SYSTEM_COLUMN_NAMES.contains(&attr_name.as_str()) {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column name \"{attr_name}\" conflicts with a system column name"),
                    ));
                }
                if cols.iter().any(|c| c.name == attr_name) {
                    return Err(PgError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column name \"{attr_name}\" specified more than once"),
                    ));
                }
                let resolved = types::resolve_data_type(b, &attr.data_type, apos)?;
                if types::is_pseudo_type(resolved.id.oid) {
                    let shown = b.type_name(resolved.id);
                    return Err(PgError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        format!("column \"{attr_name}\" has pseudo-type {shown}"),
                    ));
                }
                let mut col = ColumnSpec::new(attr_name, resolved.id);
                col.ndims = resolved.ndims;
                if let Some(coll) = &attr.collation {
                    let q = b.qual(coll)?;
                    col.collation = Some(env.collation_oid(q.schema.as_deref(), &q.name)?);
                }
                cols.push(col);
            }
            Ok(BoundStatement::CreateComposite(CreateCompositeSpec { namespace, name: type_name, owner, attributes: cols }))
        }
        Some(UserDefinedTypeRepresentation::Range { .. })
        | Some(UserDefinedTypeRepresentation::SqlDefinition { .. })
        | None => Err(unsupported("CREATE TYPE without AS ENUM or AS (...) (range, base and shell types)", pos)),
    }
}

/// CHECK expression of a domain: bound in a one-column scope `VALUE` of the base type (module docs).
/// The stored text names the column `value` (unquoted, so it re-parses to the same column; a quoted
/// `"VALUE"` would not match the folded `value` scope column), as `ALTER DOMAIN ... ADD CHECK` does.
fn bind_domain_check(b: &mut Binder, e: &Expr, base: TypeId, domain_name: &str) -> Result<StoredExpr> {
    let columns = [("value".to_string(), base)];
    stored::table_expr_scope(b, domain_name, &columns)?;
    let mut refs: Vec<i16> = Vec::new();
    let names = |_: i16| -> Option<String> { Some("value".to_string()) };
    let result = stored::bind_ddl_expr(b, e, ExprCtx::check(), None, None, &mut refs, &names);
    b.scopes.pop();
    let (bound, stored_expr) = result?;
    if bound.ty.oid != oids::BOOL {
        let shown = b.type_name(bound.ty);
        return Err(diag::not_boolean("CHECK", &shown, expr_pos(b, e, None)));
    }
    Ok(stored_expr)
}

/// DEFAULT expression of a domain: no column references, coerced (assignment) to the base type.
fn bind_domain_default(b: &mut Binder, e: &Expr, base: TypeId) -> Result<StoredExpr> {
    let mut refs: Vec<i16> = Vec::new();
    let no_names = |_: i16| -> Option<String> { None };
    let (_bound, stored_expr) =
        stored::bind_ddl_expr(b, e, ExprCtx::column_default(), Some(base), None, &mut refs, &no_names)?;
    Ok(stored_expr)
}

/// `Statement::CreateDomain`.
pub fn bind_create_domain(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::CreateDomain(dom) = stmt else {
        return Err(PgError::internal("binder: bind_create_domain called for another statement"));
    };
    let pos = obj_pos(b, &dom.name);
    let (schema, name, qpos) = target_name(b, &dom.name)?;
    let namespace = creation_namespace(b, schema.as_deref(), &name, qpos)?;
    // PG reports type errors of CREATE DOMAIN at the base type name, not at the domain name.
    let type_pos = domain_type_pos(b, &dom.name).or(pos);
    let base = types::resolve_data_type(b, &dom.data_type, type_pos)?;
    // PG DefineDomain: only base, composite, domain, enum, range and multirange types may be the base
    // type; a pseudo-type (void, record, ...) is 42804 at the base type name.
    let base_is_valid = pgarbor_types::TypeEnv::type_info(b.env, base.id.oid)
        .is_some_and(|t| matches!(t.typtype, b'b' | b'c' | b'd' | b'e' | b'r' | b'm'));
    if !base_is_valid {
        let shown = pgarbor_types::names::display_name(b.env, base.id.oid);
        return Err(diag::with_position(
            PgError::new(SqlState::DATATYPE_MISMATCH, format!("\"{shown}\" is not a valid base type for a domain")),
            type_pos,
        ));
    }
    let env = b.env;
    let collation = match &dom.collation {
        Some(id) => {
            let coll_name = b.ident(id);
            Some(env.collation_oid(None, &coll_name)?)
        }
        None => None,
    };
    // PG DefineDomain: COLLATE over a base type that cannot collate is 42804, at the base type name.
    if collation.is_some() && !pgarbor_types::TypeEnv::type_info(env, base.id.oid).is_some_and(|t| t.is_collatable()) {
        let shown = b.type_name(base.id);
        return Err(diag::with_position(
            PgError::new(SqlState::DATATYPE_MISMATCH, format!("collations are not supported by type {shown}")),
            type_pos,
        ));
    }
    let default = match &dom.default {
        Some(e) => Some(bind_domain_default(b, e, base.id)?),
        None => None,
    };
    let mut checks: Vec<DomainConstraintSpec> = Vec::new();
    // PG DefineDomain: `nullDefined` / `typNotNull` over the NULL and NOT NULL constraints (the parser
    // encodes them as `CHECK (marker)`).
    let mut null_defined = false;
    let mut not_null: Option<Option<String>> = None;
    for constraint in &dom.constraints {
        match constraint {
            TableConstraint::Check(check)
                if matches!(&*check.expr, Expr::Identifier(id)
                    if id.value == DOMAIN_NOT_NULL_MARKER || id.value == DOMAIN_NULL_MARKER) =>
            {
                let (is_not_null, mpos) = match &*check.expr {
                    Expr::Identifier(id) => (id.value == DOMAIN_NOT_NULL_MARKER, b.pos_span(id.span)),
                    _ => (false, pos),
                };
                if is_not_null {
                    if null_defined {
                        return Err(if not_null.is_none() {
                            PgError::syntax("conflicting NULL/NOT NULL constraints", mpos)
                        } else {
                            let mut e = PgError::new(
                                SqlState::INVALID_OBJECT_DEFINITION,
                                "redundant NOT NULL constraint definition",
                            );
                            e.position = mpos;
                            e
                        });
                    }
                    not_null = Some(check.name.as_ref().map(|id| b.ident(id)));
                } else {
                    if null_defined && not_null.is_some() {
                        return Err(PgError::syntax("conflicting NULL/NOT NULL constraints", mpos));
                    }
                    not_null = None;
                }
                null_defined = true;
            }
            TableConstraint::Check(check) => {
                if check.enforced == Some(false) {
                    return Err(unsupported("NOT ENFORCED on a domain constraint", pos));
                }
                let cname = match &check.name {
                    Some(id) => Some(b.ident(id)),
                    None => None,
                };
                let expr = bind_domain_check(b, &check.expr, base.id, &name)?;
                checks.push(DomainConstraintSpec { name: cname, expr, not_valid: false });
            }
            _ => return Err(PgError::syntax("syntax error", pos)),
        }
    }
    let owner = env.current_user();
    let spec = CreateDomainSpec {
        namespace,
        name,
        owner,
        base: base.id,
        collation,
        default,
        not_null,
        checks,
        ndims: base.ndims,
    };
    Ok(BoundStatement::CreateDomain(Box::new(spec)))
}

/// 42601 `conflicting or redundant options` at the repeated role option.
fn role_conflict(item: &RoleOptionItem) -> PgError {
    conflicting_options(item.pos)
}

/// PG `CreateRole`: option loop (conflicts), then value checks, reserved name, member lists.
/// `CREATE USER` defaults to LOGIN; `CREATE GROUP` is `CREATE ROLE`.
fn create_role_spec(b: &mut Binder, kind: RoleStmtKind, name: &Ident, options: &[RoleOptionItem]) -> Result<CreateRoleSpec> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut spec = CreateRoleSpec::new(b.ident(name));
    if kind == RoleStmtKind::User {
        spec.can_login = true;
    }
    let mut conn_limit: Option<i32> = None;
    let mut password: Option<Option<String>> = None;
    let mut valid_until: Option<(&str, Option<u32>)> = None;
    let mut in_roles: &[Ident] = &[];
    let mut members: &[Ident] = &[];
    let mut admins: &[Ident] = &[];
    for item in options {
        let key = match &item.option {
            RoleOptionAst::Password(_) => "password",
            RoleOptionAst::Superuser(_) => "superuser",
            RoleOptionAst::Inherit(_) => "inherit",
            RoleOptionAst::CreateRole(_) => "createrole",
            RoleOptionAst::CreateDb(_) => "createdb",
            RoleOptionAst::Login(_) => "canlogin",
            RoleOptionAst::Replication(_) => "isreplication",
            RoleOptionAst::BypassRls(_) => "bypassrls",
            RoleOptionAst::ConnectionLimit(_) => "connectionlimit",
            RoleOptionAst::ValidUntil(_) => "validUntil",
            RoleOptionAst::RoleMembers(_) => "rolemembers",
            RoleOptionAst::AddRoleTo(_) => "addroleto",
            RoleOptionAst::AdminMembers(_) => "adminmembers",
            RoleOptionAst::Sysid => {
                // PG only warns, once per SYSID, and does not treat repeats as conflicts.
                b.notice(PgError::notice("SYSID can no longer be specified"));
                continue;
            }
        };
        if seen.contains(&key) {
            return Err(role_conflict(item));
        }
        seen.push(key);
        match &item.option {
            RoleOptionAst::Password(p) => password = Some(p.clone()),
            RoleOptionAst::Superuser(v) => spec.superuser = *v,
            RoleOptionAst::Inherit(v) => spec.inherit = *v,
            RoleOptionAst::CreateRole(v) => spec.create_role = *v,
            RoleOptionAst::CreateDb(v) => spec.create_db = *v,
            RoleOptionAst::Login(v) => spec.can_login = *v,
            RoleOptionAst::Replication(v) => spec.replication = *v,
            RoleOptionAst::BypassRls(v) => spec.bypass_rls = *v,
            RoleOptionAst::ConnectionLimit(v) => conn_limit = Some(*v),
            RoleOptionAst::ValidUntil(t) => valid_until = Some((t.as_str(), item.pos)),
            RoleOptionAst::RoleMembers(l) => members = l,
            RoleOptionAst::AddRoleTo(l) => in_roles = l,
            RoleOptionAst::AdminMembers(l) => admins = l,
            RoleOptionAst::Sysid => {}
        }
    }
    if let Some(limit) = conn_limit {
        if limit < -1 {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid connection limit: {limit}")));
        }
        spec.conn_limit = limit;
    }
    if spec.name.starts_with("pg_") {
        return Err(errors::reserved_role_name(&spec.name));
    }
    if let Some((text, _tpos)) = valid_until {
        // PG evaluates the VALID UNTIL string without a statement position.
        let ctx = b.io_ctx();
        let datum = pgarbor_types::io::input(&ctx, TypeId::of(oids::TIMESTAMPTZ), text)?;
        match datum {
            Datum::TimestampTz(us) => spec.valid_until = Some(us),
            _ => return Err(PgError::internal("binder: timestamptz input produced another datum")),
        }
    }
    if let Some(p) = password {
        spec.password = match p {
            Some(s) if s.is_empty() => {
                b.notice(PgError::notice("empty string is not a valid password, clearing password"));
                None
            }
            other => other,
        };
    }
    for id in in_roles {
        spec.in_roles.push(role_oid(b, id)?);
    }
    for id in members {
        spec.members.push(role_oid(b, id)?);
    }
    for id in admins {
        spec.admins.push(role_oid(b, id)?);
    }
    Ok(spec)
}

/// `ExtraStatement::CreateRole` (`CREATE ROLE|USER|GROUP`).
pub fn bind_create_role(b: &mut Binder, kind: RoleStmtKind, name: &Ident, options: &[RoleOptionItem]) -> Result<BoundStatement> {
    let spec = create_role_spec(b, kind, name, options)?;
    Ok(BoundStatement::CreateRole(Box::new(spec)))
}

/// `Statement::CreateDatabase`.
pub fn bind_create_database(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::CreateDatabase { db_name, if_not_exists, or_replace, transient, location, managed_location, clone, .. } =
        stmt
    else {
        return Err(PgError::internal("binder: bind_create_database called for another statement"));
    };
    let pos = obj_pos(b, db_name);
    if *or_replace {
        return Err(diag::syntax("syntax error", pos));
    }
    if *transient || location.is_some() || managed_location.is_some() || clone.is_some() {
        return Err(unsupported("CREATE DATABASE with TRANSIENT, LOCATION or CLONE", pos));
    }
    let name = single_name(b, db_name)?;
    let owner = b.env.current_user();
    Ok(BoundStatement::CreateDatabase(Box::new(BoundCreateDatabase {
        name,
        owner,
        template: None,
        encoding: None,
        locale: None,
        connection_limit: -1,
        is_template: false,
        allow_connections: true,
        if_not_exists: *if_not_exists,
    })))
}

#[cfg(test)]
mod tests {
    use super::super::context::{BindContext, BindSession};
    use super::*;
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use crate::extra_stmt::ExtraStatement;
    use crate::parser::{parse, Parsed};

    /// Parse one statement with PGArbor's own parser (the one the server uses).
    fn parse_one(sql: &str) -> Parsed {
        let mut stmts = parse(sql).expect("test SQL parses");
        stmts.remove(0).parsed
    }

    /// The option list of a `CREATE SEQUENCE`, as the numeric binder sees it (`AS` / `OWNED BY` left out).
    fn sequence_options(sql: &str) -> Vec<AstSequenceOption> {
        let Parsed::Extra(ExtraStatement::CreateSequence { options, .. }) = parse_one(sql) else {
            panic!("not a CREATE SEQUENCE: {sql}");
        };
        options
            .iter()
            .filter_map(|o| match o {
                SequenceOptionAst::IncrementBy(t) => Some(AstSequenceOption::IncrementBy(number_expr(t), true)),
                SequenceOptionAst::MinValue(v) => Some(AstSequenceOption::MinValue(v.as_deref().map(number_expr))),
                SequenceOptionAst::MaxValue(v) => Some(AstSequenceOption::MaxValue(v.as_deref().map(number_expr))),
                SequenceOptionAst::StartWith(t) => Some(AstSequenceOption::StartWith(number_expr(t), true)),
                SequenceOptionAst::Cache(t) => Some(AstSequenceOption::Cache(number_expr(t))),
                SequenceOptionAst::Cycle(c) => Some(AstSequenceOption::Cycle(*c)),
                _ => None,
            })
            .collect()
    }

    /// Run `f` with a binder over the bootstrapped catalog (search path pg_catalog, public).
    fn with_binder<T>(f: impl FnOnce(&mut Binder) -> T) -> T {
        with_binder_on("", f)
    }

    /// [`with_binder`] over the source text `sql`, so error positions resolve against it.
    fn with_binder_on<T>(sql: &str, f: impl FnOnce(&mut Binder) -> T) -> T {
        let (_kv, env) = testing::bootstrapped().expect("bootstrap");
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        f(&mut b)
    }

    /// Bind one DDL statement of the families this file owns.
    fn bind_sql(sql: &str) -> Result<BoundStatement> {
        let parsed = parse_one(sql);
        with_binder_on(sql, |b| match &parsed {
            Parsed::Extra(ExtraStatement::CreateSequence { temporary, unlogged, if_not_exists, name, options }) => {
                bind_create_sequence(b, *temporary, *unlogged, *if_not_exists, name, options)
            }
            Parsed::Extra(ExtraStatement::CreateRole { kind, name, options }) => bind_create_role(b, *kind, name, options),
            Parsed::Extra(other) => panic!("unexpected statement in test: {other:?}"),
            Parsed::Ast(stmt) => match &**stmt {
                Statement::CreateSchema { .. } => bind_create_schema(b, stmt),
                Statement::CreateType { .. } => bind_create_type(b, stmt),
                Statement::CreateDomain(_) => bind_create_domain(b, stmt),
                Statement::CreateDatabase { .. } => bind_create_database(b, stmt),
                other => panic!("unexpected statement in test: {other}"),
            },
        })
    }

    fn err_of(r: Result<BoundStatement>) -> PgError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn options_error(sql: &str, data_type: Oid) -> PgError {
        let opts = sequence_options(sql);
        match with_binder(|b| bind_sequence_options(b, &opts, data_type, None)) {
            Err(e) => e,
            Ok(v) => panic!("expected an error, got {v:?}"),
        }
    }

    #[test]
    fn integer_text_range_and_syntax() {
        assert_eq!(int_from_text(false, "9223372036854775807", None).unwrap(), i64::MAX);
        assert_eq!(int_from_text(true, "9223372036854775808", None).unwrap(), i64::MIN);
        let e = int_from_text(false, "99999999999999999999", None).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "value \"99999999999999999999\" is out of range for type bigint");
        let e = int_from_text(false, "2.5", None).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.message, "invalid input syntax for type bigint: \"2.5\"");
    }

    #[test]
    fn sequence_defaults_fill_every_clause() {
        let opts = sequence_options("create sequence s increment 2 start 10 maxvalue 100");
        let r = with_binder(|b| bind_sequence_options(b, &opts, oids::INT8, None)).unwrap();
        assert_eq!(r.type_oid, Some(oids::INT8));
        assert_eq!(r.increment, Some(2));
        assert_eq!(r.minvalue, Some(SeqBound::Value(1)));
        assert_eq!(r.maxvalue, Some(SeqBound::Value(100)));
        assert_eq!(r.start, Some(10));
        assert_eq!(r.cache, Some(1));
        assert_eq!(r.cycle, Some(false));
    }

    #[test]
    fn descending_sequence_defaults() {
        let opts = sequence_options("create sequence s increment -1");
        let r = with_binder(|b| bind_sequence_options(b, &opts, oids::INT8, None)).unwrap();
        assert_eq!(r.increment, Some(-1));
        assert_eq!(r.maxvalue, Some(SeqBound::Value(-1)));
        assert_eq!(r.minvalue, Some(SeqBound::Value(i64::MIN)));
        assert_eq!(r.start, Some(-1));
    }

    #[test]
    fn sequence_range_errors_match_pg() {
        let e = options_error("create sequence s minvalue 5 maxvalue 3", oids::INT8);
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "MINVALUE (5) must be less than MAXVALUE (3)");

        let e = options_error("create sequence s increment 0", oids::INT8);
        assert_eq!(e.message, "INCREMENT must not be zero");

        let e = options_error("create sequence s maxvalue 100000", oids::INT2);
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "MAXVALUE (100000) is out of range for sequence data type smallint");

        let e = options_error("create sequence s start 1 minvalue 5", oids::INT8);
        assert_eq!(e.message, "START value (1) cannot be less than MINVALUE (5)");

        let e = options_error("create sequence s cache 0", oids::INT8);
        assert_eq!(e.message, "CACHE (0) must be greater than zero");

        let e = options_error("create sequence s increment 2 increment 3", oids::INT8);
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "conflicting or redundant options");

        let e = options_error("create sequence s increment 99999999999999999999", oids::INT8);
        assert_eq!(e.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    }

    #[test]
    fn create_sequence_error_positions() {
        // errorConflictingDefElem points at the repeated option; range errors carry no position.
        let e = err_of(bind_sql("create sequence sx increment 2 increment 3"));
        assert_eq!(e.message, "conflicting or redundant options");
        assert_eq!(e.position, Some(32));
        let e = err_of(bind_sql("create sequence sx minvalue 10 start 5"));
        assert_eq!(e.message, "START value (5) cannot be less than MINVALUE (10)");
        assert_eq!(e.position, None);
    }

    #[test]
    fn create_sequence_statement() {
        match bind_sql("create sequence s increment 2 start 10 maxvalue 100").unwrap() {
            BoundStatement::CreateSequence(spec) => {
                assert_eq!(spec.name, "s");
                assert_eq!(spec.owned_by, None);
                assert_eq!(spec.options.increment, Some(2));
            }
            _ => panic!("expected CreateSequence"),
        }
    }

    #[test]
    fn schema_reserved_prefix() {
        let e = err_of(bind_sql("create schema pg_x"));
        assert_eq!(e.sqlstate, SqlState::RESERVED_NAME);
        assert_eq!(e.message, "unacceptable schema name \"pg_x\"");
        assert_eq!(e.detail.as_deref(), Some("The prefix \"pg_\" is reserved for system schemas."));
    }

    #[test]
    fn enum_labels() {
        match bind_sql("create type mood as enum ('a','b')").unwrap() {
            BoundStatement::CreateEnum(spec) => {
                assert_eq!(spec.name, "mood");
                assert_eq!(spec.labels, vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected CreateEnum"),
        }
        let long = "x".repeat(64);
        let e = err_of(bind_sql(&format!("create type mood as enum ('{long}')")));
        assert_eq!(e.sqlstate, SqlState::INVALID_NAME);
        assert_eq!(e.detail.as_deref(), Some("Labels must be 63 bytes or less."));
        let e = err_of(bind_sql("create type mood as enum ('a','a')"));
        assert_eq!(e.sqlstate, SqlState::UNIQUE_VIOLATION);
    }

    #[test]
    fn role_options() {
        match bind_sql("create role r login password 'x' createdb").unwrap() {
            BoundStatement::CreateRole(spec) => {
                assert_eq!(spec.name, "r");
                assert!(spec.can_login);
                assert!(spec.create_db);
                assert_eq!(spec.password.as_deref(), Some("x"));
                assert!(spec.inherit);
            }
            _ => panic!("expected CreateRole"),
        }
        match bind_sql("create role u login password null").unwrap() {
            BoundStatement::CreateRole(spec) => {
                assert_eq!(spec.name, "u");
                assert!(spec.can_login);
                assert_eq!(spec.password, None);
            }
            _ => panic!("expected CreateRole"),
        }
        let e = err_of(bind_sql("create role pg_x"));
        assert_eq!(e.sqlstate, SqlState::RESERVED_NAME);
        assert_eq!(e.message, "role name \"pg_x\" is reserved");
        let e = err_of(bind_sql("create role r connection limit -2"));
        assert_eq!(e.message, "invalid connection limit: -2");
    }

    #[test]
    fn database_defaults() {
        match bind_sql("create database d").unwrap() {
            BoundStatement::CreateDatabase(db) => {
                assert_eq!(db.name, "d");
                assert_eq!(db.connection_limit, -1);
                assert!(db.allow_connections);
                assert!(!db.is_template);
            }
            _ => panic!("expected CreateDatabase"),
        }
    }

    #[test]
    #[ignore = "needs SQL-07, SQL-10, SQL-11, SQL-29"]
    fn domain_check_text() {
        match bind_sql("create domain d as int check (value > 0)").unwrap() {
            BoundStatement::CreateDomain(spec) => {
                assert_eq!(spec.checks.len(), 1);
                assert_eq!(spec.checks[0].expr.sql, "(VALUE > 0)");
            }
            _ => panic!("expected CreateDomain"),
        }
    }

    #[test]
    fn domain_pseudo_base_type_is_42804_at_type_name() {
        let e = err_of(bind_sql("create domain dpseudo as record"));
        assert_eq!(e.sqlstate, SqlState::DATATYPE_MISMATCH);
        assert_eq!(e.message, "\"record\" is not a valid base type for a domain");
        assert_eq!(e.position, Some(26));
    }

    #[test]
    #[ignore = "needs SQL-07"]
    fn composite_type_duplicate_attribute() {
        let e = err_of(bind_sql("create type c as (a int, a text)"));
        assert_eq!(e.sqlstate, SqlState::DUPLICATE_COLUMN);
        assert_eq!(e.message, "column name \"a\" specified more than once");
    }

    #[test]
    fn sequence_options_in_any_order_bind_alike() {
        let spec = |sql: &str| match bind_sql(sql).unwrap() {
            BoundStatement::CreateSequence(spec) => spec.options,
            _ => panic!("expected CreateSequence"),
        };
        let a = spec("create sequence s maxvalue 100 as smallint minvalue 3 cache 4 no cycle increment by -1 start with 50");
        let b = spec("create sequence s increment -1 start 50 cache 4 minvalue 3 maxvalue 100 as smallint no cycle");
        assert_eq!(a, b);
        assert_eq!(a.type_oid, Some(oids::INT2));
        assert_eq!(a.increment, Some(-1));
        assert_eq!(a.start, Some(50));
        assert_eq!(a.minvalue, Some(SeqBound::Value(3)));
        assert_eq!(a.maxvalue, Some(SeqBound::Value(100)));
        assert_eq!(a.cache, Some(4));
        // `RESTART` is accepted and ignored for a new sequence; signs may be explicit.
        let c = spec("create sequence s restart increment +3");
        assert_eq!(c.increment, Some(3));
        let d = spec("create sequence s increment - 3");
        assert_eq!(d.increment, Some(-3));
    }

    #[test]
    fn sequence_negative_and_signed_values() {
        let e = options_error("create sequence s increment 0", oids::INT8);
        assert_eq!(e.message, "INCREMENT must not be zero");
        let opts = sequence_options("create sequence s minvalue -10 maxvalue -2 start -5 increment -1");
        let r = with_binder(|b| bind_sequence_options(b, &opts, oids::INT8, None)).unwrap();
        assert_eq!(r.minvalue, Some(SeqBound::Value(-10)));
        assert_eq!(r.maxvalue, Some(SeqBound::Value(-2)));
        assert_eq!(r.start, Some(-5));
        // A signed number kept as text (the ALTER SEQUENCE path) binds the same way.
        let opts = vec![AstSequenceOption::IncrementBy(number_expr("-4"), true)];
        let r = with_binder(|b| bind_sequence_options(b, &opts, oids::INT8, None)).unwrap();
        assert_eq!(r.increment, Some(-4));
    }

    #[test]
    fn sequence_repeated_and_rejected_options() {
        for sql in [
            "create sequence s increment 1 increment by 2",
            "create sequence s as int as bigint",
            "create sequence s cycle no cycle",
            "create sequence s owned by none owned by none",
            "create sequence s as int increment 0 as int",
        ] {
            let e = err_of(bind_sql(sql));
            assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR, "{sql}");
            assert_eq!(e.message, "conflicting or redundant options", "{sql}");
        }
        let e = err_of(bind_sql("create sequence s as text"));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "sequence type must be smallint, integer, or bigint");
        let e = err_of(bind_sql("create sequence s sequence name x"));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "invalid sequence option SEQUENCE NAME");
        let e = err_of(bind_sql("create sequence s logged"));
        assert_eq!(e.message, "option \"logged\" not recognized");
        let e = err_of(bind_sql("create sequence s increment 1.5"));
        assert_eq!(e.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.message, "invalid input syntax for type bigint: \"1.5\"");
    }

    #[test]
    fn role_options_in_any_order_with_signed_limit() {
        let spec = |sql: &str| match bind_sql(sql).unwrap() {
            BoundStatement::CreateRole(spec) => spec,
            _ => panic!("expected CreateRole"),
        };
        let a = spec("create role r connection limit -1 createdb nologin noinherit password 'x'");
        let b = spec("create role r with password 'x' noinherit nologin createdb connection limit -1");
        assert_eq!(a, b);
        assert_eq!(a.conn_limit, -1);
        assert!(a.create_db && !a.can_login && !a.inherit);
        assert_eq!(spec("create role r connection limit +5").conn_limit, 5);
        // CREATE USER implies LOGIN, an explicit NOLOGIN wins; CREATE GROUP is a plain role.
        assert!(spec("create user u").can_login);
        assert!(!spec("create user u nologin").can_login);
        assert!(!spec("create group g").can_login);
        let c = spec("create role r valid until '2030-01-01 00:00:00+00' superuser");
        assert!(c.superuser);
        assert!(c.valid_until.is_some());
    }

    #[test]
    fn role_option_errors_match_pg() {
        let e = err_of(bind_sql("create role r connection limit -1 connection limit 3"));
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "conflicting or redundant options");
        for sql in ["create role r login nologin", "create role r superuser nosuperuser", "create role r noinherit inherit"] {
            assert_eq!(err_of(bind_sql(sql)).message, "conflicting or redundant options", "{sql}");
        }
        let e = err_of(bind_sql("create role r connection limit - 5"));
        assert_eq!(e.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "invalid connection limit: -5");
        // A number that does not fit `int` is a syntax error at the number, not a range error.
        let e = crate::parser::parse("create role r connection limit 2147483648").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "syntax error at or near \"2147483648\"");
        let e = crate::parser::parse("create role r foo").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        assert_eq!(e.message, "unrecognized role option \"foo\"");
        let e = crate::parser::parse("create role r unencrypted password 'x'").unwrap_err();
        assert_eq!(e.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "UNENCRYPTED PASSWORD is no longer supported");
    }
}
