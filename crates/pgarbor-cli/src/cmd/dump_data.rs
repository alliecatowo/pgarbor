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

use pgarbor::{ColumnInfo, Datum, Session};

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
#[allow(unused_variables)]
pub fn dump_table_data(
    session: &mut Session,
    table: &TableRef,
    copy: bool,
    out: &mut dyn Write,
) -> Result<u64, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-27", "dump table data (dump_data::dump_table_data)")))
}

/// SQL literal for one value (module docs).
#[allow(unused_variables)]
pub fn literal_for(session: &Session, col: &ColumnInfo, value: &Datum) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-27", "dump literal rendering (dump_data::literal_for)")))
}
