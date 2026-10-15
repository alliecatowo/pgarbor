//! Transaction control, SET / RESET / SHOW, DISCARD (SQL-40). PG references: `gram.y`
//! (`TransactionStmt`, `VariableSetStmt`, `VariableShowStmt`, `VariableResetStmt`, `DiscardStmt`),
//! `utility.c`, `guc.c` (`ExtractSetVariableArgs`, `flatten_set_variable_args`), `xact.c`.
//!
//! These statements NEVER touch the catalog (they must also work in an aborted transaction).
//!
//! ## Transactions (`TransactionStmt`)
//! * `BEGIN [WORK|TRANSACTION] [modes]`, `START TRANSACTION [modes]` -> `Begin(TxnModes)`; modes:
//!   `ISOLATION LEVEL {READ UNCOMMITTED|READ COMMITTED|REPEATABLE READ|SERIALIZABLE}`, `READ ONLY|READ
//!   WRITE`, `[NOT] DEFERRABLE`, comma separated; sqlparser: `Statement::StartTransaction { modes:
//!   Vec<TransactionMode>, begin, transaction, modifier, statements, exception, has_end_keyword }`
//!   (non-PG modifiers `DEFERRED/IMMEDIATE/EXCLUSIVE`, `TRY` -> 42601).
//! * `COMMIT [WORK|TRANSACTION] [AND [NO] CHAIN]`, `END` -> `Commit{chain}`; `ROLLBACK/ABORT [AND [NO]
//!   CHAIN]` -> `Rollback{chain}`; `ROLLBACK [WORK] TO [SAVEPOINT] n` -> `RollbackToSavepoint(n)`;
//!   `SAVEPOINT n` -> `Savepoint(n)`; `RELEASE [SAVEPOINT] n` -> `ReleaseSavepoint(n)`. Savepoint names
//!   folded as identifiers. `COMMIT PREPARED`/`PREPARE TRANSACTION` are `ExtraStatement::Unsupported`.
//! * `SET TRANSACTION modes` -> `SetTransaction(TxnModes)`; `SET TRANSACTION SNAPSHOT 'id'` 0A000;
//!   `SET SESSION CHARACTERISTICS AS TRANSACTION modes` -> `SetSessionCharacteristics`.
//!
//! ## SET (`Statement::Set(Set::...)`)
//! * `SET [SESSION|LOCAL] name {TO|=} v1[, v2...]` (`Set::SingleAssignment { scope, variable, values }`)
//!   -> `Set(SetStmt { scope, name, value })`. `name` = qualified GUC name (`app.setting`) lower-cased;
//!   each value rendered per `flatten_set_variable_args`: string literal -> its contents; identifier ->
//!   as written folded? (PG keeps the identifier text with case as written for `SET x = Foo`? -> it stores
//!   `Foo`; quoted identifiers keep case, unquoted are folded by the lexer), numeric literal -> the literal
//!   text, `TRUE|FALSE|ON|OFF` -> `on`/`off`/`true`/`false` words as written, `DEFAULT` -> `SetValue::Default`,
//!   `SET x FROM CURRENT` -> `FromCurrent`. Multiple values form a list (`SET search_path = a, "B"` ->
//!   `["a","B"]`). `SET x TO` with no value: 42601.
//! * Special forms normalized: `SET TIME ZONE v` (`Set::SetTimeZone { local, value }`) -> `timezone`
//!   (`LOCAL`/`DEFAULT` -> Default; `INTERVAL '1 hour'`, numbers -> text), `SET NAMES 'x'` ->
//!   `client_encoding`, `SET SCHEMA 'x'` -> `search_path`, `SET SESSION AUTHORIZATION u|DEFAULT` ->
//!   `SetSessionAuthorization`, `SET [LOCAL|SESSION] ROLE r|NONE` -> `SetRole`, `RESET ROLE` ->
//!   `SetRole{None}`, `RESET SESSION AUTHORIZATION` -> `SetSessionAuthorization{None}`.
//! * `RESET name|ALL` -> `Reset(...)`. Unknown GUC names are validated by the SESSION at execution
//!   (42704 `unrecognized configuration parameter "x"`), not here, except placeholder names `a.b` which
//!   are always allowed. Non-PG forms (`Set::MultipleAssignments`, `ParenthesizedAssignments`,
//!   `SetSessionParam`, hivevar, `SET @x`): 42601.
//! * `SET CONSTRAINTS ALL|names DEFERRED|IMMEDIATE` (extra) -> `Noop { tag: "SET CONSTRAINTS" }` in v1 for
//!   `ALL IMMEDIATE`; `DEFERRED` is 0A000 until deferred constraints exist.
//!
//! ## SHOW (`Statement::ShowVariable { variable: Vec<Ident> }`)
//! `SHOW name` -> `Show(Some(name))` (qualified names joined with `.`, lower-cased; `SHOW ALL` ->
//! `Show(None)`; special spellings: `SHOW TIME ZONE` -> `timezone`, `SHOW TRANSACTION ISOLATION LEVEL` ->
//! `transaction_isolation`, `SHOW SESSION AUTHORIZATION` -> `session_authorization`). Unknown names are
//! the session's 42704. Other `Show*` statements are classified as foreign syntax before reaching here.
//!
//! ## DISCARD (`Statement::Discard { object_type }`) -> `Discard(DiscardKind::{All,Plans,Sequences,Temp})`
//! (`DISCARD ALL` inside a transaction block is the session's 25001).
use pgarbor_common::{PgError, Result};
use sqlparser::ast::{
    BeginTransactionKind, ContextModifier, DiscardObject, Expr, Ident, ObjectName, ObjectNamePart, Reset,
    Set, SetSessionAuthorizationParamKind, Statement, TransactionAccessMode, TransactionIsolationLevel,
    TransactionMode, TransactionModifier, UnaryOperator, Value,
};

use super::context::Binder;
use crate::diag;
use crate::ir::{
    BoundStatement, DiscardKind, IsolationLevel, SetScope, SetStmt, SetValue, TransactionStmt, TxnModes,
};

/// Outcome of rendering one `SET` argument: a value text, or the `DEFAULT` keyword (which PG turns
/// into a reset of the variable, `flatten_set_variable_args` returning NULL for an empty list).
enum Arg {
    Text(String),
    Default,
}

/// Scope of a `SET`: `None` and `SESSION` are session scope, `LOCAL` is transaction scope. `GLOBAL`
/// is not PG syntax (`syntax error at or near "GLOBAL"`).
fn scope_of(modifier: Option<&ContextModifier>, pos: Option<u32>) -> Result<SetScope> {
    match modifier {
        None | Some(ContextModifier::Session) => Ok(SetScope::Session),
        Some(ContextModifier::Local) => Ok(SetScope::Local),
        Some(ContextModifier::Global) => Err(diag::syntax_error_near("GLOBAL", pos)),
    }
}

/// Dotted, folded GUC name (`app.setting`). Each part goes through `Binder::ident` (fold + 63-byte
/// truncation NOTICE), like PG's `var_name: ColId | var_name '.' ColId`.
fn object_name_text(b: &mut Binder, name: &ObjectName, pos: Option<u32>) -> Result<String> {
    let mut out = String::new();
    for (i, part) in name.0.iter().enumerate() {
        match part {
            ObjectNamePart::Identifier(id) => {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(&b.ident(id));
            }
            ObjectNamePart::Function(_) => return Err(diag::syntax("syntax error", pos)),
        }
    }
    if out.is_empty() {
        return Err(diag::syntax("syntax error", pos));
    }
    Ok(out)
}

/// True when `name` is exactly the unquoted single word `word` (case-insensitive).
fn is_bare_word(name: &ObjectName, word: &str) -> bool {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(id)] => id.quote_style.is_none() && id.value.eq_ignore_ascii_case(word),
        _ => false,
    }
}

/// True when `parts` is exactly the unquoted word sequence `words` (case-insensitive).
fn words_match(parts: &[Ident], words: &[&str]) -> bool {
    parts.len() == words.len()
        && parts
            .iter()
            .zip(words.iter())
            .all(|(id, w)| id.quote_style.is_none() && id.value.eq_ignore_ascii_case(w))
}

/// Render a numeric or string literal the way `SET` stores it. Integers and numerics keep the text
/// the client wrote; string literals give their contents; `TRUE`/`FALSE` give `true`/`false`.
fn render_value(v: &Value, pos: Option<u32>) -> Result<String> {
    match v {
        Value::Number(n, _) => Ok(n.to_string()),
        Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => Ok(s.clone()),
        Value::Boolean(true) => Ok("true".to_string()),
        Value::Boolean(false) => Ok("false".to_string()),
        _ => Err(diag::syntax("syntax error", pos)),
    }
}

/// Render one `SET` argument (PG `var_value` / `zone_value` / `NumericOnly`).
///
/// * bare identifiers are folded like any identifier (`Foo` -> `foo`, `"Foo"` stays `Foo`);
/// * unquoted `DEFAULT` gives [`Arg::Default`]; with `zone` (SET TIME ZONE) so does unquoted `LOCAL`;
/// * `+n` / `-n` numeric literals keep their sign;
/// * anything else is a syntax error (PG's grammar accepts no expressions here).
fn render_arg(b: &mut Binder, expr: &Expr, zone: bool) -> Result<Arg> {
    match expr {
        Expr::Identifier(id) => {
            let bare = id.quote_style.is_none();
            if bare && id.value.eq_ignore_ascii_case("default") {
                return Ok(Arg::Default);
            }
            if zone && bare && id.value.eq_ignore_ascii_case("local") {
                return Ok(Arg::Default);
            }
            Ok(Arg::Text(b.ident(id)))
        }
        Expr::Value(v) => Ok(Arg::Text(render_value(&v.value, b.pos(expr))?)),
        Expr::UnaryOp { op, expr: inner } if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus) => {
            let sign = if matches!(op, UnaryOperator::Minus) { "-" } else { "" };
            match &**inner {
                Expr::Value(v) => match &v.value {
                    Value::Number(n, _) => Ok(Arg::Text(format!("{sign}{n}"))),
                    _ => Err(diag::syntax("syntax error", b.pos(expr))),
                },
                _ => Err(diag::syntax("syntax error", b.pos(expr))),
            }
        }
        // `SET TIME ZONE INTERVAL '...'` needs PG's interval normalization (`interval_in` then
        // `interval_out`), which this engine does not have yet.
        Expr::Interval(_) if zone => Err(PgError::unimplemented(
            "SQL-40",
            "SET TIME ZONE with an INTERVAL value (interval normalization)",
        )),
        _ => Err(diag::syntax("syntax error", b.pos(expr))),
    }
}

/// The value of a `SET` (one or several arguments). A single `DEFAULT` is [`SetValue::Default`];
/// several arguments form a list, and `DEFAULT` inside a list is a syntax error.
fn set_value(b: &mut Binder, values: &[Expr], zone: bool, pos: Option<u32>) -> Result<SetValue> {
    match values {
        [] => Err(diag::syntax("syntax error", pos)),
        [single] => match render_arg(b, single, zone)? {
            Arg::Default => Ok(SetValue::Default),
            Arg::Text(t) => Ok(SetValue::Values(vec![t])),
        },
        many => {
            let mut out = Vec::with_capacity(many.len());
            for e in many {
                match render_arg(b, e, false)? {
                    Arg::Text(t) => out.push(t),
                    Arg::Default => return Err(diag::syntax("syntax error", b.pos(e))),
                }
            }
            Ok(SetValue::Values(out))
        }
    }
}

/// Transaction characteristics. PG applies the items in order, so the last one given wins
/// (`BEGIN READ ONLY, READ WRITE` is read-write). `SNAPSHOT` is not a PG isolation level.
fn txn_modes(modes: &[TransactionMode], pos: Option<u32>) -> Result<TxnModes> {
    let mut out = TxnModes::default();
    for mode in modes {
        match mode {
            TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => out.read_only = Some(true),
            TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => out.read_only = Some(false),
            TransactionMode::IsolationLevel(level) => {
                let iso = match level {
                    TransactionIsolationLevel::ReadUncommitted => IsolationLevel::ReadUncommitted,
                    TransactionIsolationLevel::ReadCommitted => IsolationLevel::ReadCommitted,
                    TransactionIsolationLevel::RepeatableRead => IsolationLevel::RepeatableRead,
                    TransactionIsolationLevel::Serializable => IsolationLevel::Serializable,
                    TransactionIsolationLevel::Snapshot => return Err(diag::syntax_error_near("SNAPSHOT", pos)),
                };
                out.isolation = Some(iso);
            }
        }
    }
    Ok(out)
}

/// Non-PG modifiers (`DEFERRED`, `IMMEDIATE`, `EXCLUSIVE`, `TRY`, `CATCH`) are syntax errors.
fn reject_modifier(modifier: &Option<TransactionModifier>, pos: Option<u32>) -> Result<()> {
    match modifier {
        None => Ok(()),
        Some(m) => Err(diag::syntax_error_near(&m.to_string(), pos)),
    }
}

/// BEGIN / START TRANSACTION / COMMIT / END / ROLLBACK / ABORT / SAVEPOINT / RELEASE.
pub fn bind_transaction(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let pos = b.pos(stmt);
    match stmt {
        Statement::StartTransaction { modes, begin, transaction, modifier, statements, exception, has_end_keyword } => {
            // BEGIN ... END blocks (scripting) are not PG transaction syntax.
            if !statements.is_empty() || exception.is_some() || *has_end_keyword {
                return Err(PgError::syntax("syntax error", pos));
            }
            reject_modifier(modifier, pos)?;
            match transaction {
                Some(BeginTransactionKind::Tran) => return Err(diag::syntax_error_near("TRAN", pos)),
                Some(BeginTransactionKind::Work) if !*begin => return Err(diag::syntax_error_near("WORK", pos)),
                _ => {}
            }
            let modes = txn_modes(modes, pos)?;
            Ok(BoundStatement::Transaction(TransactionStmt::Begin(modes)))
        }
        // END is COMMIT (`end` only records the spelling).
        Statement::Commit { chain, modifier, .. } => {
            reject_modifier(modifier, pos)?;
            Ok(BoundStatement::Transaction(TransactionStmt::Commit { chain: *chain }))
        }
        Statement::Rollback { chain, savepoint } => match savepoint {
            None => Ok(BoundStatement::Transaction(TransactionStmt::Rollback { chain: *chain })),
            Some(name) => {
                // PG: `ROLLBACK [WORK] TO [SAVEPOINT] name` takes no AND CHAIN.
                if *chain {
                    return Err(diag::syntax_error_near("AND", pos));
                }
                let name = b.ident(name);
                Ok(BoundStatement::Transaction(TransactionStmt::RollbackToSavepoint(name)))
            }
        },
        Statement::Savepoint { name } => {
            let name = b.ident(name);
            Ok(BoundStatement::Transaction(TransactionStmt::Savepoint(name)))
        }
        Statement::ReleaseSavepoint { name } => {
            let name = b.ident(name);
            Ok(BoundStatement::Transaction(TransactionStmt::ReleaseSavepoint(name)))
        }
        _ => Err(PgError::internal("bind_transaction called with a non-transaction statement")),
    }
}

/// SET (all forms, including `SET TRANSACTION`) and RESET.
pub fn bind_set(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let pos = b.pos(stmt);
    match stmt {
        Statement::Set(set) => bind_set_stmt(b, set, pos),
        Statement::Reset(reset) => bind_reset(b, &reset.reset, pos),
        _ => Err(PgError::internal("bind_set called with a non-SET statement")),
    }
}

fn bind_set_stmt(b: &mut Binder, set: &Set, pos: Option<u32>) -> Result<BoundStatement> {
    match set {
        Set::SingleAssignment { scope, hivevar, variable, values } => {
            if *hivevar {
                return Err(diag::syntax("syntax error", pos));
            }
            let scope = scope_of(scope.as_ref(), pos)?;
            let name = object_name_text(b, variable, pos)?;
            let value = set_value(b, values, false, pos)?;
            Ok(BoundStatement::Set(SetStmt { scope, name, value }))
        }
        // `SET TIME ZONE v` is `SET timezone TO v`; LOCAL/DEFAULT reset it.
        Set::SetTimeZone { local, value } => {
            let scope = if *local { SetScope::Local } else { SetScope::Session };
            let value = match render_arg(b, value, true)? {
                Arg::Default => SetValue::Default,
                Arg::Text(t) => SetValue::Values(vec![t]),
            };
            Ok(BoundStatement::Set(SetStmt { scope, name: "timezone".to_string(), value }))
        }
        // `SET NAMES enc` is `SET client_encoding TO enc` (PG has no COLLATE here).
        Set::SetNames { charset_name, collation_name } => {
            if collation_name.is_some() {
                return Err(diag::syntax_error_near("COLLATE", pos));
            }
            let enc = b.ident(charset_name);
            Ok(BoundStatement::Set(SetStmt {
                scope: SetScope::Session,
                name: "client_encoding".to_string(),
                value: SetValue::Values(vec![enc]),
            }))
        }
        Set::SetNamesDefault {} => Ok(BoundStatement::Set(SetStmt {
            scope: SetScope::Session,
            name: "client_encoding".to_string(),
            value: SetValue::Default,
        })),
        Set::SetRole { context_modifier, role_name } => {
            let local = scope_of(context_modifier.as_ref(), pos)? == SetScope::Local;
            let role = role_name.as_ref().map(|r| b.ident(r));
            Ok(BoundStatement::SetRole { role, local })
        }
        Set::SetSessionAuthorization(param) => {
            let local = scope_of(Some(&param.scope), pos)? == SetScope::Local;
            let user = match &param.kind {
                SetSessionAuthorizationParamKind::Default => None,
                SetSessionAuthorizationParamKind::User(id) => Some(b.ident(id)),
            };
            Ok(BoundStatement::SetSessionAuthorization { user, local })
        }
        Set::SetTransaction { modes, snapshot, session } => {
            if snapshot.is_some() {
                return Err(PgError::unimplemented("SQL-40", "SET TRANSACTION SNAPSHOT"));
            }
            if modes.is_empty() {
                return Err(PgError::syntax("syntax error at end of input", None));
            }
            let modes = txn_modes(modes, pos)?;
            if *session {
                Ok(BoundStatement::Transaction(TransactionStmt::SetSessionCharacteristics(modes)))
            } else {
                Ok(BoundStatement::Transaction(TransactionStmt::SetTransaction(modes)))
            }
        }
        Set::ParenthesizedAssignments { .. } | Set::MultipleAssignments { .. } | Set::SetSessionParam(_) => {
            Err(diag::syntax("syntax error", pos))
        }
    }
}

/// RESET name | ALL, plus the RESET forms that map onto other statements.
fn bind_reset(b: &mut Binder, reset: &Reset, pos: Option<u32>) -> Result<BoundStatement> {
    match reset {
        Reset::ALL => Ok(BoundStatement::Reset(None)),
        Reset::SessionAuthorization => Ok(BoundStatement::SetSessionAuthorization { user: None, local: false }),
        Reset::ConfigurationParameter(name) => {
            if is_bare_word(name, "role") {
                return Ok(BoundStatement::SetRole { role: None, local: false });
            }
            let name = object_name_text(b, name, pos)?;
            Ok(BoundStatement::Reset(Some(name)))
        }
    }
}

/// SHOW.
pub fn bind_show(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let Statement::ShowVariable { variable } = stmt else {
        return Err(PgError::internal("bind_show called with a non-SHOW statement"));
    };
    if variable.len() == 1 && variable[0].quote_style.is_none() && variable[0].value.eq_ignore_ascii_case("all") {
        return Ok(BoundStatement::Show(None));
    }
    // Special spellings of PG's SHOW grammar.
    if words_match(variable, &["time", "zone"]) {
        return Ok(BoundStatement::Show(Some("timezone".to_string())));
    }
    if words_match(variable, &["transaction", "isolation", "level"]) {
        return Ok(BoundStatement::Show(Some("transaction_isolation".to_string())));
    }
    if words_match(variable, &["session", "authorization"]) {
        return Ok(BoundStatement::Show(Some("session_authorization".to_string())));
    }
    if variable.is_empty() {
        return Err(PgError::syntax("syntax error", b.pos(stmt)));
    }
    let mut parts = Vec::with_capacity(variable.len());
    for id in variable {
        parts.push(b.ident(id));
    }
    Ok(BoundStatement::Show(Some(parts.join("."))))
}

/// DISCARD.
pub fn bind_discard(b: &mut Binder, stmt: &Statement) -> Result<BoundStatement> {
    let _ = b;
    match stmt {
        Statement::Discard { object_type } => {
            let kind = match object_type {
                DiscardObject::ALL => DiscardKind::All,
                DiscardObject::PLANS => DiscardKind::Plans,
                DiscardObject::SEQUENCES => DiscardKind::Sequences,
                DiscardObject::TEMP => DiscardKind::Temp,
            };
            Ok(BoundStatement::Discard(kind))
        }
        _ => Err(PgError::internal("bind_discard called with a non-DISCARD statement")),
    }
}

/// `SET CONSTRAINTS` (from `ExtraStatement::SetConstraints`).
///
/// `ALL IMMEDIATE` is accepted as a no-op (every constraint is already immediate). `DEFERRED` needs
/// deferrable constraints, which do not exist yet, and naming constraints needs a catalog lookup
/// (42704) that this binder does not do; both are reported as unsupported, never silently ignored.
pub fn bind_set_constraints(b: &mut Binder, names: Option<&[ObjectName]>, deferred: bool) -> Result<BoundStatement> {
    let _ = b;
    if deferred {
        return Err(PgError::unimplemented("SQL-40", "SET CONSTRAINTS ... DEFERRED"));
    }
    if names.is_some() {
        return Err(PgError::unimplemented("SQL-40", "SET CONSTRAINTS with constraint names"));
    }
    Ok(BoundStatement::Noop { tag: "SET CONSTRAINTS".to_string(), notices: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_one(sql: &str) -> Statement {
        let mut v = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL must parse");
        v.remove(0)
    }

    /// Run `f` with a binder over a bootstrapped catalog (these statements never read it).
    fn with_binder<R>(sql: &str, f: impl FnOnce(&mut Binder) -> R) -> Result<R> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let src = SourceMap::new(sql);
        let mut b = Binder::new(&ctx, &src);
        Ok(f(&mut b))
    }

    fn run(sql: &str, f: fn(&mut Binder, &Statement) -> Result<BoundStatement>) -> Result<BoundStatement> {
        let stmt = parse_one(sql);
        with_binder(sql, |b| f(b, &stmt))?
    }

    fn txn(s: TransactionStmt) -> BoundStatement {
        BoundStatement::Transaction(s)
    }

    #[test]
    fn begin_isolation_and_access_mode() {
        let got = run("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY", bind_transaction).expect("binds");
        let want = txn(TransactionStmt::Begin(TxnModes {
            isolation: Some(IsolationLevel::Serializable),
            read_only: Some(true),
            deferrable: None,
        }));
        assert_eq!(got, want);
    }

    #[test]
    fn commit_and_chain() {
        let got = run("COMMIT AND CHAIN", bind_transaction).expect("binds");
        assert_eq!(got, txn(TransactionStmt::Commit { chain: true }));
    }

    #[test]
    fn rollback_to_savepoint_folds_name() {
        let got = run("ROLLBACK TO SAVEPOINT s", bind_transaction).expect("binds");
        assert_eq!(got, txn(TransactionStmt::RollbackToSavepoint("s".to_string())));
    }

    #[test]
    fn savepoint_and_release() {
        let got = run("SAVEPOINT sp1", bind_transaction).expect("binds");
        assert_eq!(got, txn(TransactionStmt::Savepoint("sp1".to_string())));
        let got = run("RELEASE SAVEPOINT sp1", bind_transaction).expect("binds");
        assert_eq!(got, txn(TransactionStmt::ReleaseSavepoint("sp1".to_string())));
    }

    #[test]
    fn begin_non_pg_modifier_is_syntax_error() {
        // sqlparser's PostgreSQL dialect rejects the SQLite-style modifier while parsing, before the
        // binder sees it. PG answers 42601 as well ("syntax error at or near \"DEFERRED\""), so the
        // check is end-to-end through the parser.
        let err = crate::parser::parse("BEGIN DEFERRED").err().expect("must fail");
        assert_eq!(err.sqlstate.code(), "42601");
    }

    #[test]
    fn set_search_path_list() {
        let got = run("SET search_path = public, \"X\"", bind_set).expect("binds");
        let want = BoundStatement::Set(SetStmt {
            scope: SetScope::Session,
            name: "search_path".to_string(),
            value: SetValue::Values(vec!["public".to_string(), "X".to_string()]),
        });
        assert_eq!(got, want);
    }

    #[test]
    fn set_time_zone_string() {
        let got = run("SET TIME ZONE 'UTC'", bind_set).expect("binds");
        let want = BoundStatement::Set(SetStmt {
            scope: SetScope::Session,
            name: "timezone".to_string(),
            value: SetValue::Values(vec!["UTC".to_string()]),
        });
        assert_eq!(got, want);
    }

    #[test]
    fn set_local_number() {
        let got = run("SET LOCAL x = 1", bind_set).expect("binds");
        let want = BoundStatement::Set(SetStmt {
            scope: SetScope::Local,
            name: "x".to_string(),
            value: SetValue::Values(vec!["1".to_string()]),
        });
        assert_eq!(got, want);
    }

    #[test]
    fn set_session_authorization_default_and_role_none() {
        let got = run("SET SESSION AUTHORIZATION DEFAULT", bind_set).expect("binds");
        assert_eq!(got, BoundStatement::SetSessionAuthorization { user: None, local: false });
        let got = run("SET ROLE NONE", bind_set).expect("binds");
        assert_eq!(got, BoundStatement::SetRole { role: None, local: false });
    }

    #[test]
    fn reset_role_and_all() {
        let got = run("RESET ROLE", bind_set).expect("binds");
        assert_eq!(got, BoundStatement::SetRole { role: None, local: false });
        let got = run("RESET ALL", bind_set).expect("binds");
        assert_eq!(got, BoundStatement::Reset(None));
    }

    #[test]
    fn set_transaction_snapshot_is_unsupported() {
        let err = run("SET TRANSACTION SNAPSHOT '00000003-0000001B-1'", bind_set).err().expect("must fail");
        assert_eq!(err.sqlstate.code(), "0A000");
    }

    #[test]
    fn show_all_and_time_zone() {
        let got = run("SHOW ALL", bind_show).expect("binds");
        assert_eq!(got, BoundStatement::Show(None));
        let got = run("SHOW TIME ZONE", bind_show).expect("binds");
        assert_eq!(got, BoundStatement::Show(Some("timezone".to_string())));
    }

    #[test]
    fn discard_all() {
        let got = run("DISCARD ALL", bind_discard).expect("binds");
        assert_eq!(got, BoundStatement::Discard(DiscardKind::All));
    }

    #[test]
    fn set_constraints_all_immediate_is_noop() {
        let got = with_binder("SET CONSTRAINTS ALL IMMEDIATE", |b| bind_set_constraints(b, None, false))
            .expect("context builds")
            .expect("binds");
        assert_eq!(got, BoundStatement::Noop { tag: "SET CONSTRAINTS".to_string(), notices: Vec::new() });
    }

    #[test]
    fn set_constraints_deferred_is_unsupported() {
        let err = with_binder("SET CONSTRAINTS ALL DEFERRED", |b| bind_set_constraints(b, None, true))
            .expect("context builds")
            .err()
            .expect("must fail");
        assert_eq!(err.sqlstate.code(), "0A000");
    }
}
