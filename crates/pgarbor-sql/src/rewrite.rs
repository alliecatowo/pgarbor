//! The catalog's `ExprRewriter`: re-deparse stored expressions after a rename (SQL-45, sonnet).
//! Implements amendment `pgarbor-catalog-04-sql-expression-rewriter`.
//!
//! PostgreSQL stores parsed trees, so `ALTER TABLE t RENAME b TO bb` leaves `pg_get_constraintdef`
//! printing `CHECK ((bb <> ''::text))` for free. PGArbor stores canonical TEXT, so every stored
//! expression whose `depends_on` mentions the renamed object must be re-deparsed.
//!
//! [`SqlExprRewriter`] implements `pgarbor_catalog::ddl::spec::ExprRewriter`:
//! `rewrite(expr, change, env)`:
//! 1. Decide whether `expr.sql` is an EXPRESSION or a VIEW QUERY (`expr.sql` starts with `" SELECT"` /
//!    `" VALUES"` / `" WITH"`, i.e. begins with a space + keyword and ends with `;`).
//! 2. Parse the OLD text (`parser::parse_expr_text` or `parse_one`).
//! 3. Rename-aware re-binding: bind the parsed text against a catalog view in which the change is
//!    ALREADY applied (`env` is the snapshot AFTER the rename, per the trait docs) — except that the old
//!    text still contains the OLD names, so first rewrite the AST: for `NameChange::Column{table, attnum,
//!    old, new}` replace column identifiers `old` that resolve to that column; for
//!    `Relation{oid, old, new}` replace relation names in `FROM` and in `regclass` string literals
//!    (`'old_seq'::regclass` -> `'new_seq'::regclass`, schema-qualified as the deparser would print);
//!    for `RelationSchema` / `Schema` re-qualify; for `Type`/`Function` replace names in casts / calls.
//!    The robust way: bind the OLD text with name resolution done through OIDs recorded in
//!    `depends_on` (rebind with a binder whose name lookup maps old names to the renamed objects), then
//!    deparse the bound tree with the NEW names (the deparser reads names from the catalog snapshot, so
//!    it prints new names automatically). Implement it that way: `binder::stored::bind_stored_expr_with_
//!    aliases` semantics = bind with an environment overlay `{old name -> oid}`.
//! 4. `deparse_expr` / `deparse_viewdef` -> new `sql`; `bound` stays `None`; `depends_on` unchanged
//!    (names are not part of dependencies).
//! Oracle fixtures: after `ALTER TABLE t RENAME b TO bb`: `pg_get_constraintdef` -> `CHECK ((bb <> ''::text))`;
//! view `SELECT a, b AS b FROM t` -> ` SELECT a,\n    bb AS b\n   FROM t;` (output alias keeps the OLD
//! view column name `b` — the view's own column is not renamed); after `ALTER SEQUENCE s RENAME TO s2`:
//! default `nextval('s2'::regclass)`.
//!
//! ## How this implementation realises the design
//! * **No-op fast path.** The object's name (the old name, or the relation's current name for
//!   `RelationSchema`) must occur in the text, raw or with `"` doubled. Otherwise the text is returned
//!   unchanged (acceptance: an expression not mentioning the object is returned unchanged).
//! * **Name rewriting is token-level.** The `sqlparser` visitor API is not enabled in this workspace
//!   (`sqlparser` is built without the `visitor` feature), so the rewrite works on the tokenizer's output
//!   and splices the ORIGINAL source bytes back together, replacing only the tokens that change. Every
//!   other byte, including string literal escapes, is preserved.
//! * **Expressions** (CHECK, DEFAULT, index and generation expressions): `Column` changes are applied
//!   through an overlay. The binder scope is built from the post-change columns, but the changed column
//!   is named with its OLD name, so the old text binds to the same attnum. The deparser then prints the
//!   NEW name. Relation, schema, type and function names are rewritten at the token level, and
//!   `regclass` string literals are re-qualified. Owner-table detection is described on [`owner_table`].
//! * **View queries** are rewritten at the token level with a scan of the FROM clause (table items,
//!   aliases, subqueries, parenthesised join groups). The rewritten text is re-parsed and bound with
//!   `binder::select::bind_subselect`. The view's column names come from the OLD text's projection list
//!   (explicit alias, or the column/function name), so `bb AS b` keeps the old name.
//! * **Known limits** are reported as `PgError::unimplemented("SQL-45", ..)` when they can change the
//!   meaning of a stored expression, for example an unqualified column that could belong to several FROM
//!   items when a subquery is present. Unqualified column names that provably belong to something else
//!   are left unchanged.
use pgarbor_catalog::ddl::spec::{ExprRewriter, NameChange};
use pgarbor_catalog::{CatalogEnv, ObjClass, ObjectAddress, StoredExpr};
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::context::IoConfig;
use pgarbor_types::text::format::quote_identifier;
use pgarbor_types::TypeId;
use sqlparser::ast::{Expr, ObjectNamePart, SelectItem, SetExpr, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use crate::binder::context::{BindContext, BindSession, Binder, ExprCtx};
use crate::binder::expr::bind_expr;
use crate::binder::select::bind_subselect;
use crate::binder::stored::table_expr_scope;
use crate::deparse::expr::deparse_expr;
use crate::deparse::query::deparse_viewdef;
use crate::deparse::{DeparseCtx, DeparseOptions, TableColumns};
use crate::parser::{parse_expr_text, parse_one, Parsed};
use crate::source::SourceMap;

/// The rewriter handed to `CatalogTxn::rename_*` / `set_*_schema`.
// CONTRACT
#[derive(Clone, Copy, Debug, Default)]
pub struct SqlExprRewriter;

impl ExprRewriter for SqlExprRewriter {
    fn rewrite(&self, expr: &StoredExpr, change: &NameChange, env: &CatalogEnv) -> Result<StoredExpr> {
        rewrite_stored_expr(expr, change, env)
    }
}

/// Worker body (module docs). Kept as a free function so tests can call it directly.
pub fn rewrite_stored_expr(expr: &StoredExpr, change: &NameChange, env: &CatalogEnv) -> Result<StoredExpr> {
    let needle = needle_of(change, env)?;
    if !mentions(&expr.sql, &needle) {
        return Ok(expr.clone());
    }
    let sql = if is_view_query(&expr.sql) {
        rewrite_view(&expr.sql, change, env)?
    } else {
        rewrite_expression(&expr.sql, change, env, &expr.depends_on)?
    };
    Ok(StoredExpr { sql, bound: None, depends_on: expr.depends_on.clone() })
}

// ---------------------------------------------------------------------------------------------
// Entry helpers
// ---------------------------------------------------------------------------------------------

/// The identifier text that must occur in an affected expression.
fn needle_of(change: &NameChange, env: &CatalogEnv) -> Result<String> {
    Ok(match change {
        NameChange::Column { old, .. }
        | NameChange::Relation { old, .. }
        | NameChange::Schema { old, .. }
        | NameChange::Type { old, .. }
        | NameChange::Function { old, .. } => old.clone(),
        NameChange::RelationSchema { oid, .. } => relation_name(env, *oid)?,
    })
}

/// True when `text` can contain `needle` as an identifier (raw or with `"` doubled).
fn mentions(text: &str, needle: &str) -> bool {
    text.contains(needle) || text.contains(&needle.replace('"', "\"\""))
}

/// A view query text starts with `SELECT`, `VALUES` or `WITH` (after the leading space of
/// `pg_get_viewdef`). Expression text never starts with those keywords.
fn is_view_query(sql: &str) -> bool {
    let head = sql.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
    QUERY_START.iter().any(|k| head.eq_ignore_ascii_case(k))
}

const QUERY_START: &[&str] = &["SELECT", "VALUES", "WITH"];
const CLAUSE_END: &[&str] = &[
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "WINDOW",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "RETURNING",
];
/// Keywords that can follow a relation name in a FROM item and therefore are not an alias.
const NOT_ALIAS: &[&str] = &[
    "ON",
    "USING",
    "WHERE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "FULL",
    "INNER",
    "CROSS",
    "NATURAL",
    "ORDER",
    "GROUP",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "WINDOW",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "FOR",
    "TABLESAMPLE",
    "LATERAL",
    "FROM",
    "SELECT",
    "VALUES",
    "WITH",
    "AS",
];

// ---------------------------------------------------------------------------------------------
// Catalog lookups (all failures are internal: the snapshot must contain the renamed objects)
// ---------------------------------------------------------------------------------------------

fn relation_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    env.snapshot()
        .relation(oid)
        .map(|r| r.name.clone())
        .ok_or_else(|| PgError::internal(format!("relation {oid} not found while rewriting a stored expression")))
}

fn relation_namespace(env: &CatalogEnv, oid: Oid) -> Result<Oid> {
    env.snapshot()
        .relation(oid)
        .map(|r| r.namespace)
        .ok_or_else(|| PgError::internal(format!("relation {oid} not found while rewriting a stored expression")))
}

fn namespace_name(env: &CatalogEnv, oid: Oid) -> Result<String> {
    env.snapshot()
        .namespace(oid)
        .map(|n| n.name.clone())
        .ok_or_else(|| PgError::internal(format!("schema {oid} not found while rewriting a stored expression")))
}

fn schema_name_of_relation(env: &CatalogEnv, oid: Oid) -> Result<String> {
    namespace_name(env, relation_namespace(env, oid)?)
}

/// `qualifier` absent, or equal to `expected`.
fn qualifier_is(qualifier: Option<&str>, expected: &str) -> bool {
    match qualifier {
        None => true,
        Some(q) => q == expected,
    }
}

/// `"schema"."name"` with each part quoted only when needed.
fn qualified(schema: &str, name: &str) -> Result<String> {
    Ok(format!("{}.{}", quote_identifier(schema)?, quote_identifier(name)?))
}

/// The table an expression is bound against. For `Column` changes the table is known. Otherwise the
/// first column dependency (`sub_id > 0`) names it, and a DEFAULT such as `nextval('s')` has none.
fn owner_table(change: &NameChange, depends_on: &[ObjectAddress]) -> Option<Oid> {
    if let NameChange::Column { table, .. } = change {
        return Some(*table);
    }
    depends_on.iter().find(|a| matches!(a.class, ObjClass::Relation) && a.sub_id > 0).map(|a| a.oid)
}

// ---------------------------------------------------------------------------------------------
// Lexing: tokens with byte offsets, so unchanged text is copied byte-for-byte
// ---------------------------------------------------------------------------------------------

struct Lexed<'t> {
    text: &'t str,
    toks: Vec<TokenWithSpan>,
    /// Byte offset of each token in `text`.
    starts: Vec<usize>,
}

fn lex(text: &str) -> Result<Lexed<'_>> {
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, text);
    let toks = tokenizer
        .tokenize_with_location()
        .map_err(|e| PgError::internal(format!("could not parse stored expression \"{text}\": {e:?}")))?;
    let map = SourceMap::new(text);
    let mut starts = Vec::with_capacity(toks.len());
    for t in &toks {
        let off = if matches!(t.token, Token::EOF) {
            text.len()
        } else {
            map.byte_offset(t.span.start)
                .ok_or_else(|| PgError::internal(format!("could not locate a token of \"{text}\"")))?
        };
        starts.push(off);
    }
    Ok(Lexed { text, toks, starts })
}

impl<'t> Lexed<'t> {
    /// No replacements.
    fn blank(&self) -> Vec<Option<String>> {
        vec![None; self.toks.len()]
    }

    /// Significant tokens (not whitespace or comments, not EOF) as (token index, token).
    fn significant(&self) -> Vec<(usize, &Token)> {
        self.toks
            .iter()
            .enumerate()
            .filter(|(_, t)| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
            .map(|(i, t)| (i, &t.token))
            .collect()
    }

    /// Re-emit the text, replacing the tokens with a `Some` replacement.
    fn render(&self, repl: &[Option<String>]) -> Result<String> {
        let mut out = String::with_capacity(self.text.len() + 16);
        for (i, r) in repl.iter().enumerate() {
            match r {
                Some(s) => out.push_str(s),
                None => {
                    let start = self.starts.get(i).copied().unwrap_or(self.text.len());
                    let end = self.starts.get(i + 1).copied().unwrap_or(self.text.len());
                    let piece = self
                        .text
                        .get(start..end)
                        .ok_or_else(|| PgError::internal("could not rewrite a stored expression"))?;
                    out.push_str(piece);
                }
            }
        }
        Ok(out)
    }
}

type Sig<'a> = [(usize, &'a Token)];

fn at<'a>(sig: &Sig<'a>, k: usize) -> Option<&'a Token> {
    sig.get(k).map(|p| p.1)
}

fn before<'a>(sig: &Sig<'a>, j: usize) -> Option<&'a Token> {
    j.checked_sub(1).and_then(|k| at(sig, k))
}

/// (value, quoted) of a word token.
fn word_of(t: Option<&Token>) -> Option<(&str, bool)> {
    match t {
        Some(Token::Word(w)) => Some((w.value.as_str(), w.quote_style.is_some())),
        _ => None,
    }
}

/// An unquoted keyword from `list`.
fn is_kw(t: Option<&Token>, list: &[&str]) -> bool {
    match word_of(t) {
        Some((v, false)) => list.iter().any(|k| v.eq_ignore_ascii_case(k)),
        _ => false,
    }
}

/// A word equal to the identifier `name`.
fn name_is(t: Option<&Token>, name: &str) -> bool {
    matches!(word_of(t), Some((v, _)) if v == name)
}

/// `'text'` with the value's quotes doubled.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------------------------
// Common rules: regclass literals, type names, schema qualifiers, function names
// ---------------------------------------------------------------------------------------------

/// Rules that apply to expressions and view queries alike. `regclass` maps the text of a
/// `'...'::regclass` literal to its new text (`None` = unchanged).
fn apply_common_rules(
    sig: &Sig<'_>,
    change: &NameChange,
    regclass: &dyn Fn(&str) -> Result<Option<String>>,
    repl: &mut [Option<String>],
) -> Result<()> {
    for j in 0..sig.len() {
        let (idx, tok) = sig[j];
        match tok {
            Token::SingleQuotedString(s) => {
                let cast_to_regclass = matches!(at(sig, j + 1), Some(Token::DoubleColon))
                    && is_kw(at(sig, j + 2), &["regclass"]);
                if cast_to_regclass {
                    if let Some(value) = regclass(s.as_str())? {
                        repl[idx] = Some(sql_literal(&value));
                    }
                }
            }
            Token::Word(_) => match change {
                NameChange::Type { old, new, .. } => {
                    if name_is(Some(tok), old) && type_name_position(sig, j) {
                        repl[idx] = Some(quote_identifier(new)?);
                    }
                }
                NameChange::Schema { old, new, .. } => {
                    let qualifier = matches!(at(sig, j + 1), Some(Token::Period))
                        && !matches!(before(sig, j), Some(Token::Period));
                    if name_is(Some(tok), old) && qualifier {
                        repl[idx] = Some(quote_identifier(new)?);
                    }
                }
                NameChange::Function { old, new, .. } => {
                    let call = matches!(at(sig, j + 1), Some(Token::LParen))
                        && !matches!(before(sig, j), Some(Token::DoubleColon));
                    if name_is(Some(tok), old) && call {
                        repl[idx] = Some(quote_identifier(new)?);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

/// A word that is the type name after `::`, possibly schema-qualified (`::s.mood`).
fn type_name_position(sig: &Sig<'_>, j: usize) -> bool {
    match before(sig, j) {
        Some(Token::DoubleColon) => !matches!(at(sig, j + 1), Some(Token::Period)),
        Some(Token::Period) => j
            .checked_sub(3)
            .is_some_and(|k| matches!(at(sig, k), Some(Token::DoubleColon))),
        _ => false,
    }
}

/// The new text of a `'...'::regclass` literal for a relation or schema change, or `None` when the
/// literal does not name the changed object. Output is always schema-qualified: the deparser prints
/// the unqualified form again when it is visible in the search path.
fn regclass_value(s: &str, change: &NameChange, env: &CatalogEnv) -> Result<Option<String>> {
    let Some(parts) = split_qualified(s) else {
        return Ok(None);
    };
    let (qual, last) = match parts.as_slice() {
        [only] => (None, only.as_str()),
        [q, last] => (Some(q.as_str()), last.as_str()),
        _ => return Ok(None),
    };
    match change {
        NameChange::Relation { oid, old, new } => {
            let schema = schema_name_of_relation(env, *oid)?;
            if last == old.as_str() && qualifier_is(qual, &schema) {
                Ok(Some(qualified(&schema, new)?))
            } else {
                Ok(None)
            }
        }
        NameChange::RelationSchema { oid, old_schema, new_schema } => {
            let name = relation_name(env, *oid)?;
            let old_schema_name = namespace_name(env, *old_schema)?;
            if last == name && qualifier_is(qual, &old_schema_name) {
                let new_schema_name = namespace_name(env, *new_schema)?;
                Ok(Some(qualified(&new_schema_name, &name)?))
            } else {
                Ok(None)
            }
        }
        NameChange::Schema { old, new, .. } => match qual {
            Some(q) if q == old.as_str() => Ok(Some(qualified(new, last)?)),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Split `public."My Seq"` into unquoted parts (unquoted parts fold to lower case, as regclass input
/// does). `None` for malformed text or empty parts.
fn split_qualified(s: &str) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == '.' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c.to_ascii_lowercase());
        }
    }
    if in_quotes {
        return None;
    }
    parts.push(cur);
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(parts)
}

// ---------------------------------------------------------------------------------------------
// Expressions (CHECK, DEFAULT, index, generation)
// ---------------------------------------------------------------------------------------------

fn rewrite_expression(sql: &str, change: &NameChange, env: &CatalogEnv, depends_on: &[ObjectAddress]) -> Result<String> {
    let lx = lex(sql)?;
    let sig = lx.significant();
    let mut repl = lx.blank();
    let regclass = |s: &str| regclass_value(s, change, env);
    apply_common_rules(&sig, change, &regclass, &mut repl)?;
    let new_sql = lx.render(&repl)?;

    let owner = owner_table(change, depends_on);
    let (owner_name, scope, post_names) = table_scope(env, owner, change)?;
    let ast = parse_expr_text(&new_sql)?;
    let ctx = BindContext {
        env,
        session: BindSession { database: String::new() },
        io_config: IoConfig::default(),
        statement_timestamp: 0,
        declared_params: Vec::new(),
    };
    let src = SourceMap::new(&new_sql);
    let mut b = Binder::new(&ctx, &src);
    table_expr_scope(&mut b, &owner_name, &scope)?;
    let bound = bind_expr(&mut b, &ast, ExprCtx::check());
    b.scopes.pop();
    let bound = bound?;
    let namer = TableColumns { names: post_names };
    let dctx = DeparseCtx { env, catalog: env, namer: &namer, opts: DeparseOptions::default() };
    deparse_expr(&dctx, &bound)
}

/// Column scope for the owner table: (table name, (name, type) in attnum order, names for the
/// deparser). For a `Column` change the OLD name is used in the scope, so the stored text still binds
/// to the same attnum. The deparser gets the NEW names.
fn table_scope(
    env: &CatalogEnv,
    owner: Option<Oid>,
    change: &NameChange,
) -> Result<(String, Vec<(String, TypeId)>, Vec<String>)> {
    let Some(oid) = owner else {
        return Ok((String::new(), Vec::new(), Vec::new()));
    };
    let rel = env
        .snapshot()
        .relation(oid)
        .ok_or_else(|| PgError::internal(format!("relation {oid} not found while rewriting a stored expression")))?;
    let mut scope: Vec<(String, TypeId)> = rel.columns.iter().map(|c| (c.name.clone(), c.type_id.clone())).collect();
    let post_names: Vec<String> = scope.iter().map(|(n, _)| n.clone()).collect();
    if let NameChange::Column { attnum, old, .. } = change {
        let slot = usize::try_from(i32::from(*attnum) - 1).ok().and_then(|i| scope.get_mut(i));
        match slot {
            Some(s) => s.0 = old.clone(),
            None => return Err(PgError::internal(format!("column {attnum} not found while rewriting a stored expression"))),
        }
    }
    Ok((rel.name.clone(), scope, post_names))
}

// ---------------------------------------------------------------------------------------------
// View queries
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bare {
    /// Rename bare column references.
    Rename,
    /// Bare references belong to another FROM item; leave them.
    Keep,
    /// Cannot decide without a full name resolver; error when a candidate exists.
    Unknown,
}

/// One FROM item (table or function), with sig indices of its tokens.
struct FromItem {
    schema_j: Option<usize>,
    name_j: usize,
    schema: Option<String>,
    name: String,
    alias: Option<String>,
    is_rel: bool,
}

impl FromItem {
    /// The name column references use: the alias if present, else the relation name.
    fn refname(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.name.clone())
    }
}

struct FromScan {
    items: Vec<FromItem>,
    /// FROM items that are not plain relations (subqueries, function calls).
    unknown: usize,
    /// Sig indices that are table, schema or alias names (never column references).
    excluded: Vec<bool>,
}

#[derive(Clone, Copy)]
struct Frame {
    /// The parenthesis opens a query (`SELECT`/`VALUES`/`WITH`) or a join group.
    query: bool,
    /// Inside a FROM clause at this level.
    from_active: bool,
    /// The next token starts a FROM item.
    expect_item: bool,
}

/// Scan every FROM clause of the query, nested ones included (canonical view text gives them
/// unique reference names).
fn scan_from(sig: &Sig<'_>) -> FromScan {
    let mut scan = FromScan { items: Vec::new(), unknown: 0, excluded: vec![false; sig.len()] };
    let mut stack = vec![Frame { query: true, from_active: false, expect_item: false }];
    let mut j = 0usize;
    while j < sig.len() {
        let top = stack.len() - 1;
        let tok = sig[j].1;
        let mut next = j + 1;
        match tok {
            Token::LParen => {
                let query = is_kw(at(sig, j + 1), QUERY_START);
                if stack[top].expect_item {
                    stack[top].expect_item = false;
                    if query {
                        scan.unknown += 1;
                        stack.push(Frame { query: true, from_active: false, expect_item: false });
                    } else {
                        stack.push(Frame { query: true, from_active: true, expect_item: true });
                    }
                } else {
                    stack.push(Frame { query, from_active: false, expect_item: false });
                }
            }
            Token::RParen => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            Token::Comma => {
                if stack[top].from_active {
                    stack[top].expect_item = true;
                }
            }
            Token::Word(w) => {
                let up = if w.quote_style.is_none() { w.value.to_ascii_uppercase() } else { String::new() };
                match up.as_str() {
                    "FROM" => {
                        if stack[top].query {
                            stack[top].from_active = true;
                            stack[top].expect_item = true;
                        }
                    }
                    "JOIN" => {
                        if stack[top].from_active {
                            stack[top].expect_item = true;
                        }
                    }
                    "ON" | "USING" => stack[top].expect_item = false,
                    _ if CLAUSE_END.contains(&up.as_str()) => {
                        stack[top].from_active = false;
                        stack[top].expect_item = false;
                    }
                    _ if stack[top].expect_item && !is_kw(Some(tok), &["LATERAL", "ONLY"]) => {
                        let (schema_j, name_j) =
                            if matches!(at(sig, j + 1), Some(Token::Period)) && word_of(at(sig, j + 2)).is_some() {
                                (Some(j), j + 2)
                            } else {
                                (None, j)
                            };
                        let schema = schema_j.and_then(|sj| word_of(at(sig, sj)).map(|(v, _)| v.to_string()));
                        let name = word_of(at(sig, name_j)).map(|(v, _)| v.to_string()).unwrap_or_default();
                        let mut k = name_j + 1;
                        let is_func = matches!(at(sig, k), Some(Token::LParen));
                        let mut alias: Option<String> = None;
                        let mut alias_j: Option<usize> = None;
                        if !is_func {
                            let explicit = is_kw(at(sig, k), &["AS"]);
                            if explicit {
                                k += 1;
                            }
                            if let Some((v, _)) = word_of(at(sig, k)) {
                                if explicit || !is_kw(at(sig, k), NOT_ALIAS) {
                                    alias = Some(v.to_string());
                                    alias_j = Some(k);
                                    k += 1;
                                }
                            }
                        }
                        if let Some(sj) = schema_j {
                            scan.excluded[sj] = true;
                        }
                        scan.excluded[name_j] = true;
                        if let Some(aj) = alias_j {
                            scan.excluded[aj] = true;
                        }
                        if is_func {
                            scan.unknown += 1;
                        } else {
                            scan.items.push(FromItem { schema_j, name_j, schema, name, alias, is_rel: true });
                        }
                        stack[top].expect_item = false;
                        next = k;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        j = next;
    }
    scan
}

/// A bare column reference position: not qualified, not a cast target, not an output alias, not a
/// call.
fn is_column_position(sig: &Sig<'_>, j: usize) -> bool {
    !matches!(before(sig, j), Some(Token::Period) | Some(Token::DoubleColon))
        && !is_kw(before(sig, j), &["AS"])
        && !matches!(at(sig, j + 1), Some(Token::Period) | Some(Token::LParen))
}

/// Dispatch the view-only rules by change kind.
fn apply_view_rules(sig: &Sig<'_>, scan: &FromScan, change: &NameChange, env: &CatalogEnv, repl: &mut [Option<String>]) -> Result<()> {
    match change {
        NameChange::Column { table, old, new, .. } => column_view_rules(sig, scan, *table, old, new, env, repl),
        NameChange::Relation { oid, old, new } => relation_view_rules(sig, scan, *oid, old, new, env, repl),
        NameChange::RelationSchema { oid, old_schema, new_schema } => {
            relation_schema_view_rules(sig, scan, *oid, *old_schema, *new_schema, env, repl)
        }
        NameChange::Schema { .. } | NameChange::Type { .. } | NameChange::Function { .. } => Ok(()),
    }
}

/// Renames of column `old` (of `table`) inside a view query.
fn column_view_rules(
    sig: &Sig<'_>,
    scan: &FromScan,
    table: Oid,
    old: &str,
    new: &str,
    env: &CatalogEnv,
    repl: &mut [Option<String>],
) -> Result<()> {
    let relname = relation_name(env, table)?;
    let ns_name = schema_name_of_relation(env, table)?;
    let changed: Vec<usize> = scan
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.is_rel && it.name == relname && qualifier_is(it.schema.as_deref(), &ns_name))
        .map(|(k, _)| k)
        .collect();
    let refnames: Vec<String> = changed.iter().map(|&k| scan.items[k].refname()).collect();
    let mode = bare_mode(scan, &changed, old, env);
    for j in 0..sig.len() {
        if scan.excluded[j] || !name_is(Some(sig[j].1), old) {
            continue;
        }
        if matches!(before(sig, j), Some(Token::Period)) {
            // Qualified reference: `ref.old`.
            let qualifier = j.checked_sub(2).and_then(|k| word_of(at(sig, k))).map(|(v, _)| v);
            if let Some(q) = qualifier {
                if refnames.iter().any(|r| r.as_str() == q) {
                    repl[sig[j].0] = Some(quote_identifier(new)?);
                }
            }
            continue;
        }
        if is_column_position(sig, j) {
            match mode {
                Bare::Rename => repl[sig[j].0] = Some(quote_identifier(new)?),
                Bare::Keep => {}
                Bare::Unknown => {
                    return Err(PgError::unimplemented(
                        "SQL-45",
                        format!("renaming unqualified column \"{old}\" in a view whose FROM clause has subqueries or several relations"),
                    ))
                }
            }
        }
    }
    Ok(())
}

/// Whether an unqualified column named `old` belongs to the changed table.
fn bare_mode(scan: &FromScan, changed: &[usize], old: &str, env: &CatalogEnv) -> Bare {
    if scan.unknown > 0 {
        return Bare::Unknown;
    }
    match changed {
        [] => Bare::Keep,
        [only] => {
            for (k, it) in scan.items.iter().enumerate() {
                if k == *only {
                    continue;
                }
                match env.resolve_relation(it.schema.as_deref(), &it.name) {
                    Ok(rel) => {
                        if rel.columns.iter().any(|c| !c.dropped && c.name.as_str() == old) {
                            return Bare::Unknown;
                        }
                    }
                    Err(_) => return Bare::Unknown,
                }
            }
            Bare::Rename
        }
        _ => Bare::Unknown,
    }
}

/// Renames of relation `old` (now `new`) inside a view query.
fn relation_view_rules(
    sig: &Sig<'_>,
    scan: &FromScan,
    oid: Oid,
    old: &str,
    new: &str,
    env: &CatalogEnv,
    repl: &mut [Option<String>],
) -> Result<()> {
    let ns_name = schema_name_of_relation(env, oid)?;
    let mut unaliased = false;
    for it in &scan.items {
        if it.is_rel && it.name.as_str() == old && qualifier_is(it.schema.as_deref(), &ns_name) {
            repl[sig[it.name_j].0] = Some(quote_identifier(new)?);
            if it.alias.is_none() {
                unaliased = true;
            }
        }
    }
    if unaliased {
        // Column qualifiers `old.col` now refer to the new reference name.
        for j in 0..sig.len() {
            if scan.excluded[j] || !name_is(Some(sig[j].1), old) {
                continue;
            }
            let qualifier = matches!(at(sig, j + 1), Some(Token::Period))
                && !matches!(before(sig, j), Some(Token::Period));
            if qualifier {
                repl[sig[j].0] = Some(quote_identifier(new)?);
            }
        }
    }
    Ok(())
}

/// Renames of a relation moved to another schema inside a view query.
fn relation_schema_view_rules(
    sig: &Sig<'_>,
    scan: &FromScan,
    oid: Oid,
    old_schema: Oid,
    new_schema: Oid,
    env: &CatalogEnv,
    repl: &mut [Option<String>],
) -> Result<()> {
    let relname = relation_name(env, oid)?;
    let old_name = namespace_name(env, old_schema)?;
    let new_name = namespace_name(env, new_schema)?;
    for it in &scan.items {
        if it.is_rel && it.name == relname && qualifier_is(it.schema.as_deref(), &old_name) {
            match it.schema_j {
                Some(sj) => repl[sig[sj].0] = Some(quote_identifier(&new_name)?),
                None => {
                    repl[sig[it.name_j].0] =
                        Some(format!("{}.{}", quote_identifier(&new_name)?, quote_identifier(&relname)?))
                }
            }
        }
    }
    Ok(())
}

/// Output names of the OLD view query, one per projection item (`None` when the name depends on
/// the expression and is taken from the bound query instead).
fn old_view_names(sql: &str) -> Result<Vec<Option<String>>> {
    let raw = parse_one(sql)?;
    match raw.parsed {
        Parsed::Ast(stmt) => match *stmt {
            Statement::Query(q) => Ok(projection_names(&q.body).unwrap_or_default()),
            _ => Err(PgError::internal("could not rewrite a stored view query")),
        },
        Parsed::Extra(_) => Err(PgError::internal("could not rewrite a stored view query")),
    }
}

fn projection_names(body: &SetExpr) -> Option<Vec<Option<String>>> {
    match body {
        SetExpr::Select(sel) => Some(sel.projection.iter().map(select_item_name).collect()),
        SetExpr::SetOperation { left, .. } => projection_names(left),
        SetExpr::Query(q) => projection_names(&q.body),
        _ => None,
    }
}

fn select_item_name(item: &SelectItem) -> Option<String> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
        SelectItem::UnnamedExpr(e) => expr_output_name(e),
        _ => None,
    }
}

/// PostgreSQL's `FigureColname` for the forms that take their name from the expression itself.
fn expr_output_name(e: &Expr) -> Option<String> {
    match e {
        Expr::Identifier(id) => Some(id.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|p| p.value.clone()),
        Expr::Function(f) => match f.name.0.last() {
            Some(ObjectNamePart::Identifier(id)) => Some(id.value.clone()),
            _ => None,
        },
        Expr::Cast { expr, .. } => expr_output_name(expr),
        _ => None,
    }
}

/// Rewrite a view query text (old names to new names) and re-deparse it. The OLD output names are
/// kept as the view's column names, so the deparser prints `bb AS b`.
fn rewrite_view(sql: &str, change: &NameChange, env: &CatalogEnv) -> Result<String> {
    let old_names = old_view_names(sql)?;
    let lx = lex(sql)?;
    let sig = lx.significant();
    let mut repl = lx.blank();
    let scan = scan_from(&sig);
    let regclass = |s: &str| regclass_value(s, change, env);
    apply_common_rules(&sig, change, &regclass, &mut repl)?;
    apply_view_rules(&sig, &scan, change, env, &mut repl)?;
    let new_sql = lx.render(&repl)?;
    bind_view(&new_sql, &old_names, env)
}

/// Parse, bind and deparse a rewritten view query.
fn bind_view(sql: &str, old_names: &[Option<String>], env: &CatalogEnv) -> Result<String> {
    let raw = parse_one(sql)?;
    let query = match raw.parsed {
        Parsed::Ast(stmt) => match *stmt {
            Statement::Query(q) => q,
            _ => return Err(PgError::internal("could not bind a rewritten view query")),
        },
        Parsed::Extra(_) => return Err(PgError::internal("could not bind a rewritten view query")),
    };
    let ctx = BindContext {
        env,
        session: BindSession { database: String::new() },
        io_config: IoConfig::default(),
        statement_timestamp: 0,
        declared_params: Vec::new(),
    };
    let src = SourceMap::new(sql);
    let mut b = Binder::new(&ctx, &src);
    let bq = bind_subselect(&mut b, &query, &[])?;
    let names: Vec<String> = if old_names.is_empty() {
        bq.output.iter().map(|c| c.name.clone()).collect()
    } else {
        if old_names.len() != bq.output.len() {
            return Err(PgError::internal("view column count changed while rewriting a stored view query"));
        }
        old_names
            .iter()
            .zip(bq.output.iter())
            .map(|(old, col)| old.clone().unwrap_or_else(|| col.name.clone()))
            .collect()
    };
    deparse_viewdef(env, &bq, &names, DeparseOptions::default())
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn no_regclass(_: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Apply the common rules with no regclass handling, then render.
    fn common(text: &str, change: &NameChange) -> Result<String> {
        let lx = lex(text)?;
        let sig = lx.significant();
        let mut repl = lx.blank();
        apply_common_rules(&sig, change, &no_regclass, &mut repl)?;
        lx.render(&repl)
    }

    #[test]
    fn split_qualified_handles_quotes() {
        assert_eq!(split_qualified("s2"), Some(vec!["s2".to_string()]));
        assert_eq!(split_qualified("Public.S2"), Some(vec!["public".to_string(), "s2".to_string()]));
        assert_eq!(
            split_qualified("public.\"My\"\"Seq\""),
            Some(vec!["public".to_string(), "My\"Seq".to_string()])
        );
        assert_eq!(split_qualified("\"unterminated"), None);
        assert_eq!(split_qualified("a..b"), None);
        assert_eq!(split_qualified("a."), None);
    }

    #[test]
    fn view_detection_and_mention_check() {
        assert!(is_view_query(" SELECT a,\n    bb AS b\n   FROM t;"));
        assert!(is_view_query(" VALUES (1)"));
        assert!(!is_view_query("((bb <> ''::text))"));
        assert!(!is_view_query("nextval('s2'::regclass)"));
        assert!(mentions("CHECK ((b <> ''::text))", "b"));
        assert!(!mentions("(a > 1)", "b"));
        assert!(mentions("\"Odd\"\"Name\" > 1", "Odd\"Name"));
    }

    #[test]
    fn lexer_round_trips_bytes_exactly() -> Result<()> {
        let texts = [
            " SELECT a,\n    bb AS b\n   FROM t;",
            "(('é'::text || b) <> E'\\n'::text)",
            "nextval('s2'::regclass)",
        ];
        for text in texts {
            let lx = lex(text)?;
            assert_eq!(lx.render(&lx.blank())?, text);
        }
        Ok(())
    }

    #[test]
    fn schema_rename_requalifies_function_and_type_names() -> Result<()> {
        let change = NameChange::Schema { oid: 16400, old: "s".to_string(), new: "Sx".to_string() };
        assert_eq!(common("s.f(x) + 1", &change)?, "\"Sx\".f(x) + 1");
        assert_eq!(common("'a'::s.mood", &change)?, "'a'::\"Sx\".mood");
        Ok(())
    }

    #[test]
    fn function_and_type_renames_touch_only_their_positions() -> Result<()> {
        let func = NameChange::Function { oid: 16400, old: "f".to_string(), new: "g".to_string() };
        assert_eq!(common("f(b) + lower(f)", &func)?, "g(b) + lower(f)");
        let ty = NameChange::Type { oid: 16401, old: "mood".to_string(), new: "mood2".to_string() };
        assert_eq!(common("('a'::mood)", &ty)?, "('a'::mood2)");
        assert_eq!(common("(mood = 'a')", &ty)?, "(mood = 'a')");
        Ok(())
    }

    #[test]
    fn from_scan_finds_items_aliases_and_subqueries() -> Result<()> {
        let text = " SELECT a,\n    x.n\n   FROM (scr_tmp.t\n     LEFT JOIN ( SELECT u.a AS n\n           FROM scr_tmp.u) x ON ((x.n = t.a))),\n     scr_tmp.u uu\n  WHERE (uu.d = 1);";
        let lx = lex(text)?;
        let sig = lx.significant();
        let scan = scan_from(&sig);
        assert_eq!(scan.unknown, 1);
        assert!(scan.items.iter().any(|i| i.name == "t" && i.alias.is_none()));
        assert!(scan.items.iter().any(|i| i.name == "u" && i.alias.as_deref() == Some("uu")));
        Ok(())
    }

    #[test]
    fn projection_names_follow_aliases_and_column_names() {
        let cast = Expr::Cast {
            kind: sqlparser::ast::CastKind::DoubleColon,
            expr: Box::new(Expr::Identifier(sqlparser::ast::Ident::new("b"))),
            data_type: sqlparser::ast::DataType::Text,
            format: None,
        };
        assert_eq!(expr_output_name(&cast), Some("b".to_string()));
        assert_eq!(expr_output_name(&Expr::Identifier(sqlparser::ast::Ident::new("a"))), Some("a".to_string()));
    }

    #[test]
    fn unmentioned_expression_is_unchanged() -> Result<()> {
        // The fast path returns before any parsing or catalog access for the needle check.
        let change = NameChange::Column { table: 16384, attnum: 2, old: "b".to_string(), new: "bb".to_string() };
        assert!(!mentions("(a > 1)", "b"));
        assert_eq!(common("(a > 1)", &change)?, "(a > 1)");
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-29, SQL-43 (expression binding and deparse)"]
    fn check_column_rename_fixture() -> Result<()> {
        // Acceptance: rename column b -> bb: `CHECK ((b <> ''::text))` becomes `((bb <> ''::text))`.
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let expr = StoredExpr::new("((b <> ''::text))");
        let change = NameChange::Column { table: 16384, attnum: 2, old: "b".to_string(), new: "bb".to_string() };
        let out = rewrite_stored_expr(&expr, &change, &env)?;
        assert_eq!(out.sql, "((bb <> ''::text))");
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-16, SQL-29, SQL-43, SQL-44 (view binding and deparse)"]
    fn view_column_rename_fixture() -> Result<()> {
        // Acceptance: ` SELECT a,\n    b AS b\n   FROM t;` after renaming column b -> bb keeps the alias.
        let (_kv, env) = pgarbor_catalog::testing::bootstrapped()?;
        let expr = StoredExpr::new(" SELECT a,\n    b AS b\n   FROM t;");
        let change = NameChange::Column { table: 16384, attnum: 2, old: "b".to_string(), new: "bb".to_string() };
        let out = rewrite_stored_expr(&expr, &change, &env)?;
        assert_eq!(out.sql, " SELECT a,\n    bb AS b\n   FROM t;");
        Ok(())
    }
}
