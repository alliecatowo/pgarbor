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
use pgarbor::{ColumnInfo, FormatCode, Session, StatementOutput, StatementResult, TypeId};
use pgarbor_common::PgError;
use pgarbor::Datum;

use crate::cli::FormatArg;
use crate::ctx::{CliError, Ctx};
use crate::reports::{ErrorJson, ExecColumnJson, ExecStatementJson};
use crate::table::{self, Table};
use std::io::Write;

/// Build a [`Table`] from a rows result (module docs "Values to strings").
pub fn rows_to_table(session: &Session, columns: &[ColumnInfo], rows: &[Vec<Datum>]) -> Result<Table, CliError> {
    let mut table = Table { headers: columns.iter().map(|c| c.name.clone()).collect(), rows: Vec::with_capacity(rows.len()), right_align: columns.iter().map(|c| table::is_numeric_oid(c.type_oid)).collect() };
    for row in rows {
        if row.len() != columns.len() { return Err(CliError::internal("row column count does not match result description")); }
        let mut rendered = Vec::with_capacity(row.len());
        for (col, value) in columns.iter().zip(row) {
            let encoded = session.encode_value(TypeId { oid: col.type_oid, typmod: col.type_mod }, value, FormatCode::Text).map_err(CliError::from)?;
            rendered.push(encoded.map(|v| String::from_utf8_lossy(&v).into_owned()));
        }
        table.rows.push(rendered);
    }
    Ok(table)
}

/// Print one statement's result and notices (module docs).
pub fn print_result(ctx: &mut Ctx, session: &Session, out: &StatementOutput, fmt: FormatArg) -> Result<(), CliError> {
    for notice in &out.notices {
        let _ = writeln!(ctx.err, "{}:  {}", notice.severity.as_str(), notice.message);
        if let Some(detail) = &notice.detail { let _ = writeln!(ctx.err, "DETAIL:  {detail}"); }
        if let Some(hint) = &notice.hint { let _ = writeln!(ctx.err, "HINT:  {hint}"); }
    }
    match &out.result {
        StatementResult::Empty => {}
        StatementResult::Command { tag } => ctx.out_line(tag),
        StatementResult::Rows { columns, rows, .. } => match fmt {
            FormatArg::Table => { let t = rows_to_table(session, columns, rows)?; let s = table::render_aligned(&t, true)?; if !s.is_empty() { for line in s.split_terminator('\n') { ctx.out_line(line); } } }
            FormatArg::Csv => { let t = rows_to_table(session, columns, rows)?; let s = table::render_csv(&t)?; for line in s.split_terminator('\n') { ctx.out_line(line); } }
            FormatArg::Json => {
                let mut objects = Vec::with_capacity(rows.len());
                for row in rows {
                    let mut object = serde_json::Map::new();
                    for (col, value) in columns.iter().zip(row) { object.insert(col.name.clone(), datum_to_json(session, col, value)?); }
                    objects.push(serde_json::Value::Object(object));
                }
                let value = serde_json::to_string(&objects).map_err(|e| CliError::internal(format!("json serialization failed: {e}")))?;
                ctx.out_line(&value);
            }
        },
    }
    let _ = ctx.err.flush();
    Ok(())
}

/// Print an error for statement text `sql` (module docs).
pub fn print_error(ctx: &mut Ctx, sql: &str, err: &PgError) -> Result<(), CliError> {
    let _ = writeln!(ctx.err, "{}:  {}", err.severity.as_str(), err.message);
    if let Some(detail) = &err.detail { let _ = writeln!(ctx.err, "DETAIL:  {detail}"); }
    if let Some(hint) = &err.hint { let _ = writeln!(ctx.err, "HINT:  {hint}"); }
    if let Some(pos) = err.position {
        if let Some((line, caret)) = caret_line(sql, pos) {
            let line_no = sql.chars().take(pos.saturating_sub(1) as usize).filter(|c| *c == '\n').count() + 1;
            let _ = writeln!(ctx.err, "LINE {line_no}: {line}");
            let _ = writeln!(ctx.err, "{}{}^", " ".repeat(7 + line_no.to_string().len()), caret);
        }
    }
    let _ = ctx.err.flush();
    Ok(())
}

fn caret_line(sql: &str, position: u32) -> Option<(String, String)> {
    if position == 0 { return None; }
    let before = sql.chars().take(position as usize - 1).collect::<String>();
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = sql[ line_start.. ].split('\n').next()?.trim_end_matches('\r').to_string();
    let column = before.rsplit('\n').next().unwrap_or("").chars().count();
    Some((line, " ".repeat(column)))
}

/// JSON form of one executed statement (success or error).
pub fn statement_json(
    session: &Session,
    index: usize,
    sql: &str,
    outcome: Result<&StatementOutput, &PgError>,
) -> Result<ExecStatementJson, CliError> {
    match outcome {
        Err(e) => Ok(ExecStatementJson { index, sql: sql.to_string(), tag: None, columns: None, rows: None, notices: Vec::new(), error: Some(ErrorJson { sqlstate: e.sqlstate.code().to_string(), message: e.message.clone(), detail: e.detail.clone(), hint: e.hint.clone(), position: e.position }) }),
        Ok(output) => {
            let (tag, columns, rows) = match &output.result {
                StatementResult::Empty => (None, None, None),
                StatementResult::Command { tag } => (Some(tag.clone()), None, None),
                StatementResult::Rows { columns, rows, tag } => {
                    let cols = columns.iter().map(|c| ExecColumnJson { name: c.name.clone(), type_oid: c.type_oid }).collect();
                    let mut json_rows = Vec::with_capacity(rows.len());
                    for row in rows { let mut values = Vec::with_capacity(row.len()); for (c, v) in columns.iter().zip(row) { values.push(datum_to_json(session, c, v)?); } json_rows.push(values); }
                    (Some(tag.clone()), Some(cols), Some(json_rows))
                }
            };
            Ok(ExecStatementJson { index, sql: sql.to_string(), tag, columns, rows, notices: output.notices.iter().map(|n| n.message.clone()).collect(), error: None })
        }
    }
}

/// One datum as JSON (module docs). `type_oid` is the column's type oid.
pub fn datum_to_json(session: &Session, col: &ColumnInfo, value: &Datum) -> Result<serde_json::Value, CliError> {
    if matches!(value, Datum::Null) { return Ok(serde_json::Value::Null); }
    let encoded = session.encode_value(TypeId { oid: col.type_oid, typmod: col.type_mod }, value, FormatCode::Text).map_err(CliError::from)?;
    let text = String::from_utf8_lossy(encoded.as_deref().unwrap_or_default()).into_owned();
    Ok(json_for_text(col.type_oid, &text))
}

fn json_for_text(oid: u32, text: &str) -> serde_json::Value {
    use serde_json::Value;
    match oid {
        16 => Value::Bool(text == "t" || text == "true"),
        21 | 23 => text.parse::<i64>().ok().map(|n| Value::Number(n.into())).unwrap_or_else(|| Value::String(text.to_string())),
        20 => text.parse::<i64>().ok().map(|n| Value::Number(n.into())).unwrap_or_else(|| Value::String(text.to_string())),
        700 | 701 => match text { "NaN" | "Infinity" | "-Infinity" => Value::String(text.to_string()), _ => text.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or_else(|| Value::String(text.to_string())) },
        114 | 3802 => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())),
        _ => Value::String(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_values_follow_oid_classes() {
        assert_eq!(json_for_text(16, "t"), serde_json::json!(true));
        assert_eq!(json_for_text(21, "-4"), serde_json::json!(-4));
        assert_eq!(json_for_text(20, "9007199254740993"), serde_json::json!(9007199254740993_i64));
        assert_eq!(json_for_text(701, "Infinity"), serde_json::json!("Infinity"));
        assert_eq!(json_for_text(114, "{\"x\":1}"), serde_json::json!({"x":1}));
        assert_eq!(json_for_text(25, "t"), serde_json::json!("t"));
    }

    #[test]
    fn caret_tracks_multiline_character_position() {
        assert_eq!(caret_line("SELECT\n  λx", 10), Some(("  λx".to_string(), "  ".to_string())));
        assert_eq!(caret_line("abc", 0), None);
    }
}

/// `StatementResult` helper used by summaries: `Some((rows, tag))` for row results.
pub fn rows_of(r: &StatementResult) -> Option<(&[Vec<Datum>], &str)> {
    match r {
        StatementResult::Rows { rows, tag, .. } => Some((rows.as_slice(), tag.as_str())),
        _ => None,
    }
}
