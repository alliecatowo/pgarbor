//! Table data section of `pgarbor dump` (lease CLI-27).
//!
//! [`dump_table_data`] writes the rows of one table to a `std::io::Write` sink using the session that
//! is already inside the dump's REPEATABLE READ transaction.
//!
//! ## INSERT mode (default; works without COPY support)
//! `SELECT <quoted column list> FROM <qualified table>` (generated columns excluded; identity columns
//! included) executed in chunks via `session.simple_query` of the whole result (v1 holds one table's
//! rows in memory; document the limit: tables above 1 GiB of text should use `--copy` once COPY
//! streaming exists). One statement per row:
//! `INSERT INTO s.t (a, b) VALUES (1, 'x');` with identity columns needing
//! `OVERRIDING SYSTEM VALUE` when the column is `GENERATED ALWAYS`.
//! Row order: the executor's natural scan order (the primary key order for index-organized scans is not
//! promised); the dump must reload to an equivalent database, not byte-identical text.
//!
//! ## Literal rendering ([`literal_for`])
//! By type OID of the column: NULL -> `NULL`; bool 16 -> `true`/`false`; int2/int4/int8 (21/23/20),
//! oid 26 -> the digits; float4/float8 (700/701) -> finite values as text output, `NaN`/`Infinity`/
//! `-Infinity` as quoted `'NaN'` etc.; numeric 1700 -> text output (`NaN` quoted); bytea 17 ->
//! `'\x<hex>'`; arrays and every other type -> `quote_literal(text_output)` (the text input function
//! reconverts, with an explicit cast `::typename` only for types whose literal would otherwise be
//! ambiguous: none in v1). Text output comes from `Session::encode_value(.., FormatCode::Text)`.
//!
//! ## COPY mode (`--copy`; needs pgarbor-wire-01 for `MemChannel`)
//! `COPY s.t (a, b) FROM stdin;` then rows produced by running `COPY (SELECT ..) TO STDOUT` through
//! a memory channel, then `\.` on its own line. Until `pgarbor::MemChannel` is available return
//! exit 8 `dump --copy needs COPY streaming (not implemented yet)`.
//!
//! Returns the number of rows written.
use pgarbor_common::PgError;
use std::io::Write;

use pgarbor::{ColumnInfo, Datum, FormatCode, Session, StatementResult, TypeId};

use crate::ctx::CliError;

/// What to dump for one table.
// CONTRACT
#[derive(Clone, Debug)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    /// Column names to dump in attnum order (generated columns already excluded).
    pub columns: Vec<String>,
    /// Names of `GENERATED ALWAYS AS IDENTITY` columns (need OVERRIDING SYSTEM VALUE).
    pub identity_always: Vec<String>,
}

/// Write the data of `table` (module docs). `copy` selects COPY mode.
pub fn dump_table_data(
    session: &mut Session,
    table: &TableRef,
    copy: bool,
    out: &mut dyn Write,
) -> Result<u64, CliError> {
    if copy {
        return Err(CliError::from(PgError::unimplemented("CLI-27", "dump --copy needs COPY streaming (not implemented yet)")));
    }
    if table.columns.is_empty() {
        return Ok(0);
    }
    let columns = table.columns.iter().map(|c| crate::cmd::dump::quote_ident(c)).collect::<Result<Vec<_>, _>>()?.join(", ");
    let sql = format!("SELECT {columns} FROM {}.{}", crate::cmd::dump::quote_ident(&table.schema)?, crate::cmd::dump::quote_ident(&table.name)?);
    let outputs = session.simple_query(&sql).map_err(CliError::from)?;
    let mut rows: &[Vec<Datum>] = &[];
    let mut column_info: &[ColumnInfo] = &[];
    for output in &outputs {
        if let StatementResult::Rows { columns, rows: result_rows, .. } = &output.result {
            column_info = columns;
            rows = result_rows;
            break;
        }
    }
    let mut count = 0u64;
    for row in rows {
        if row.len() != column_info.len() || row.len() != table.columns.len() {
            return Err(CliError::internal("dump query returned a row with an unexpected number of columns"));
        }
        let mut values = Vec::with_capacity(row.len());
        for (col, value) in column_info.iter().zip(row) {
            values.push(literal_for(session, col, value)?);
        }
        let overriding = if table.identity_always.is_empty() { "" } else { " OVERRIDING SYSTEM VALUE" };
        let statement = format!("INSERT INTO {}.{} ({columns}){overriding} VALUES ({});\n",
            crate::cmd::dump::quote_ident(&table.schema)?, crate::cmd::dump::quote_ident(&table.name)?, values.join(", "));
        out.write_all(statement.as_bytes()).map_err(|e| CliError::io("write dump data", &e))?;
        count += 1;
    }
    Ok(count)
}

/// SQL literal for one value (module docs).
pub fn literal_for(session: &Session, col: &ColumnInfo, value: &Datum) -> Result<String, CliError> {
    if matches!(value, Datum::Null) {
        return Ok("NULL".to_owned());
    }
    let encoded = session.encode_value(TypeId { oid: col.type_oid, typmod: col.type_mod }, value, FormatCode::Text)
        .map_err(CliError::from)?
        .ok_or_else(|| CliError::internal("non-NULL value encoded as SQL NULL"))?;
    let text = std::str::from_utf8(&encoded).map_err(|_| CliError::internal("text value encoding was not UTF-8"))?;
    Ok(literal_from_text(col.type_oid, Some(text)))
}

fn literal_from_text(oid: u32, text: Option<&str>) -> String {
    let Some(text) = text else { return "NULL".to_owned(); };
    match oid {
        16 => match text { "t" | "true" => "true".to_owned(), _ => "false".to_owned() },
        21 | 23 | 20 | 26 => text.to_owned(),
        700 | 701 if matches!(text, "NaN" | "Infinity" | "-Infinity") => quote_literal_text(text),
        700 | 701 | 1700 if text != "NaN" => text.to_owned(),
        1700 => quote_literal_text(text),
        17 if text.starts_with("\\x") => format!("'{}'", text.replace('\'', "''")),
        _ => quote_literal_text(text),
    }
}

fn quote_literal_text(text: &str) -> String {
    if text.contains('\\') {
        format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", text.replace('\'', "''"))
    }
}

#[cfg(test)]
mod tests {
    use super::literal_from_text;

    #[test]
    fn renders_scalar_literals() {
        assert_eq!(literal_from_text(23, Some("42")), "42");
        assert_eq!(literal_from_text(16, Some("t")), "true");
        assert_eq!(literal_from_text(23, None), "NULL");
        assert_eq!(literal_from_text(701, Some("NaN")), "'NaN'");
        assert_eq!(literal_from_text(701, Some("Infinity")), "'Infinity'");
        assert_eq!(literal_from_text(1700, Some("1.25")), "1.25");
        assert_eq!(literal_from_text(17, Some("\\x00ff")), "'\\x00ff'");
        assert_eq!(literal_from_text(25, Some("it's \\")), r"E'it''s \\'");
    }
}
