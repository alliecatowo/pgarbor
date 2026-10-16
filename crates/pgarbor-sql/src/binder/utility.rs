//! PREPARE / EXECUTE / DEALLOCATE, EXPLAIN, LISTEN/NOTIFY/UNLISTEN, LOCK, VACUUM/ANALYZE/REINDEX/
//! CLUSTER/CHECKPOINT (SQL-41). PG references: `prepare.c` (`PrepareQuery`, `ExecuteQuery`,
//! `EvaluateParams`), `explain.c` (`ExplainQuery`, option parsing `ExplainQuery` switch), `async.c`,
//! `lockcmds.c` (`LockTableCommand`), `vacuum.c`, `gram.y`.
//!
//! ## PREPARE (`Statement::Prepare { name, data_types, statement }`)
//! `data_types` -> declared parameter types (`types::resolve_data_type`); the inner statement is bound
//! with a FRESH parameter table seeded with those types (`BindContext::declared_params` semantic: build a
//! nested `ParamTable::new(&declared)` and swap it into the Binder while binding the inner statement via
//! `dispatch::bind_inner_statement(.., top_level = false)`; only `SELECT`, `INSERT`, `UPDATE`, `DELETE`,
//! `MERGE`, `VALUES` allowed: 42601 `syntax error at or near ...`-style `PREPARE` of other statements
//! is a grammar error). Result `BoundPrepare { name (folded), param_types (final, after inference;
//! 42P18 if unresolved), statement, text }`; `text` = the PREPARE statement's own source text
//! (what `pg_prepared_statements.statement` shows). Duplicate prepared-statement names (42P05
//! `prepared statement "x" already exists`) are the SESSION's check.
//! ## EXECUTE (`Statement::Execute { name, parameters, using, has_parentheses, immediate, into, output, default }`)
//! `parameters` bound with `ExprCtx::execute_args()` (no columns; sub-selects allowed; `$n` not allowed:
//! 42P02) -> `BoundExecute { name, args }`; coercion to the prepared statement's parameter types happens in
//! the session with [`coerce_execute_args`] (42804 `parameter $1 of type integer cannot be coerced to the
//! expected type text`, 42601? `wrong number of parameters for prepared statement "x"` 42601 DETAIL
//! `Expected 2 parameters but got 1.`; 26000 `prepared statement "x" does not exist`). `EXECUTE IMMEDIATE`,
//! `INTO`, `OUTPUT`, `DEFAULT`: 42601.
//! ## DEALLOCATE (`Statement::Deallocate { name, prepare }`) -> `Deallocate(Some(name))`; `DEALLOCATE ALL`
//! (name `ALL`) -> `Deallocate(None)`.
//!
//! ## EXPLAIN (`Statement::Explain { describe_alias, analyze, verbose, query_plan, estimate, statement,
//! format, options }`)
//! `describe_alias` DESCRIBE/DESC (MySQL) -> 42601. Options in the parenthesized list `(ANALYZE [bool],
//! VERBOSE, COSTS, SETTINGS, GENERIC_PLAN, BUFFERS, SERIALIZE, WAL, TIMING, SUMMARY, MEMORY, FORMAT
//! TEXT|XML|JSON|YAML)` and the legacy `EXPLAIN [ANALYZE] [VERBOSE] stmt`; unknown option: 42601
//! `unrecognized EXPLAIN option "x"`; `FORMAT bad`: 22023 `unrecognized value for EXPLAIN option
//! "format": "bad"` (both positioned at the option; oracle). Defaults in `ExplainOptions::default()` (`COSTS` on; `ANALYZE` implies
//! `TIMING`/`SUMMARY`/`BUFFERS` off here). The inner statement is bound with `dispatch::bind_inner_statement`
//! and must be plannable (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `VALUES`, `CREATE TABLE AS`, `EXECUTE`,
//! `DECLARE`, `REFRESH MATERIALIZED VIEW`); anything else is a grammar error 42601. Result
//! `Explain(Box<BoundExplain{options, statement}>)`.
//!
//! ## LISTEN / UNLISTEN / NOTIFY
//! `LISTEN ch` -> `Listen(ch)` (channel is an identifier, folded, max 63 bytes); `UNLISTEN ch|*` ->
//! `Unlisten`; `NOTIFY ch [, 'payload']` -> `Notify { channel, payload }` (payload must be a string literal;
//! > 7999 bytes: 22023 `payload string too long`; channel empty: 22023 `channel name cannot be empty`).
//! ## LOCK (`Statement::Lock(LockTableStmt { tables, lock_mode, nowait, only, .. })`)
//! `LOCK [TABLE] [ONLY] t,.. [IN mode MODE] [NOWAIT]` -> `Lock(BoundLock{tables, mode, nowait})`; default
//! mode ACCESS EXCLUSIVE; tables resolved (42P01), must be tables/views.
//! ## Maintenance
//! `VACUUM [(opts)|FULL|FREEZE|ANALYZE|VERBOSE] [t,..]`, `ANALYZE [t]` (`Statement::Analyze`),
//! `REINDEX ...`, `CLUSTER`, `CHECKPOINT` -> `Maintenance { kind, targets: Vec<Oid> }` (accepted, no
//! physical effect; targets resolved so missing tables still error 42P01). `VACUUM` cannot run inside
//! a transaction block (25001, session).
use pgarbor_catalog::snapshot::ClassRef;
use pgarbor_catalog::{CatalogEnv, Relation, RelKind};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_types::catalog_data::CastContext;
use pgarbor_types::TypeId;
use sqlparser::ast::{Analyze, DescribeAlias, Expr, Ident, LockTableMode, ObjectName, Statement, UtilityOption, Value, VacuumStatement};

use super::context::{BindContext, Binder, BoundArg, ExprCtx};
use super::params::ParamTable;
use super::{coerce, dispatch, expr, params, types};
use crate::diag;
use crate::extra_stmt::{ExtraStatement, MaintenanceOption, ReindexKind};
use crate::ir::{
    BoundExecute, BoundExplain, BoundExpr, BoundLock, BoundPrepare, BoundStatement, CoercionForm, ExplainFormat,
    ExplainOptions, ExprKind, LockMode, MaintenanceKind,
};
use crate::names::QualName;

/// Longest NOTIFY payload in bytes. PG rejects `strlen(payload) >= NOTIFY_PAYLOAD_MAX_LENGTH` (8000).
const NOTIFY_PAYLOAD_MAX_BYTES: usize = 7999;

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Position of an identifier.
fn ident_pos(b: &Binder, id: &Ident) -> Option<u32> {
    b.pos_span(id.span)
}

/// An unquoted identifier spelled `word` (case-insensitive), e.g. the keyword `ALL`.
fn is_keyword(id: &Ident, word: &str) -> bool {
    id.quote_style.is_none() && id.value.eq_ignore_ascii_case(word)
}

/// `syntax error at or near "<token>"`, where the token is the word the query text has at
/// character position `pos` (PG prints the token as written).
fn syntax_error_at(b: &Binder, pos: Option<u32>) -> PgError {
    let token: String = match pos {
        Some(p) => b
            .src
            .text()
            .chars()
            .skip(p.saturating_sub(1) as usize)
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
        None => String::new(),
    };
    if token.is_empty() {
        PgError::syntax("syntax error", pos)
    } else {
        diag::syntax_error_near(&token, pos)
    }
}

/// 3-part names must name the connected database.
fn check_database(b: &Binder, qn: &QualName) -> Result<()> {
    match &qn.catalog {
        Some(cat) if *cat != b.ctx.session.database => {
            Err(diag::cross_database(&format!("{cat}.{}", qn.display()), qn.pos))
        }
        _ => Ok(()),
    }
}

/// Resolve a table, view, or matview name (PG `RangeVarGetRelidExtended`): returns a copy of the
/// catalog entry and the name's position. 42P01 when missing.
fn resolve_relation(b: &mut Binder, name: &ObjectName) -> Result<(Relation, Option<u32>)> {
    let qn = b.qual(name)?;
    check_database(b, &qn)?;
    let env: &CatalogEnv = b.env;
    match env.resolve_relation(qn.schema.as_deref(), &qn.name) {
        Ok(rel) => Ok((rel.clone(), qn.pos)),
        Err(e) => Err(diag::with_position(e, qn.pos)),
    }
}

/// Relation kinds that VACUUM / ANALYZE / CLUSTER / REINDEX TABLE act on.
fn is_table_like(kind: RelKind) -> bool {
    matches!(kind, RelKind::Table | RelKind::PartitionedTable | RelKind::MatView)
}

/// Kind word of PG's `errdetail_relkind_not_supported`.
fn kind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table | RelKind::PartitionedTable => "tables",
        RelKind::View => "views",
        RelKind::MatView => "materialized views",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::Sequence => "sequences",
        RelKind::CompositeType => "composite types",
        RelKind::ForeignTable => "foreign tables",
        RelKind::Toast => "TOAST tables",
    }
}

/// PG's skip WARNING for a maintenance command that names a relation it cannot process.
fn skip_warning(b: &mut Binder, rel_name: &str, verb: &str) {
    // PG `ereport(WARNING)` without an errcode: SQLSTATE 01000 (ERRCODE_WARNING), not 00000.
    let w = PgError::new(
        SqlState::WARNING,
        format!("skipping \"{rel_name}\" --- cannot {verb} non-tables or special system tables"),
    )
    .with_severity(Severity::Warning);
    b.notice(w);
}

/// A channel name: folded and truncated like any identifier (`b.ident`); empty is 22023.
fn channel_name(b: &mut Binder, id: &Ident) -> Result<String> {
    let name = b.ident(id);
    if name.is_empty() {
        let pos = ident_pos(b, id);
        return Err(diag::with_position(
            PgError::new(SqlState::INVALID_PARAMETER_VALUE, "channel name cannot be empty"),
            pos,
        ));
    }
    Ok(name)
}

// ---------------------------------------------------------------------------------------------
// PREPARE / EXECUTE / DEALLOCATE
// ---------------------------------------------------------------------------------------------

/// Statements PREPARE accepts (PG `PreparableStmt`).
fn is_preparable(s: &Statement) -> bool {
    matches!(
        s,
        Statement::Query(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) | Statement::Merge(_)
    )
}

/// PREPARE.
pub fn bind_prepare(b: &mut Binder, stmt: &Statement, stmt_text: &str) -> Result<BoundStatement> {
    let Statement::Prepare { name, data_types, statement } = stmt else {
        return Err(PgError::internal("bind_prepare called with a non-PREPARE statement"));
    };
    let inner: &Statement = statement.as_ref();
    if !is_preparable(inner) {
        return Err(syntax_error_at(b, b.pos(inner)));
    }
    let prepared_name = b.ident(name);
    let mut declared: Vec<TypeId> = Vec::with_capacity(data_types.len());
    for dt in data_types {
        declared.push(types::resolve_data_type(b, dt, None)?.id);
    }

    // The inner statement sees its own parameter table, seeded with the declared types.
    let saved = std::mem::replace(&mut b.params, ParamTable::new(&declared));
    let bound = bind_prepared_inner(b, inner);
    b.params = saved;
    let (bound_inner, param_types) = bound?;

    Ok(BoundStatement::Prepare(Box::new(BoundPrepare {
        name: prepared_name,
        param_types,
        statement: Box::new(bound_inner),
        text: stmt_text.to_string(),
    })))
}

/// Bind the statement inside PREPARE and finalize its parameter types.
fn bind_prepared_inner(b: &mut Binder, inner: &Statement) -> Result<(BoundStatement, Vec<TypeId>)> {
    let bound = dispatch::bind_inner_statement(b, inner, false)?;
    let final_types = params::finalize(b)?;
    Ok((bound, final_types))
}

/// First `$n` referenced inside a bound expression, if any.
fn first_param(e: &BoundExpr) -> Option<u32> {
    let mut found: Option<u32> = None;
    e.walk(&mut |x: &BoundExpr| {
        if found.is_some() {
            return false;
        }
        if let ExprKind::Param(n) = &x.kind {
            found = Some(*n);
            return false;
        }
        true
    });
    found
}

/// EXECUTE.
pub fn bind_execute(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::Execute { name, parameters, immediate, into, using, output, default, .. } = stmt else {
        return Err(PgError::internal("bind_execute called with a non-EXECUTE statement"));
    };
    if *immediate {
        return Err(diag::syntax_error_near("IMMEDIATE", None));
    }
    if !into.is_empty() {
        return Err(diag::syntax_error_near("INTO", None));
    }
    if !using.is_empty() {
        return Err(diag::syntax_error_near("USING", None));
    }
    if *output {
        return Err(diag::syntax_error_near("OUTPUT", None));
    }
    if *default {
        return Err(diag::syntax_error_near("DEFAULT", None));
    }
    let Some(name) = name else {
        return Err(diag::syntax_error_near(";", None));
    };
    let qn = b.qual(name)?;
    if qn.schema.is_some() || qn.catalog.is_some() {
        return Err(diag::syntax_error_near(".", qn.pos));
    }

    let mut args: Vec<BoundExpr> = Vec::with_capacity(parameters.len());
    let mut arg_positions: Vec<Option<u32>> = Vec::with_capacity(parameters.len());
    for arg_ast in parameters {
        arg_positions.push(b.pos(arg_ast));
        let arg = expr::bind_arg(b, arg_ast, ExprCtx::execute_args())?;
        if let Some(n) = first_param(&arg.expr) {
            let mut e = PgError::new(SqlState::UNDEFINED_PARAMETER, format!("there is no parameter ${n}"));
            e.position = b.pos(arg_ast);
            return Err(e);
        }
        args.push(arg.expr);
    }
    Ok(BoundStatement::Execute(Box::new(BoundExecute { name: qn.name, args, arg_positions: arg_positions })))
}

/// DEALLOCATE.
pub fn bind_deallocate(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::Deallocate { name, .. } = stmt else {
        return Err(PgError::internal("bind_deallocate called with a non-DEALLOCATE statement"));
    };
    if is_keyword(name, "all") {
        return Ok(BoundStatement::Deallocate(None));
    }
    Ok(BoundStatement::Deallocate(Some(b.ident(name))))
}

/// Coerce EXECUTE arguments to the prepared statement's parameter types (used by the session, which
/// owns the prepared statement table). Each arg: assignment-context coercion (PG `EvaluateParams` uses
/// `coerce_to_target_type` with `COERCION_ASSIGNMENT`); wrong count -> 42601 `wrong number of parameters
/// for prepared statement "name"` DETAIL `Expected N parameters but got M.`; type failure -> 42804
/// `parameter $1 of type integer cannot be coerced to the expected type text` HINT `You will need to
/// rewrite or cast the expression.` Pure over `BoundExpr` + the catalog env; no Binder state: it builds
/// a throwaway `Binder`.
pub fn coerce_execute_args(
    ctx: &BindContext,
    src: &crate::source::SourceMap,
    name: &str,
    args: Vec<BoundExpr>,
    param_types: &[TypeId],
) -> Result<Vec<BoundExpr>> {
    if args.len() != param_types.len() {
        return Err(PgError::new(
            SqlState::SYNTAX_ERROR,
            format!("wrong number of parameters for prepared statement \"{name}\""),
        )
        .with_detail(format!("Expected {} parameters but got {}.", param_types.len(), args.len())));
    }
    let mut b = Binder::new(ctx, src);
    let mut out: Vec<BoundExpr> = Vec::with_capacity(args.len());
    for (i, (arg, target)) in args.into_iter().zip(param_types.iter().copied()).enumerate() {
        let given = arg.ty;
        let bound_arg = BoundArg::new(arg, None);
        match coerce::try_coerce(&mut b, &bound_arg, target, CastContext::Assignment, CoercionForm::Implicit)? {
            Some(coerced) => out.push(coerced),
            None => {
                let msg = format!(
                    "parameter ${} of type {} cannot be coerced to the expected type {}",
                    i + 1,
                    b.type_name(given),
                    b.type_name(target)
                );
                return Err(PgError::new(SqlState::DATATYPE_MISMATCH, msg)
                    .with_hint("You will need to rewrite or cast the expression."));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// EXPLAIN
// ---------------------------------------------------------------------------------------------

/// Statements EXPLAIN accepts (PG `ExplainableStmt`).
fn is_explainable(s: &Statement) -> bool {
    match s {
        Statement::Query(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => true,
        Statement::Execute { .. } | Statement::Declare { .. } => true,
        Statement::CreateTable(ct) => ct.query.is_some(),
        _ => false,
    }
}

/// `EXPLAIN option requires ANALYZE` (PG `ParseExplainOptionList`).
fn requires_analyze(opt: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("EXPLAIN option {opt} requires ANALYZE"))
}

/// `unrecognized value for EXPLAIN option "x": "y"`, positioned at the option.
fn bad_option_value(name: &str, value: &str, pos: Option<u32>) -> PgError {
    diag::with_position(
        PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unrecognized value for EXPLAIN option \"{name}\": \"{value}\""),
        ),
        pos,
    )
}

/// PG `defGetBoolean` spellings.
fn parse_bool_word(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "on" | "1" => Some(true),
        "false" | "f" | "no" | "n" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// PG `defGetBoolean`: a missing argument means true; anything else not boolean is 42601.
fn option_bool(name: &str, arg: Option<&Expr>) -> Result<bool> {
    let not_boolean = || PgError::new(SqlState::SYNTAX_ERROR, format!("{name} requires a Boolean value"));
    match arg {
        None => Ok(true),
        Some(Expr::Value(v)) => match &v.value {
            Value::Boolean(flag) => Ok(*flag),
            Value::SingleQuotedString(text) => parse_bool_word(text).ok_or_else(not_boolean),
            // `defGetBoolean` T_Integer: only 0 and 1.
            Value::Number(text, _) => match text.as_str() {
                "1" => Ok(true),
                "0" => Ok(false),
                _ => Err(not_boolean()),
            },
            _ => Err(not_boolean()),
        },
        Some(Expr::Identifier(id)) => parse_bool_word(&id.value).ok_or_else(not_boolean),
        Some(_) => Err(not_boolean()),
    }
}

/// PG `defGetString`: an identifier (folded unless quoted) or a string literal.
fn option_text(name: &str, arg: Option<&Expr>) -> Result<String> {
    let missing = || PgError::new(SqlState::SYNTAX_ERROR, format!("{name} requires a parameter"));
    match arg {
        Some(Expr::Identifier(id)) => {
            if id.quote_style.is_some() {
                Ok(id.value.clone())
            } else {
                Ok(id.value.to_ascii_lowercase())
            }
        }
        Some(Expr::Value(v)) => match &v.value {
            Value::SingleQuotedString(text) => Ok(text.clone()),
            _ => Err(missing()),
        },
        _ => Err(missing()),
    }
}

/// Parenthesized EXPLAIN options (PG `ParseExplainOptionList`, explain_state.c). Defaults: COSTS on;
/// TIMING, BUFFERS and SUMMARY follow ANALYZE unless written explicitly.
fn parse_explain_options(b: &Binder, list: &[UtilityOption]) -> Result<ExplainOptions> {
    let mut o = ExplainOptions::default();
    let mut timing: Option<bool> = None;
    let mut buffers: Option<bool> = None;
    let mut summary: Option<bool> = None;
    let mut serialize = false;
    let mut generic_plan = false;
    let mut memory = false;

    for opt in list {
        let name = opt.name.value.to_ascii_lowercase();
        let pos = ident_pos(b, &opt.name);
        let arg = opt.arg.as_ref();
        match name.as_str() {
            "analyze" => o.analyze = option_bool(&name, arg)?,
            "verbose" => o.verbose = option_bool(&name, arg)?,
            "costs" => o.costs = option_bool(&name, arg)?,
            "buffers" => buffers = Some(option_bool(&name, arg)?),
            "wal" => o.wal = option_bool(&name, arg)?,
            "settings" => o.settings = option_bool(&name, arg)?,
            "generic_plan" => generic_plan = option_bool(&name, arg)?,
            "timing" => timing = Some(option_bool(&name, arg)?),
            "summary" => summary = Some(option_bool(&name, arg)?),
            "memory" => memory = option_bool(&name, arg)?,
            "serialize" => {
                serialize = match arg {
                    // SERIALIZE without an argument means TEXT.
                    None => true,
                    Some(_) => {
                        let v = option_text(&name, arg)?;
                        match v.as_str() {
                            "off" | "none" => false,
                            "text" | "binary" => true,
                            _ => return Err(bad_option_value(&name, &v, pos)),
                        }
                    }
                };
            }
            "format" => {
                let v = option_text(&name, arg)?;
                o.format = match v.as_str() {
                    "text" => ExplainFormat::Text,
                    "xml" => ExplainFormat::Xml,
                    "json" => ExplainFormat::Json,
                    "yaml" => ExplainFormat::Yaml,
                    _ => return Err(bad_option_value(&name, &v, pos)),
                };
            }
            _ => {
                return Err(diag::with_position(
                    PgError::new(SqlState::SYNTAX_ERROR, format!("unrecognized EXPLAIN option \"{name}\"")),
                    pos,
                ));
            }
        }
    }

    if o.wal && !o.analyze {
        return Err(requires_analyze("WAL"));
    }
    o.timing = timing.unwrap_or(o.analyze);
    o.buffers = buffers.unwrap_or(o.analyze);
    o.summary = summary.unwrap_or(o.analyze);
    if o.timing && !o.analyze {
        return Err(requires_analyze("TIMING"));
    }
    if serialize && !o.analyze {
        return Err(requires_analyze("SERIALIZE"));
    }
    if generic_plan && o.analyze {
        return Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
        ));
    }
    // Accepted by PostgreSQL but without an effect PGArbor can report yet: turning them on is an
    // explicit 0A000 rather than a silent no-op.
    if serialize {
        return Err(diag::unsupported_construct("EXPLAIN option SERIALIZE", None));
    }
    if generic_plan {
        return Err(diag::unsupported_construct("EXPLAIN option GENERIC_PLAN", None));
    }
    if memory {
        return Err(diag::unsupported_construct("EXPLAIN option MEMORY", None));
    }
    Ok(o)
}

/// 1-based character position of the first token after the EXPLAIN option header of `text` (the
/// statement text, which starts at character `start`). The header is `EXPLAIN`, an optional
/// parenthesized option list, then the legacy words ANALYZE / ANALYSE / VERBOSE (PG `gram.y`
/// `ExplainStmt`). Byte scanning only stops at ASCII bytes, so the cut point is a char boundary.
pub(super) fn explain_body_pos(text: &str, start: u32) -> Option<u32> {
    fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }
    fn skip_word(bytes: &[u8], mut i: usize) -> usize {
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        i
    }
    let bytes = text.as_bytes();
    let n = bytes.len();
    // The leading EXPLAIN keyword.
    let mut i = skip_word(bytes, skip_ws(bytes, 0));
    loop {
        i = skip_ws(bytes, i);
        if i < n && bytes[i] == b'(' {
            let mut depth = 0usize;
            while i < n {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            continue;
        }
        let end = skip_word(bytes, i);
        let word = text.get(i..end).unwrap_or("");
        if !word.is_empty() && ["ANALYZE", "ANALYSE", "VERBOSE"].iter().any(|w| word.eq_ignore_ascii_case(w)) {
            i = end;
            continue;
        }
        break;
    }
    let i = skip_ws(bytes, i);
    let chars = text.get(..i)?.chars().count();
    Some(start.checked_add(u32::try_from(chars).ok()?)?)
}

/// EXPLAIN.
pub fn bind_explain(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::Explain { describe_alias, analyze, verbose, query_plan, estimate, statement, format, options } =
        stmt
    else {
        return Err(PgError::internal("bind_explain called with a non-EXPLAIN statement"));
    };
    if !matches!(describe_alias, DescribeAlias::Explain) {
        let token = if matches!(describe_alias, DescribeAlias::Desc) { "DESC" } else { "DESCRIBE" };
        return Err(diag::syntax_error_near(token, None));
    }
    if *query_plan {
        return Err(diag::syntax_error_near("QUERY", None));
    }
    if *estimate {
        return Err(diag::syntax_error_near("ESTIMATE", None));
    }
    if format.is_some() {
        return Err(diag::syntax_error_near("FORMAT", None));
    }
    let inner: &Statement = statement.as_ref();
    if !is_explainable(inner) {
        // Statements without a span (e.g. DROP) fall back to the token after the EXPLAIN header.
        let body_pos = b.scratch.get(dispatch::EXPLAIN_BODY_POS_KEY).and_then(|p| p.parse::<u32>().ok());
        let pos = b.pos(inner).or(body_pos);
        return Err(syntax_error_at(b, pos));
    }
    let explain_options = match options {
        Some(list) => parse_explain_options(b, list)?,
        None => {
            // Legacy `EXPLAIN [ANALYZE] [VERBOSE] stmt`: the same defaults as the option list.
            let mut o = ExplainOptions::default();
            o.analyze = *analyze;
            o.verbose = *verbose;
            o.timing = o.analyze;
            o.buffers = o.analyze;
            o.summary = o.analyze;
            o
        }
    };
    let bound = dispatch::bind_inner_statement(b, inner, false)?;
    Ok(BoundStatement::Explain(Box::new(BoundExplain { options: explain_options, statement: Box::new(bound) })))
}

// ---------------------------------------------------------------------------------------------
// LISTEN / UNLISTEN / NOTIFY
// ---------------------------------------------------------------------------------------------

/// LISTEN / UNLISTEN / NOTIFY.
pub fn bind_notify(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    match stmt {
        Statement::LISTEN { channel } => Ok(BoundStatement::Listen(channel_name(b, channel)?)),
        Statement::UNLISTEN { channel } => {
            if channel.quote_style.is_none() && channel.value == "*" {
                Ok(BoundStatement::Unlisten(None))
            } else {
                Ok(BoundStatement::Unlisten(Some(channel_name(b, channel)?)))
            }
        }
        Statement::NOTIFY { channel, payload } => {
            let name = channel_name(b, channel)?;
            if let Some(text) = payload {
                if text.len() > NOTIFY_PAYLOAD_MAX_BYTES {
                    return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "payload string too long"));
                }
            }
            Ok(BoundStatement::Notify { channel: name, payload: payload.clone() })
        }
        _ => Err(PgError::internal("bind_notify called with a non-LISTEN/NOTIFY statement")),
    }
}

// ---------------------------------------------------------------------------------------------
// LOCK TABLE
// ---------------------------------------------------------------------------------------------

fn lock_mode_of(mode: &LockTableMode) -> LockMode {
    match mode {
        LockTableMode::AccessShare => LockMode::AccessShare,
        LockTableMode::RowShare => LockMode::RowShare,
        LockTableMode::RowExclusive => LockMode::RowExclusive,
        LockTableMode::ShareUpdateExclusive => LockMode::ShareUpdateExclusive,
        LockTableMode::Share => LockMode::Share,
        LockTableMode::ShareRowExclusive => LockMode::ShareRowExclusive,
        LockTableMode::Exclusive => LockMode::Exclusive,
        LockTableMode::AccessExclusive => LockMode::AccessExclusive,
    }
}

/// LOCK TABLE. `ONLY` and `*` have no effect: PGArbor has no table inheritance.
pub fn bind_lock(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::Lock(lock) = stmt else {
        return Err(PgError::internal("bind_lock called with a non-LOCK statement"));
    };
    let mode = match &lock.lock_mode {
        None => LockMode::AccessExclusive,
        Some(m) => lock_mode_of(m),
    };
    let mut tables: Vec<Oid> = Vec::with_capacity(lock.tables.len());
    for target in &lock.tables {
        // LockTableCommand uses RangeVarGetRelidExtended(missing_ok = false): errors carry no
        // parse position, and a missing explicit schema is 3F000.
        let qn = b.qual(&target.name)?;
        check_database(b, &qn)?;
        let env: &CatalogEnv = b.env;
        if let Some(s) = qn.schema.as_deref() {
            if s != "pg_temp" && env.lookup_namespace(s).is_none() {
                return Err(PgError::new(SqlState::INVALID_SCHEMA_NAME, format!("schema \"{s}\" does not exist")));
            }
        }
        let not_lockable = |name: &str, plural: &str| {
            PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("cannot lock relation \"{name}\""))
                .with_detail(format!("This operation is not supported for {plural}."))
        };
        match env.lookup_class(qn.schema.as_deref(), &qn.name)? {
            None => {
                let shown = match qn.schema.as_deref() {
                    Some(s) => format!("{s}.{}", qn.name),
                    None => qn.name.clone(),
                };
                return Err(PgError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{shown}\" does not exist")));
            }
            Some(ClassRef::Index(_)) | Some(ClassRef::SystemIndex(_)) => {
                return Err(not_lockable(&qn.name, "indexes"));
            }
            Some(ClassRef::Sequence(_)) => return Err(not_lockable(&qn.name, "sequences")),
            Some(ClassRef::Relation(_)) => {}
        }
        let rel = env.resolve_relation(qn.schema.as_deref(), &qn.name)?.clone();
        match rel.kind {
            RelKind::Table | RelKind::PartitionedTable | RelKind::View => tables.push(rel.oid),
            kind => return Err(not_lockable(&rel.name, kind_plural(kind))),
        }
    }
    Ok(BoundStatement::Lock(BoundLock { tables, mode, nowait: lock.nowait }))
}

// ---------------------------------------------------------------------------------------------
// VACUUM / ANALYZE / REINDEX / CLUSTER / CHECKPOINT
// ---------------------------------------------------------------------------------------------

/// VACUUM / ANALYZE and the extras REINDEX / CLUSTER / CHECKPOINT.
pub fn bind_maintenance(b: &mut Binder, stmt: Option<&Statement>, extra: Option<&ExtraStatement>) -> Result<BoundStatement> {
    if let Some(s) = stmt {
        return match s {
            Statement::Vacuum(v) => bind_vacuum(b, v),
            Statement::Analyze(a) => bind_analyze(b, a),
            _ => Err(PgError::internal("bind_maintenance called with an unexpected statement")),
        };
    }
    match extra {
        Some(ExtraStatement::Reindex { kind, name, concurrently }) => {
            bind_reindex(b, *kind, name.as_ref(), *concurrently)
        }
        Some(ExtraStatement::Cluster { table, index }) => bind_cluster(b, table.as_ref(), index.as_ref()),
        Some(ExtraStatement::Checkpoint) => {
            Ok(BoundStatement::Maintenance { kind: MaintenanceKind::Checkpoint, targets: Vec::new() })
        }
        _ => Err(PgError::internal("bind_maintenance called with an unexpected statement")),
    }
}

/// `VACUUM [FULL] [table]`. Accepted with no physical effect; the table must exist.
fn bind_vacuum(b: &mut Binder, v: &VacuumStatement) -> Result<BoundStatement> {
    let non_pg = if v.sort_only {
        Some("SORT")
    } else if v.delete_only {
        Some("DELETE")
    } else if v.reindex {
        Some("REINDEX")
    } else if v.recluster {
        Some("RECLUSTER")
    } else if v.threshold.is_some() {
        Some("TO")
    } else if v.boost {
        Some("BOOST")
    } else {
        None
    };
    if let Some(token) = non_pg {
        return Err(diag::syntax_error_near(token, None));
    }
    let mut targets: Vec<Oid> = Vec::new();
    if let Some(name) = &v.table_name {
        let (rel, _pos) = resolve_relation(b, name)?;
        if is_table_like(rel.kind) {
            targets.push(rel.oid);
        } else {
            skip_warning(b, &rel.name, "vacuum");
        }
    }
    Ok(BoundStatement::Maintenance { kind: MaintenanceKind::Vacuum, targets })
}

/// `ANALYZE [table [(column, ...)]]`.
fn bind_analyze(b: &mut Binder, a: &Analyze) -> Result<BoundStatement> {
    let hive_token = if a.partitions.is_some() {
        Some("PARTITION")
    } else if a.for_columns {
        Some("FOR")
    } else if a.cache_metadata {
        Some("CACHE")
    } else if a.noscan {
        Some("NOSCAN")
    } else if a.compute_statistics {
        Some("COMPUTE")
    } else if a.has_table_keyword {
        Some("TABLE")
    } else {
        None
    };
    if let Some(token) = hive_token {
        return Err(diag::syntax_error_near(token, None));
    }
    let mut targets: Vec<Oid> = Vec::new();
    if let Some(name) = &a.table_name {
        // PG ExecVacuum reports a missing relation or column without a cursor position.
        let (rel, _pos) = resolve_relation(b, name).map_err(|mut e| {
            e.position = None;
            e
        })?;
        if is_table_like(rel.kind) {
            for id in &a.columns {
                let col = b.ident(id);
                if !rel.columns.iter().any(|c| c.name == col) {
                    return Err(diag::column_of_relation_missing(&col, &rel.name, None));
                }
            }
            targets.push(rel.oid);
        } else {
            skip_warning(b, &rel.name, "analyze");
        }
    }
    Ok(BoundStatement::Maintenance { kind: MaintenanceKind::Analyze, targets })
}

/// Options of `ANALYZE (...)` and `VACUUM (...)` accepted by PG's `ExecVacuum` (names are upper-cased).
const ANALYZE_OPTIONS: &[&str] = &["VERBOSE", "SKIP_LOCKED", "BUFFER_USAGE_LIMIT"];
const VACUUM_OPTIONS: &[&str] = &[
    "ANALYZE",
    "FULL",
    "FREEZE",
    "VERBOSE",
    "SKIP_LOCKED",
    "DISABLE_PAGE_SKIPPING",
    "INDEX_CLEANUP",
    "PROCESS_MAIN",
    "PROCESS_TOAST",
    "TRUNCATE",
    "PARALLEL",
    "SKIP_DATABASE_STATS",
    "ONLY_DATABASE_STATS",
    "BUFFER_USAGE_LIMIT",
];

/// `VACUUM` / `ANALYZE` with an option list or legacy keywords (`ExtraStatement::Maintenance`). Checked the
/// way PG's `ExecVacuum` checks them: option names (42601), `BUFFER_USAGE_LIMIT` (22023), boolean values
/// (42601), then the column list. Maintenance has no physical effect in PGArbor, so an option changes the
/// result only where it would change what the statement does: statistics collection (`ANALYZE` of VACUUM),
/// `VERBOSE` output and `SKIP_LOCKED` are not implemented and are 0A000 (EXE-39).
pub fn bind_maintenance_options(
    b: &mut Binder,
    vacuum: bool,
    options: &[MaintenanceOption],
    table: Option<&ObjectName>,
    columns: &[Ident],
) -> Result<BoundStatement> {
    let word = if vacuum { "VACUUM" } else { "ANALYZE" };
    let allowed = if vacuum { VACUUM_OPTIONS } else { ANALYZE_OPTIONS };
    let mut analyze = !vacuum;
    let mut verbose = false;
    let mut skip_locked = false;
    let mut full = false;
    let mut buffer_set = false;
    let mut freeze = false;
    let mut disable_page_skipping = false;
    let mut process_toast = true;
    let mut only_database_stats = false;
    let mut skip_database_stats = false;
    let mut nworkers: i64 = 0;
    for opt in options {
        let name = opt.name.as_str();
        if !allowed.contains(&name) {
            let shown = opt.name.to_ascii_lowercase();
            return Err(diag::syntax(format!("unrecognized {word} option \"{shown}\""), opt.pos));
        }
        match name {
            "BUFFER_USAGE_LIMIT" => {
                check_buffer_usage_limit(opt)?;
                buffer_set = true;
            }
            "VERBOSE" => verbose = def_get_boolean(opt)?,
            "SKIP_LOCKED" => skip_locked = def_get_boolean(opt)?,
            "ANALYZE" => analyze = def_get_boolean(opt)?,
            "FULL" => full = def_get_boolean(opt)?,
            "FREEZE" => freeze = def_get_boolean(opt)?,
            "DISABLE_PAGE_SKIPPING" => disable_page_skipping = def_get_boolean(opt)?,
            "PROCESS_TOAST" => process_toast = def_get_boolean(opt)?,
            "SKIP_DATABASE_STATS" => skip_database_stats = def_get_boolean(opt)?,
            "ONLY_DATABASE_STATS" => only_database_stats = def_get_boolean(opt)?,
            "PROCESS_MAIN" | "TRUNCATE" => {
                def_get_boolean(opt)?;
            }
            // INDEX_CLEANUP: no value, or AUTO in any case, is auto; any other value is a boolean (vacuum.c
            // ExecVacuum, `get_vacoptval_from_boolean`).
            "INDEX_CLEANUP" => {
                if opt.value.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case("auto")) {
                    def_get_boolean(opt)?;
                }
            }
            // PARALLEL changes no observable result here; the worker count is still validated and kept for the
            // FULL check below.
            "PARALLEL" => nworkers = parallel_workers(opt)?,
            _ => {}
        }
    }
    // PG ExecVacuum: FULL with workers, then FULL with DISABLE_PAGE_SKIPPING or without PROCESS_TOAST.
    if full && nworkers > 0 {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "VACUUM FULL cannot be performed in parallel"));
    }
    if buffer_set && full && !analyze {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL"));
    }
    if full && disable_page_skipping {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL",
        ));
    }
    if full && !process_toast {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "PROCESS_TOAST required with VACUUM FULL"));
    }
    // PG ExecVacuum, ONLY_DATABASE_STATS: no table work, so no table list and no option that changes the
    // table work (ANALYZE, FULL, FREEZE, SKIP_LOCKED, SKIP_DATABASE_STATS). VERBOSE and PROCESS_MAIN/TOAST pass.
    if only_database_stats {
        if table.is_some() {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ONLY_DATABASE_STATS cannot be specified with a list of tables",
            ));
        }
        if analyze || full || freeze || skip_locked || skip_database_stats {
            return Err(PgError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ONLY_DATABASE_STATS cannot be specified with other VACUUM options",
            ));
        }
    }
    if !analyze && !columns.is_empty() {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "ANALYZE option must be specified when a column list is provided",
        ));
    }
    let mut targets: Vec<Oid> = Vec::new();
    if let Some(name) = table {
        // PG ExecVacuum reports a missing relation or column without a cursor position.
        let (rel, _pos) = resolve_relation(b, name).map_err(|mut e| {
            e.position = None;
            e
        })?;
        if is_table_like(rel.kind) {
            for id in columns {
                let col = b.ident(id);
                if !rel.columns.iter().any(|c| c.name == col) {
                    return Err(diag::column_of_relation_missing(&col, &rel.name, None));
                }
            }
            targets.push(rel.oid);
        } else {
            skip_warning(b, &rel.name, if vacuum { "vacuum" } else { "analyze" });
        }
    }
    // Unimplemented output and locking options come after the table and column lookups, as PG's vacuum() does
    // them after ExecVacuum has checked the option values; a missing relation is still reported as 42P01.
    if verbose && !only_database_stats {
        return Err(PgError::unimplemented("EXE-39", "VERBOSE output of VACUUM / ANALYZE"));
    }
    if skip_locked {
        return Err(PgError::unimplemented("EXE-39", "SKIP_LOCKED"));
    }
    if vacuum && analyze {
        return Err(PgError::unimplemented("EXE-39", "VACUUM ANALYZE (column statistics collection)"));
    }
    let kind = if vacuum { MaintenanceKind::Vacuum } else { MaintenanceKind::Analyze };
    Ok(BoundStatement::Maintenance { kind, targets })
}

/// `PARALLEL n` (PG `ExecVacuum`): a value is required, read as an integer (`defGetInt32`) and must lie in
/// 0 through `MAX_PARALLEL_WORKER_LIMIT` (1024). All failures are 42601.
fn parallel_workers(opt: &MaintenanceOption) -> Result<i64> {
    let Some(text) = opt.value.as_deref() else {
        return Err(diag::syntax("parallel option requires a value between 0 and 1024", opt.pos));
    };
    let Ok(n) = text.parse::<i64>() else {
        return Err(diag::syntax("parallel requires an integer value", opt.pos));
    };
    if !(0..=1024).contains(&n) {
        return Err(diag::syntax("parallel workers for vacuum must be between 0 and 1024", opt.pos));
    }
    Ok(n)
}

/// PG `defGetBoolean` (commands/define.c): no value means true; otherwise the integers 1 and 0, or the
/// strings TRUE/FALSE and ON/OFF in any case. Any other value is 42601 `<option> requires a Boolean value`.
fn def_get_boolean(opt: &MaintenanceOption) -> Result<bool> {
    let Some(text) = opt.value.as_deref() else {
        return Ok(true);
    };
    match text.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" => Ok(true),
        "false" | "off" | "0" => Ok(false),
        // PG defGetBoolean: a plain 42601 with no cursor position.
        _ => Err(PgError::new(
            SqlState::SYNTAX_ERROR,
            format!("{} requires a Boolean value", opt.name.to_ascii_lowercase()),
        )),
    }
}

/// `BUFFER_USAGE_LIMIT` (PG `parse_int` with `GUC_UNIT_KB`, then the range check of `ExecVacuum`): 0, or
/// 128 kB through 16 GB (16777216 kB).
fn check_buffer_usage_limit(opt: &MaintenanceOption) -> Result<()> {
    let Some(text) = opt.value.as_deref() else {
        return Err(diag::syntax("BUFFER_USAGE_LIMIT requires a parameter", opt.pos));
    };
    // PG `parse_int` sets a HINT only for an out-of-int-range value (`Value exceeds integer range.`) and for a
    // number followed by an unknown unit (the memory units hint). Text without a number gets none.
    let hint = match buffer_usage_limit_kb(text) {
        Some(0) => return Ok(()),
        Some(kb) if (128..=16_777_216).contains(&kb) => return Ok(()),
        Some(kb) => (kb < i64::from(i32::MIN) || kb > i64::from(i32::MAX)).then_some("Value exceeds integer range."),
        None => {
            let s = text.trim();
            let end = s.find(|c: char| !(c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))).unwrap_or(s.len());
            s[..end].parse::<f64>().is_ok().then_some(MEMORY_UNITS_HINT)
        }
    };
    let err = PgError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB",
    );
    Err(match hint {
        Some(h) => err.with_hint(h),
        None => err,
    })
}

/// PG `memory_units_hint` (guc.c): the HINT for a memory size with an unknown unit.
const MEMORY_UNITS_HINT: &str = "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".";

/// Size in kB of a `parse_int` memory string (a decimal number, then B, kB, MB, GB or TB; no unit means
/// kB), rounded as PG rounds it. None when the text does not parse.
fn buffer_usage_limit_kb(text: &str) -> Option<i64> {
    let s = text.trim();
    let end = s.find(|c: char| !(c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))).unwrap_or(s.len());
    let value: f64 = s[..end].parse().ok()?;
    let factor = match s[end..].trim() {
        "" | "kB" => 1.0,
        "B" => 1.0 / 1024.0,
        "MB" => 1024.0,
        "GB" => 1024.0 * 1024.0,
        "TB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let kb = (value * factor).round();
    if kb.is_finite() { Some(kb as i64) } else { None }
}

/// Index named by `REINDEX INDEX` (42809 when the name is not an index).
fn index_oid(b: &mut Binder, name: &ObjectName) -> Result<Oid> {
    let qn = b.qual(name)?;
    check_database(b, &qn)?;
    let env: &CatalogEnv = b.env;
    match env.resolve_class(qn.schema.as_deref(), &qn.name) {
        Ok(ClassRef::Index(oid)) | Ok(ClassRef::SystemIndex(oid)) => Ok(oid),
        // ExecReindex errors carry no parse position (RangeVar locations are not reported).
        Ok(_) => Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not an index", qn.name))),
        Err(e) => Err(e),
    }
}

/// REINDEX {INDEX|TABLE|SCHEMA|DATABASE|SYSTEM}. Accepted with no physical effect.
fn bind_reindex(b: &mut Binder, kind: ReindexKind, name: Option<&ObjectName>, concurrently: bool) -> Result<BoundStatement> {
    // indexcmds.c `ExecReindex`: system catalogs cannot be rebuilt concurrently.
    if kind == ReindexKind::System && concurrently {
        return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "cannot reindex system catalogs concurrently"));
    }
    let targets: Vec<Oid> = match kind {
        ReindexKind::Index => {
            let name = name.ok_or_else(|| diag::syntax_error_near(";", None))?;
            vec![index_oid(b, name)?]
        }
        ReindexKind::Table => {
            let name = name.ok_or_else(|| diag::syntax_error_near(";", None))?;
            let qn = b.qual(name)?;
            check_database(b, &qn)?;
            let env: &CatalogEnv = b.env;
            let not_table = |n: &str| {
                PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{n}\" is not a table or materialized view"))
            };
            match env.resolve_class(qn.schema.as_deref(), &qn.name)? {
                ClassRef::Index(_) | ClassRef::SystemIndex(_) | ClassRef::Sequence(_) => {
                    return Err(not_table(&qn.name));
                }
                ClassRef::Relation(_) => {}
            }
            let rel = env.resolve_relation(qn.schema.as_deref(), &qn.name)?;
            if !is_table_like(rel.kind) {
                return Err(not_table(&rel.name));
            }
            vec![rel.oid]
        }
        ReindexKind::Schema => {
            let name = name.ok_or_else(|| diag::syntax_error_near(";", None))?;
            let qn = b.qual(name)?;
            // The executor takes relation OIDs (an empty list means every user table), so expand the
            // schema here to its tables and materialized views, in OID order (indexcmds.c
            // `ReindexMultipleTables`). A schema without any such relation is a no-op.
            let ns = b.env.namespace_oid(&qn.name)?;
            let mut tables: Vec<Oid> = b
                .env
                .snapshot()
                .user_relations()
                .filter(|r| r.namespace == ns && matches!(r.kind, RelKind::Table | RelKind::MatView))
                .map(|r| r.oid)
                .collect();
            tables.sort_unstable();
            if tables.is_empty() {
                return Ok(BoundStatement::Noop { tag: "REINDEX".to_string(), notices: Vec::new() });
            }
            tables
        }
        ReindexKind::Database | ReindexKind::System => {
            if let Some(name) = name {
                let qn = b.qual(name)?;
                if qn.name != b.ctx.session.database {
                    return Err(PgError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "can only reindex the currently open database",
                    ));
                }
            }
            Vec::new()
        }
    };
    let kind = if concurrently { MaintenanceKind::ReindexConcurrently } else { MaintenanceKind::Reindex };
    Ok(BoundStatement::Maintenance { kind, targets })
}

/// CLUSTER [table [USING index]]. Plain `CLUSTER table` needs a previously clustered index
/// (indisclustered), else 42704; `targets` = `[table]` or `[table, index]`.
fn bind_cluster(b: &mut Binder, table: Option<&ObjectName>, index: Option<&Ident>) -> Result<BoundStatement> {
    let mut targets: Vec<Oid> = Vec::new();
    match table {
        Some(name) => {
            let (rel, pos) = resolve_relation(b, name)?;
            if !is_table_like(rel.kind) {
                let err = PgError::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table or materialized view", rel.name),
                );
                return Err(diag::with_position(err, pos));
            }
            let env: &CatalogEnv = b.env;
            targets.push(rel.oid);
            match index {
                Some(idx) => {
                    // Looked up on the search path; the index must index this table (cluster.c).
                    let idx_name = b.ident(idx);
                    match env.lookup_class(None, &idx_name)? {
                        Some(ClassRef::Index(oid))
                            if env.snapshot().index(oid).is_some_and(|i| i.table == rel.oid) =>
                        {
                            // `targets[1]` is the index that becomes indisclustered (exec records it).
                            targets.push(oid);
                        }
                        Some(ClassRef::SystemIndex(_)) => {}
                        _ => {
                            return Err(PgError::new(
                                SqlState::UNDEFINED_OBJECT,
                                format!("index \"{idx_name}\" for table \"{}\" does not exist", rel.name),
                            ));
                        }
                    }
                }
                None => {
                    // Plain `CLUSTER table` reuses the index that an earlier CLUSTER / ALTER TABLE
                    // CLUSTER ON marked indisclustered; none means error 42704 (cluster.c).
                    if !env.snapshot().indexes_of(rel.oid).iter().any(|i| i.clustered) {
                        return Err(PgError::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("there is no previously clustered index for table \"{}\"", rel.name),
                        ));
                    }
                }
            }
        }
        None => {
            if let Some(idx) = index {
                return Err(diag::syntax_error_near(&idx.value, ident_pos(b, idx)));
            }
        }
    }
    Ok(BoundStatement::Maintenance { kind: MaintenanceKind::Cluster, targets })
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::BindSession;
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_one(sql: &str) -> Statement {
        Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL parses").remove(0)
    }

    fn context(env: &CatalogEnv) -> BindContext<'_> {
        BindContext {
            env,
            session: BindSession { database: "pgarbor".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        }
    }

    #[test]
    fn explain_body_pos_skips_the_option_header() {
        // `DROP` starts at character 8 (0-based) after `EXPLAIN `, so its 1-based position is 9.
        assert_eq!(explain_body_pos("EXPLAIN DROP TABLE foo", 1), Some(9));
        // Parenthesized options: 8 + 20 characters of `(ANALYZE, COSTS OFF)` + a space.
        assert_eq!(explain_body_pos("EXPLAIN (ANALYZE, COSTS OFF) DROP TABLE foo", 1), Some(30));
        // Legacy words are skipped too, and the start offset is added.
        assert_eq!(explain_body_pos("explain analyze verbose drop table foo", 3), Some(27));
    }

    fn explain_options_of(sql: &str) -> Vec<UtilityOption> {
        match parse_one(sql) {
            Statement::Explain { options: Some(list), .. } => list,
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn execute_arg_count_mismatch_is_42601() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let src = SourceMap::new("execute p(1, 2)");
        let args = vec![BoundExpr::bool_const(true), BoundExpr::bool_const(false)];
        let err = coerce_execute_args(&context(&env), &src, "p", args, &[TypeId::INT4]).err().expect("count error");
        assert_eq!(err.sqlstate.0, *b"42601");
        assert_eq!(err.message, "wrong number of parameters for prepared statement \"p\"");
        assert_eq!(err.detail.as_deref(), Some("Expected 1 parameters but got 2."));
        Ok(())
    }

    #[test]
    fn explain_analyze_json_options() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let sql = "explain (analyze, format json) select 1";
        let src = SourceMap::new(sql);
        let b = Binder::new(&ctx, &src);
        let o = parse_explain_options(&b, &explain_options_of(sql))?;
        assert!(o.analyze);
        assert_eq!(o.format, ExplainFormat::Json);
        assert!(o.timing, "TIMING follows ANALYZE");
        Ok(())
    }

    #[test]
    fn maintenance_boolean_error_has_no_position() {
        let opt = MaintenanceOption { name: "VERBOSE".to_string(), value: Some("maybe".to_string()), pos: Some(10) };
        let err = def_get_boolean(&opt).err().expect("boolean error");
        assert_eq!(err.sqlstate.0, *b"42601");
        assert_eq!(err.message, "verbose requires a Boolean value");
        assert_eq!(err.position, None);
    }

    #[test]
    fn explain_option_errors() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);

        let sql = "explain (foo) select 1";
        let src = SourceMap::new(sql);
        let b = Binder::new(&ctx, &src);
        let err = parse_explain_options(&b, &explain_options_of(sql)).err().expect("unknown option");
        assert_eq!(err.sqlstate.0, *b"42601");
        assert_eq!(err.message, "unrecognized EXPLAIN option \"foo\"");
        assert_eq!(err.position, Some(10));

        let sql = "explain (format bad) select 1";
        let src = SourceMap::new(sql);
        let b = Binder::new(&ctx, &src);
        let err = parse_explain_options(&b, &explain_options_of(sql)).err().expect("bad format");
        assert_eq!(err.sqlstate.0, *b"22023");
        assert_eq!(err.message, "unrecognized value for EXPLAIN option \"format\": \"bad\"");

        let sql = "explain (wal) select 1";
        let src = SourceMap::new(sql);
        let b = Binder::new(&ctx, &src);
        let err = parse_explain_options(&b, &explain_options_of(sql)).err().expect("wal without analyze");
        assert_eq!(err.message, "EXPLAIN option WAL requires ANALYZE");
        Ok(())
    }

    #[test]
    fn deallocate_all_and_named() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let src = SourceMap::new("deallocate all");
        let mut b = Binder::new(&ctx, &src);
        let all = bind_deallocate(&mut b, &parse_one("deallocate all"))?;
        assert_eq!(all, BoundStatement::Deallocate(None));
        let named = bind_deallocate(&mut b, &parse_one("deallocate p"))?;
        assert_eq!(named, BoundStatement::Deallocate(Some("p".to_string())));
        Ok(())
    }

    #[test]
    fn notify_and_unlisten_all() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let src = SourceMap::new("notify ch, 'payload'");
        let mut b = Binder::new(&ctx, &src);
        let out = bind_notify(&mut b, &parse_one("notify ch, 'payload'"))?;
        assert_eq!(out, BoundStatement::Notify { channel: "ch".to_string(), payload: Some("payload".to_string()) });
        let out = bind_notify(&mut b, &parse_one("unlisten *"))?;
        assert_eq!(out, BoundStatement::Unlisten(None));
        Ok(())
    }

    #[test]
    fn lock_missing_table_is_42p01() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let src = SourceMap::new("lock table nosuch in share mode nowait");
        let mut b = Binder::new(&ctx, &src);
        let err = bind_lock(&mut b, &parse_one("lock table nosuch in share mode nowait")).err().expect("missing table");
        assert_eq!(err.sqlstate.0, *b"42P01");
        Ok(())
    }

    #[test]
    fn vacuum_missing_table_is_42p01() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let src = SourceMap::new("vacuum full nosuch");
        let mut b = Binder::new(&ctx, &src);
        let stmt = parse_one("vacuum full nosuch");
        let err = bind_maintenance(&mut b, Some(&stmt), None).err().expect("missing table");
        assert_eq!(err.sqlstate.0, *b"42P01");
        Ok(())
    }

    #[test]
    fn maintenance_option_values_follow_def_get_boolean_and_parse_int() {
        let opt = |v: Option<&str>| MaintenanceOption { name: "TRUNCATE".to_string(), value: v.map(str::to_string), pos: None };
        assert_eq!(def_get_boolean(&opt(None)).ok(), Some(true));
        assert_eq!(def_get_boolean(&opt(Some("OFF"))).ok(), Some(false));
        assert_eq!(def_get_boolean(&opt(Some("1"))).ok(), Some(true));
        assert!(def_get_boolean(&opt(Some("yes"))).is_err());
        assert!(def_get_boolean(&opt(Some("2"))).is_err());
        assert_eq!(buffer_usage_limit_kb("16 GB"), Some(16_777_216));
        assert_eq!(buffer_usage_limit_kb("127 kB"), Some(127));
        assert_eq!(buffer_usage_limit_kb("1 TB"), Some(1_073_741_824));
        assert_eq!(buffer_usage_limit_kb("abc"), None);

        let bul = |v: &str| MaintenanceOption { name: "BUFFER_USAGE_LIMIT".to_string(), value: Some(v.to_string()), pos: None };
        let hint = |v: &str| check_buffer_usage_limit(&bul(v)).err().and_then(|e| e.hint);
        assert!(check_buffer_usage_limit(&bul("128 kB")).is_ok());
        assert_eq!(hint("2 TB").as_deref(), Some("Value exceeds integer range."));
        assert_eq!(hint("1 XB").as_deref(), Some(MEMORY_UNITS_HINT));
        assert_eq!(hint("16777217 kB"), None);
        assert_eq!(hint("abc"), None);
    }

    #[test]
    #[ignore = "needs SQL-04 (dispatch) and SQL-06 (params)"]
    fn prepare_declared_param_type() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = context(&env);
        let sql = "prepare p (int) as select $1";
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        match bind_prepare(&mut b, &parse_one(sql), sql)? {
            BoundStatement::Prepare(p) => assert_eq!(p.param_types, vec![TypeId::INT4]),
            other => panic!("unexpected: {other:?}"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs SQL-04 (dispatch) and SQL-09 (coercion)"]
    fn execute_coerces_text_literal_to_int() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let src = SourceMap::new("execute p('1')");
        let args = vec![BoundExpr::constant(pgarbor_types::Datum::Text("1".to_string()), TypeId::INT4)];
        let out = coerce_execute_args(&context(&env), &src, "p", args, &[TypeId::INT4])?;
        assert_eq!(out.len(), 1);
        Ok(())
    }
}
