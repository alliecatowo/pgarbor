//! psql-style result tables (lease CLI-01). Pure string rendering, no I/O.
//!
//! ## Aligned format (`psql` default, `\pset format aligned`, border 1)
//! ```text
//!  id | name  |  price
//! ----+-------+--------
//!   1 | apple |   1.50
//!  22 | pear  |
//! (2 rows)
//! ```
//! * Column width = max display width (chars) of header and cells. Header cells are CENTERED
//!   (extra space goes to the right); data cells are LEFT aligned except numeric columns
//!   (`right_align[i] == true`) which are RIGHT aligned. Cells are separated by ` | `; each line starts
//!   with one space and the last cell is not padded when left-aligned (no trailing spaces).
//! * The rule line is `-` repeated `width + 2` per column joined by `+`.
//! * NULL is rendered by the caller as the empty string (psql default).
//! * Footer `(N rows)` / `(1 row)` / `(0 rows)`; omitted when `footer == false`.
//! * Values containing `\n` are printed raw (psql's `+` continuation markers are a documented gap).
//! * A result with zero columns prints only the footer-less empty string.
//!
//! ## CSV (`render_csv`)
//! RFC 4180: fields containing `,` `"` `\n` `\r` are quoted with `"` doubled; header row first; rows
//! end with `\n`; NULL (`None`) is an empty unquoted field, empty string is `""`.
//!
//! ## Expanded (`render_expanded`, psql `\x`)
//! ```text
//! -[ RECORD 1 ]-----
//! id   | 1
//! name | apple
//! ```
//! Header line `-[ RECORD n ]` padded with `-` to (name width + 3 + value width max), names left
//! aligned padded to the longest name.
use pgarbor_common::PgError;
use crate::ctx::CliError;

/// A table to print.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Per column: right-align (numeric types).
    pub right_align: Vec<bool>,
}

/// Aligned output (module docs).
#[allow(unused_variables)]
pub fn render_aligned(t: &Table, footer: bool) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-01", "aligned table (table::render_aligned)")))
}

/// CSV output (module docs).
#[allow(unused_variables)]
pub fn render_csv(t: &Table) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-01", "CSV table (table::render_csv)")))
}

/// Expanded output (module docs).
#[allow(unused_variables)]
pub fn render_expanded(t: &Table) -> Result<String, CliError> {
    Err(CliError::from(PgError::unimplemented("CLI-01", "expanded table (table::render_expanded)")))
}

/// True for PG numeric type OIDs that psql right-aligns: int2 21, int4 23, int8 20, oid 26, float4 700,
/// float8 701, numeric 1700, money 790.
pub fn is_numeric_oid(oid: u32) -> bool {
    matches!(oid, 20 | 21 | 23 | 26 | 700 | 701 | 790 | 1700)
}
