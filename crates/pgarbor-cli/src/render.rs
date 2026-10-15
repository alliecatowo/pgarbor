//! Rendering statement results for the terminal and for JSON (lease CLI-11). Shared by `exec`,
//! `shell` and `import` (summaries).
//!
//! ## Values to strings
//! A `StatementResult::Rows{columns, rows, ..}` value is rendered with the session's text output:
//! `Session::encode_value(TypeId{oid: col.type_oid, typmod: col.type_mod}, &datum, FormatCode::Text)`
//! -> `Option<Vec<u8>>` -> `String::from_utf8_lossy`; `None` = NULL -> `None` in [`crate::table::Table`].
//!
//! ## Terminal output of one statement ([`print_result`])
//! * `Rows` + `FormatArg::Table`: `table::render_aligned` (numeric columns by
//!   `table::is_numeric_oid`) followed by a blank line is NOT printed (psql prints none).
//! * `Rows` + `Csv`: `table::render_csv`.
//! * `Rows` + `Json`: one JSON array of objects per statement (`[{"col": value, ...}]`, values per
//!   [`datum_to_json`]); duplicate column names keep the last.
//! * `Command{tag}`: the tag on its own line (`INSERT 0 1`), also for `Empty` (nothing printed).
//! * notices: `NOTICE:  message` lines on stderr via `ctx.err` (psql prints `NOTICE:  ` with TWO
//!   spaces; severity text from `PgError::severity.as_str()`), with `DETAIL:  ` / `HINT:  ` lines.
//! * errors ([`print_error`]): `ERROR:  message` (+ `DETAIL:  `, `HINT:  `, and `LINE 1: ...` +
//!   caret line when `position` is set and the statement text is known) on stderr.
//!
//! ## JSON ([`statement_json`] -> [`crate::reports::ExecStatementJson`])
//! Per module docs of that struct. [`datum_to_json`]: by type oid: bool 16 -> `Value::Bool`;
//! int2 21 / int4 23 / int8 20 -> number (int8 beyond 2^53 stays a number: serde_json
//! `arbitrary_precision` keeps it exact); float4 700 / float8 701 -> number when finite else string
//! (`"NaN"`, `"Infinity"`); json 114 / jsonb 3802 -> parsed embedded JSON (on parse failure the
//! string); everything else -> string. NULL -> `Value::Null`.
use pgarbor::{ColumnInfo, Session, StatementOutput, StatementResult};
use pgarbor_common::PgError;
use pgarbor::Datum;

use crate::cli::FormatArg;
use crate::ctx::{CliError, Ctx};
use crate::reports::ExecStatementJson;
use crate::table::Table;

/// Build a [`Table`] from a rows result (module docs "Values to strings").
#[allow(unused_variables)]
pub fn rows_to_table(session: &Session, columns: &[ColumnInfo], rows: &[Vec<Datum>]) -> Result<Table, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-11", "result to table (render::rows_to_table)")))
}

/// Print one statement's result and notices (module docs).
#[allow(unused_variables)]
pub fn print_result(ctx: &mut Ctx, session: &Session, out: &StatementOutput, fmt: FormatArg) -> Result<(), CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-11", "print a statement result (render::print_result)")))
}

/// Print an error for statement text `sql` (module docs).
#[allow(unused_variables)]
pub fn print_error(ctx: &mut Ctx, sql: &str, err: &PgError) -> Result<(), CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-11", "print an SQL error (render::print_error)")))
}

/// JSON form of one executed statement (success or error).
#[allow(unused_variables)]
pub fn statement_json(
    session: &Session,
    index: usize,
    sql: &str,
    outcome: Result<&StatementOutput, &PgError>,
) -> Result<ExecStatementJson, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-11", "statement JSON (render::statement_json)")))
}

/// One datum as JSON (module docs). `type_oid` is the column's type oid.
#[allow(unused_variables)]
pub fn datum_to_json(session: &Session, col: &ColumnInfo, value: &Datum) -> Result<serde_json::Value, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-11", "datum to JSON (render::datum_to_json)")))
}

/// `StatementResult` helper used by summaries: `Some((rows, tag))` for row results.
pub fn rows_of(r: &StatementResult) -> Option<(&[Vec<Datum>], &str)> {
    match r {
        StatementResult::Rows { rows, tag, .. } => Some((rows.as_slice(), tag.as_str())),
        _ => None,
    }
}
