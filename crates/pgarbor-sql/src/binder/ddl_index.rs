//! CREATE INDEX (SQL-32). PG references: `indexcmds.c` (`DefineIndex`, `ComputeIndexAttrs`,
//! `ResolveOpClass`, `CheckPredicate`), `parse_utilcmd.c` (`transformIndexStmt`), `gram.y`
//! (`IndexStmt`, `index_elem`).
//!
//! `bind_create_index(b, ci)` -> `BoundStatement::CreateIndex(Box<IndexSpec>)` (sqlparser `CreateIndex {
//! name, table_name, using, columns: Vec<IndexColumn>, unique, concurrently, if_not_exists, include,
//! nulls_distinct, with, predicate, index_options, alter_options }`):
//! * Table: `env.resolve_relation` (42P01); must be a table or materialized view (42809 `"v" is not a
//!   table or materialized view`); system catalog: 42501 `permission denied: "pg_class" is a system
//!   catalog`.
//! * Name: optional; `None` -> `IndexSpec.name = None` (catalog generates `<table>_<cols>_idx`). Index
//!   lives in the TABLE's schema. A qualified index name is a GRAMMAR error in PG (oracle):
//!   `create index public.zi on t(b)` -> 42601 `syntax error at or near "."` (position of the dot).
//! * Method (`USING`): `btree` (default) -> `IndexMethod::Btree`; `hash`/`gist`/`gin`/`brin`/`spgist`
//!   -> 0A000 `index method "gin"` unsupported by PGArbor (still recognized, catalog supports only btree);
//!   unknown -> 42704 `access method "foo" does not exist`.
//! * Keys (`IndexColumn { column: OrderByExpr, operator_class }`): a bare column name -> `column:
//!   Some(name)` (42703 `column "x" does not exist`); anything else is an expression key: bind in the
//!   table scope with `ExprCtx::index_expr()` (functions must be IMMUTABLE: 42P17 `functions in index
//!   expression must be marked IMMUTABLE`; no aggregates/windows/sub-selects/SRFs), store with
//!   `stored::store_expr` (canonical text like `lower(name)`), `expr_type` = result type,
//!   `expr_func_name` = function name for a top-level call (drives the attribute name). `COLLATE`,
//!   operator class (`name_ops` — resolve by name among builtin btree opclasses: `text_pattern_ops`,
//!   `varchar_pattern_ops`, `bpchar_pattern_ops`, `int4_ops`...; unknown 42704 `operator class "x" does
//!   not exist for access method "btree"`), `ASC|DESC`, `NULLS FIRST|LAST` (explicit only).
//!   More than 32 key columns: 54011 `cannot use more than 32 columns in an index`.
//! * `INCLUDE (cols)`, `NULLS [NOT] DISTINCT` (`nulls_not_distinct`), `WHERE predicate` (boolean,
//!   immutable functions only: 42P17 `functions in index predicate must be marked IMMUTABLE`; column refs
//!   of the table; stored canonical text), `WITH (opt=value)` -> `options` (`fillfactor`,
//!   `deduplicate_items`; others 22023 `unrecognized parameter "x"`), `CONCURRENTLY` -> `concurrently`,
//!   `IF NOT EXISTS`, `UNIQUE`. Tablespace, `ONLY`, `CREATE INDEX .. ON ONLY`: 0A000.
//! * Duplicate index definitions are allowed (PG warns nothing).
use pgarbor_catalog::builtin;
use pgarbor_catalog::ddl::spec::{IndexKeySpec, IndexSpec};
use pgarbor_catalog::errors as catalog_errors;
use pgarbor_catalog::model::index::IndexMethod;
use pgarbor_catalog::{CatalogEnv, Column, RelKind, Relation, StoredExpr};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::cast::lookup::is_binary_coercible;
use pgarbor_types::catalog_data::Volatility;
use pgarbor_types::{oids, TypeEnv, TypeId};
use sqlparser::ast::{
    BinaryOperator, CreateIndex, Expr, Ident, IndexColumn, IndexOption, IndexType, ObjectName, ObjectNamePart,
    OrderByExpr, OrderBySort, Value, ValueWithSpan,
};
use sqlparser::tokenizer::{Location, Span};

use super::context::{Binder, ExprCtx};
use super::stored;
use crate::diag::{self, ExprClause};
use crate::ir::{BoundExpr, BoundStatement, ExprKind};
use crate::names::QualName;

/// First OID of user objects (PG `FirstNormalObjectId`); lower OIDs are builtin/system objects.
const FIRST_NORMAL_OID: Oid = 16384;

/// Bind CREATE INDEX (module docs).
pub fn bind_create_index(b: &mut Binder, ci: &CreateIndex) -> Result<BoundStatement> {
    if ci.r#async {
        return Err(unsupported("CREATE INDEX ASYNC", None));
    }
    if let Some(opt) = ci.index_options.first() {
        // Index options written after the column list are MySQL syntax; PG rejects them as grammar errors.
        let token = match opt {
            IndexOption::Using(_) => "USING",
            IndexOption::Comment(_) => "COMMENT",
        };
        return Err(diag::syntax_error_near(token, None));
    }
    if !ci.alter_options.is_empty() {
        // MySQL-only `ALGORITHM` / `LOCK` clauses: not PG grammar. The AST carries no span for them.
        return Err(diag::syntax("syntax error", None));
    }
    if ci.columns.is_empty() {
        return Err(diag::invalid_object_definition("must specify at least one column", None));
    }
    if ci.columns.len() + ci.include.len() > 32 {
        return Err(catalog_errors::too_many_index_columns());
    }
    let name = match &ci.name {
        Some(n) => Some(index_name(b, n)?),
        None => None,
    };
    let rel = resolve_table(b, &ci.table_name)?;
    let method = index_method(b, ci.using.as_ref(), ci)?;

    // Expression keys, the predicate and INCLUDE names are bound in a synthetic scope over the table's
    // columns. The scope is removed again whatever happens.
    let depth = b.scopes.len();
    let result = bind_parts(b, rel, ci, name, method);
    b.scopes.truncate(depth);
    result.map(|spec| BoundStatement::CreateIndex(Box::new(spec)))
}

/// Keys, INCLUDE, predicate and options, with the table scope already open.
fn bind_parts(b: &mut Binder, rel: &Relation, ci: &CreateIndex, name: Option<String>, method: IndexMethod) -> Result<IndexSpec> {
    // Dropped columns keep their attnum position as placeholder names that users cannot write, so
    // column references bind to the same attnums as the live table.
    let columns: Vec<(String, TypeId)> = rel
        .columns
        .iter()
        .map(|c| {
            let n = if c.is_visible() { c.name.clone() } else { Column::dropped_name(c.attnum) };
            (n, c.type_id)
        })
        .collect();
    stored::table_expr_scope(b, &rel.name, &columns)?;

    let mut keys = Vec::with_capacity(ci.columns.len());
    for ic in &ci.columns {
        keys.push(bind_key(b, rel, ic)?);
    }
    let include = bind_include(b, rel, &ci.include)?;
    let predicate = match &ci.predicate {
        Some(e) => Some(bind_predicate(b, rel, e)?),
        None => None,
    };
    let options = bind_with(b, &ci.with)?;

    Ok(IndexSpec {
        table: rel.oid,
        name,
        unique: ci.unique,
        // `NULLS NOT DISTINCT` is parsed as `nulls_distinct = Some(false)`.
        nulls_not_distinct: ci.nulls_distinct == Some(false),
        method,
        keys,
        include,
        predicate,
        options,
        if_not_exists: ci.if_not_exists,
        concurrently: ci.concurrently,
    })
}

/// Index name: unqualified (PG `index_elem` grammar; a qualifier is a syntax error at the first dot).
fn index_name(b: &mut Binder, n: &ObjectName) -> Result<String> {
    if n.0.len() != 1 {
        let pos = match n.0.first() {
            Some(ObjectNamePart::Identifier(id)) => dot_position(b, id),
            _ => None,
        };
        return Err(diag::syntax_error_near(".", pos));
    }
    match &n.0[0] {
        ObjectNamePart::Identifier(id) => Ok(b.ident(id)),
        ObjectNamePart::Function(_) => Err(diag::syntax("syntax error", None)),
    }
}

/// Position of the `.` that follows identifier `id` (written without spaces, as PG reports it).
pub(super) fn dot_position(b: &Binder, id: &Ident) -> Option<u32> {
    let end = id.span.end;
    let at_dot = Location { line: end.line, column: end.column + 1 };
    b.pos_span(Span::new(at_dot, at_dot))
}

/// Table of the index: 42P01 via the catalog, cross-database names, system catalogs, kind check.
fn resolve_table<'a>(b: &mut Binder<'a>, n: &ObjectName) -> Result<&'a Relation> {
    let qn: QualName = b.qual(n)?;
    if let Some(cat) = &qn.catalog {
        if *cat != b.ctx.session.database {
            return Err(diag::cross_database(&format!("{}.{}", cat, qn.display()), qn.pos));
        }
    }
    let env: &'a CatalogEnv = b.env;
    // DefineIndex looks the table up without a parse location: no POSITION field.
    let rel = env.resolve_relation(qn.schema_str(), &qn.name)?;
    if rel.oid < FIRST_NORMAL_OID || rel.namespace == builtin::PG_CATALOG_NAMESPACE || rel.namespace == builtin::PG_TOAST_NAMESPACE {
        return Err(at(catalog_errors::system_catalog_permission(&rel.name), qn.pos));
    }
    if !matches!(rel.kind, RelKind::Table | RelKind::MatView) {
        return Err(at(
            PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table or materialized view", rel.name)),
            qn.pos,
        ));
    }
    Ok(rel)
}

/// Access method from `USING` (module docs): btree only; other known methods 0A000; unknown 42704.
fn index_method(b: &mut Binder, using: Option<&IndexType>, ci: &CreateIndex) -> Result<IndexMethod> {
    let name: String = match using {
        None => return Ok(IndexMethod::Btree),
        Some(t) => match t {
            IndexType::BTree => return Ok(IndexMethod::Btree),
            IndexType::Hash => "hash".to_string(),
            IndexType::GIN => "gin".to_string(),
            IndexType::GiST => "gist".to_string(),
            IndexType::SPGiST => "spgist".to_string(),
            IndexType::BRIN => "brin".to_string(),
            IndexType::Bloom => "bloom".to_string(),
            IndexType::Custom(id) => b.ident(id),
        },
    };
    match IndexMethod::from_name(&name) {
        Some(IndexMethod::Btree) => Ok(IndexMethod::Btree),
        Some(_) => {
            // DefineIndex checks the access method's capabilities before anything is built.
            let key_count = ci.columns.len();
            let (multicol, unique, include) = match name.as_str() {
                "hash" => (false, false, false),
                "spgist" => (false, false, true),
                "gist" => (true, false, true),
                _ => (true, false, false),
            };
            let fail = |what: &str| PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("access method \"{name}\" does not support {what}"));
            if key_count > 1 && !multicol {
                return Err(fail("multicolumn indexes"));
            }
            if ci.unique && !unique {
                return Err(fail("unique indexes"));
            }
            if !ci.include.is_empty() && !include {
                return Err(fail("included columns"));
            }
            Err(unsupported(&format!("index method \"{name}\""), None))
        }
        None => Err(catalog_errors::access_method_does_not_exist(&name)),
    }
}

/// One key column (module docs): a bare column name or an expression, with opclass, COLLATE and order.
fn bind_key(b: &mut Binder, rel: &Relation, ic: &IndexColumn) -> Result<IndexKeySpec> {
    let (descending, nulls_first) = sort_options(b, &ic.column)?;
    let (base, collation) = match &ic.column.expr {
        Expr::Collate { expr, collation } => (&**expr, Some(collation)),
        other => (other, None),
    };

    if let Expr::Identifier(id) = base {
        let name = b.ident(id);
        let col = rel.column_by_name(&name);
        if col.is_none() && !is_system_column(&name) {
            return Err(diag::undefined_column(None, &name, None));
        }
        if col.is_none() {
            return Err(system_column_index_error());
        }
        let key_type: Option<Oid> = col.map(|c| c.type_id.oid);
        let collation_oid = match collation {
            Some(c) => Some(resolve_collation(b, c)?),
            None => None,
        };
        let opclass = match &ic.operator_class {
            Some(n) => Some(resolve_opclass(b, n, key_type)?),
            None => None,
        };
        return Ok(IndexKeySpec {
            column: Some(name),
            expr: None,
            expr_func_name: None,
            expr_type: None,
            opclass,
            collation: collation_oid,
            descending,
            nulls_first,
        });
    }

    // Expression key: the COLLATE (if any) is part of the bound expression.
    let _ = collation;
    let names = |a: i16| column_name_for_attnum(rel, a);
    let mut refs: Vec<i16> = Vec::new();
    reject_system_expr(b, &ic.column.expr, ExprCtx::index_expr())?;
    let (bound, stored_expr) =
        stored::bind_ddl_expr(b, &ic.column.expr, ExprCtx::index_expr(), None, Some(rel.oid), &mut refs, &names)?;
    ensure_immutable(b, &ic.column.expr, &bound, "index expression")?;

    let func_name = match &bound.kind {
        ExprKind::FuncCall(f) if !f.is_cast_call => Some(f.name.clone()),
        _ => None,
    };
    let expr_type = bound.ty;
    let opclass = match &ic.operator_class {
        Some(n) => Some(resolve_opclass(b, n, Some(expr_type.oid))?),
        None => None,
    };
    Ok(IndexKeySpec {
        column: None,
        expr: Some(stored_expr),
        expr_func_name: func_name,
        expr_type: Some(expr_type),
        opclass,
        collation: None,
        descending,
        nulls_first,
    })
}

/// `ASC|DESC` and explicit `NULLS FIRST|LAST`. `USING <operator>` ordering is not supported.
fn sort_options(b: &Binder, oe: &OrderByExpr) -> Result<(bool, Option<bool>)> {
    let descending = match &oe.options.sort {
        None | Some(OrderBySort::Asc) => false,
        Some(OrderBySort::Desc) => true,
        Some(OrderBySort::Using(_)) => {
            return Err(unsupported("index column ordering with USING operator", b.pos(&oe.expr)));
        }
    };
    Ok((descending, oe.options.nulls_first))
}

/// `COLLATE name` -> collation OID (42704 `collation "x" for encoding "UTF8" does not exist` via the catalog).
fn resolve_collation(b: &mut Binder, n: &ObjectName) -> Result<Oid> {
    let qn: QualName = b.qual(n)?;
    b.env.collation_oid(qn.schema_str(), &qn.name).map_err(|e| at(e, qn.pos))
}

/// Operator class by name among the builtin btree opclasses (module docs). `key_type` (when known)
/// must be binary-coercible to the opclass input type (PG 42804 `operator class "x" does not accept
/// data type y`).
fn resolve_opclass(b: &mut Binder, n: &ObjectName, key_type: Option<Oid>) -> Result<Oid> {
    let qn: QualName = b.qual(n)?;
    let mut found: Option<&'static [Option<&'static str>]> = None;
    for row in builtin::OPCLASSES.rows.iter() {
        // Column order: oid, opcmethod, opcname, opcnamespace, opcowner, opcfamily, opcintype, opcdefault, opckeytype.
        if cell(row, 1) != Some("403") || cell(row, 2) != Some(qn.name.as_str()) {
            continue;
        }
        if let Some(s) = qn.schema_str() {
            if s != "pg_catalog" {
                continue;
            }
        }
        found = Some(*row);
        break;
    }
    let row = match found {
        Some(r) => r,
        None => {
            // PG raises these without parser_errposition: no POSITION field.
            return Err(PgError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("operator class \"{}\" does not exist for access method \"btree\"", qn.display()),
            ));
        }
    };
    let oid: Oid = cell(row, 0)
        .and_then(|s| s.parse::<Oid>().ok())
        .ok_or_else(|| PgError::internal("builtin btree opclass row without an OID"))?;
    let intype: Oid = cell(row, 6)
        .and_then(|s| s.parse::<Oid>().ok())
        .ok_or_else(|| PgError::internal("builtin opclass row without opcintype"))?;
    if let Some(kt) = key_type {
        let env: &dyn TypeEnv = b.env;
        if !is_binary_coercible(env, kt, intype)? {
            let msg = format!("operator class \"{}\" does not accept data type {}", qn.display(), b.type_name(TypeId::of(kt)));
            return Err(PgError::new(SqlState::DATATYPE_MISMATCH, msg));
        }
    }
    Ok(oid)
}

/// Text cell of a builtin catalog row.
fn cell(row: &[Option<&'static str>], i: usize) -> Option<&'static str> {
    row.get(i).copied().flatten()
}

/// INCLUDE columns: plain existing columns (42703 otherwise).
fn bind_include(b: &mut Binder, rel: &Relation, names: &[Ident]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(names.len());
    for id in names {
        let name = b.ident(id);
        if rel.column_by_name(&name).is_none() {
            if !is_system_column(&name) {
                return Err(diag::undefined_column(None, &name, None));
            }
            return Err(system_column_index_error());
        }
        out.push(name);
    }
    Ok(out)
}

/// `WHERE` predicate: boolean, immutable, stored as canonical text.
fn bind_predicate(b: &mut Binder, rel: &Relation, e: &Expr) -> Result<StoredExpr> {
    let ec = ExprCtx { clause: ExprClause::IndexPredicate, ..ExprCtx::index_expr() };
    let names = |a: i16| column_name_for_attnum(rel, a);
    let mut refs: Vec<i16> = Vec::new();
    reject_system_expr(b, e, ec)?;
    let (bound, stored_expr) = stored::bind_ddl_expr(b, e, ec, None, Some(rel.oid), &mut refs, &names)?;
    if bound.ty.oid != oids::BOOL {
        let actual = b.type_name(bound.ty);
        return Err(diag::not_boolean("WHERE", &actual, b.pos(e)));
    }
    ensure_immutable(b, e, &bound, "index predicate")?;
    Ok(stored_expr)
}

/// `WITH (name = value, ...)` -> `name=value` strings (module docs). Validation follows PG reloptions
/// for the btree options `fillfactor` (10..=100) and `deduplicate_items` (boolean).
fn bind_with(b: &mut Binder, with: &[Expr]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(with.len());
    for e in with {
        let pos = b.pos(e);
        let (id, value) = match e {
            Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => match &**left {
                Expr::Identifier(id) => (id, value_text(b, right, pos)?),
                _ => return Err(diag::syntax("syntax error", pos)),
            },
            // `WITH (name)` is the boolean `true` (PG `defGetString` of a bare option).
            Expr::Identifier(id) => (id, "true".to_string()),
            _ => return Err(diag::syntax("syntax error", pos)),
        };
        let name = b.ident(id);
        match name.as_str() {
            "fillfactor" => {
                let v = match value.trim().parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => {
                        let msg = format!("invalid value for integer option \"fillfactor\": {value}");
                        return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg));
                    }
                };
                if !(10..=100).contains(&v) {
                    let msg = format!("value {value} out of bounds for option \"fillfactor\"");
                    let err = PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg)
                        .with_detail("Valid values are between \"10\" and \"100\".");
                    return Err(err);
                }
            }
            "deduplicate_items" => {
                if parse_bool_option(&value).is_none() {
                    let msg = format!("invalid value for boolean option \"deduplicate_items\": {value}");
                    return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg));
                }
            }
            _ => {
                let msg = format!("unrecognized parameter \"{name}\"");
                return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg));
            }
        }
        out.push(format!("{name}={value}"));
    }
    Ok(out)
}

/// Literal text of a `WITH` option value (numbers, strings, booleans, bare words).
fn value_text(b: &mut Binder, e: &Expr, pos: Option<u32>) -> Result<String> {
    match e {
        Expr::Value(ValueWithSpan { value, .. }) => match value {
            Value::Number(n, _) => Ok(n.to_string()),
            Value::SingleQuotedString(s) => Ok(s.clone()),
            Value::Boolean(v) => Ok(if *v { "true".to_string() } else { "false".to_string() }),
            _ => Err(diag::syntax("syntax error", pos)),
        },
        Expr::Identifier(id) => Ok(b.ident(id)),
        _ => Err(diag::syntax("syntax error", pos)),
    }
}

/// PG `parse_bool`: `true false yes no on off 1 0` and unique prefixes thereof, case-insensitive.
fn parse_bool_option(v: &str) -> Option<bool> {
    let lower = v.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    const WORDS: [(&str, bool); 8] = [
        ("true", true),
        ("false", false),
        ("yes", true),
        ("no", false),
        ("on", true),
        ("off", false),
        ("1", true),
        ("0", false),
    ];
    for (word, val) in WORDS.iter() {
        if word.starts_with(lower.as_str()) {
            return Some(*val);
        }
    }
    None
}

/// Immutability of an index expression or predicate (42P17). `what` is `index expression` or
/// `index predicate`.
fn ensure_immutable(b: &Binder, e: &Expr, bound: &BoundExpr, what: &str) -> Result<()> {
    if matches!(bound.volatility(), Volatility::Immutable) {
        return Ok(());
    }
    let _ = (b, e);
    Err(diag::invalid_object_definition(format!("functions in {what} must be marked IMMUTABLE"), None))
}

/// Column name of attnum `a` for stored-expression deparsing (dropped columns have no name).
fn column_name_for_attnum(rel: &Relation, a: i16) -> Option<String> {
    rel.column(a).filter(|c| c.is_visible()).map(|c| c.name.clone())
}

/// System column names that the catalog resolves (its own 42704 applies to them).
/// indexcmds.c `DefineIndex`: system attributes (attnum < 0) cannot be index keys, INCLUDE columns or
/// referenced by index expressions and predicates.
fn system_column_index_error() -> PgError {
    PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "index creation on system columns is not supported")
}

fn reject_system_expr(b: &mut Binder, e: &Expr, ec: ExprCtx) -> Result<()> {
    // Reject before storing/deparsing; the stored-expression namer only accepts user attributes.
    let bound = super::expr::bind_expr(b, e, ec)?;
    if bound.any(&mut |n| matches!(&n.kind, ExprKind::Column(c) if c.levels_up == 0 && c.attnum < 0)) {
        return Err(system_column_index_error());
    }
    Ok(())
}

fn is_system_column(name: &str) -> bool {
    matches!(name, "ctid" | "xmin" | "xmax" | "cmin" | "cmax" | "tableoid")
}

fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    e.position = pos;
    e
}

/// 0A000 `<what> is not supported by PGArbor yet`.
fn unsupported(what: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by PGArbor yet")), pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_index(sql: &str) -> CreateIndex {
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL parses");
        match stmts.into_iter().next() {
            Some(sqlparser::ast::Statement::CreateIndex(ci)) => ci,
            other => panic!("expected CREATE INDEX in test input, got {other:?}"),
        }
    }

    #[test]
    fn bool_option_words_and_prefixes() {
        assert_eq!(parse_bool_option("on"), Some(true));
        assert_eq!(parse_bool_option("OFF"), Some(false));
        assert_eq!(parse_bool_option("t"), Some(true));
        assert_eq!(parse_bool_option("fa"), Some(false));
        assert_eq!(parse_bool_option("yes"), Some(true));
        assert_eq!(parse_bool_option("0"), Some(false));
        assert_eq!(parse_bool_option("maybe"), None);
        assert_eq!(parse_bool_option(""), None);
    }

    #[test]
    fn system_column_names() {
        assert!(is_system_column("xmin"));
        assert!(is_system_column("ctid"));
        assert!(!is_system_column("a"));
    }

    #[test]
    fn parsed_shapes_for_binder_inputs() {
        let ci = parse_index("CREATE INDEX ON t(a)");
        assert!(ci.name.is_none());
        assert_eq!(ci.columns.len(), 1);
        assert!(ci.predicate.is_none());

        let ci = parse_index("CREATE UNIQUE INDEX i ON t(lower(b)) WHERE a > 0");
        assert!(ci.unique);
        assert_eq!(ci.columns.len(), 1);
        assert!(ci.predicate.is_some());

        let ci = parse_index("CREATE INDEX i ON t(a) WITH (fillfactor = 70)");
        assert_eq!(ci.with.len(), 1);
    }

    #[test]
    fn qualified_index_name_is_parsed_as_multi_part_name() {
        // The binder turns the multi-part name into a 42601 at the first dot; the AST keeps the parts.
        let ci = parse_index("CREATE INDEX public.zi ON t(b)");
        assert_eq!(ci.name.as_ref().map(|n| n.0.len()), Some(2));
    }

    #[test]
    #[ignore = "needs SQL-29"]
    fn expression_key_binding_needs_stored_expr() {
        // Expression keys go through stored::bind_ddl_expr (SQL-29), which is not implemented yet.
    }
}
