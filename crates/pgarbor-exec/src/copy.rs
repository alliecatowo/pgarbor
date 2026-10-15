//! `COPY` execution and the text/CSV/binary row codecs (lease EXE-41). PG: `commands/copy.c`,
//! `copyfrom.c`, `copyto.c`, `copyfromparse.c`. Bound form: `pgarbor_sql::ir::BoundCopy`.
//!
//! ## Channel
//! `COPY ... FROM STDIN` / `TO STDOUT` talk to the client through a [`CopyChannel`] supplied by the
//! session (the wire layer's CopyData stream, or an in-memory buffer for embedded use):
//! `read_chunk()` returns the next chunk of COPY data (any size, may split lines/fields) or `None`
//! at end of data (CopyDone / EOF); `write_chunk(bytes)` sends data (a row per call is fine).
//! File endpoints (`COPY t FROM '/path'`) use `std::fs` (superuser or role `pg_read_server_files` /
//! `pg_write_server_files`; otherwise 42501 `permission denied to COPY from a file` with DETAIL
//! `Only roles with privileges of the "pg_read_server_files" role may COPY from a file.`; `TO`
//! analog `pg_write_server_files`); PROGRAM endpoints: 0A000.
//!
//! ## `execute_copy(cx, c, plan, chan)`
//! Privileges first (`perm::require_relation_privilege`: `SELECT` for TO, `INSERT` for FROM, with the listed columns).
//! * `CopyDirection::To` with `CopyTarget::Table{table, columns}`: scan the table (`HeapCursor`) and emit
//!   the listed columns (`columns` attnums; empty = all live columns in attnum order); with
//!   `CopyTarget::Query(_)` the caller passes the planned query (`plan`) and rows come from `open_plan`.
//!   Text values via `pgarbor_types::io::output` with the session `IoConfig`. `header` writes the column
//!   names first (CSV/text). Tag `COPY n`.
//! * `From`: parse the input into rows of the listed columns, evaluate DEFAULTs of unlisted columns
//!   (`TargetTable` defaults: bound exprs over the target; use `eval_standalone` after
//!   `columns_to_input_refs`), apply `where_` (over the full row, FALSE/NULL skips), run
//!   `dml_insert::insert_row` for each row (all constraints, indexes, FKs; end-of-statement
//!   `fk::run_pending` once). BATCH row ids per chunk of rows. Tag `COPY n`.
//!
//! ## Text format rules (`CopyFormat::Text`, delimiter default TAB, null string `\N`)
//! Rows end with `\n` (accept `\r\n`/`\r` on input only if consistent; PG complains
//! `literal carriage return found in data` HINT `Use "\r" to represent carriage return.` for stray
//! CRs). Escapes on input: `\b \f \n \r \t \v`, `\\`, `\<delim>`, octal `\ooo` (1-3 digits), hex `\xhh`
//! (1-2 digits); any other `\c` is `c`. On output backslash and the delimiter, `\n`, `\r`, `\t`... are
//! escaped as `\\`, `\<delim>`... (`\n`, `\r`, `\t`, `\b`, `\f`, `\v`) and control characters < 0x20
//! as `\ooo`? (PG escapes `\b \f \n \r \t \v` and backslash/delimiter only). A field equal to the null
//! string is NULL (input, unquoted); NULL is written as the null string. End-of-data marker `\.` on a
//! line by itself ends input early. Column count errors: 22P04 `missing data for column "b"` / `extra
//! data after last expected column`. Invalid values: the type input error (22P02 `invalid input syntax
//! for type integer: "x"`) (PG adds `CONTEXT: COPY t, line 3, column a: "x"`; PgError has no context
//! field yet - amendment pgarbor-types-02 - so omit it).
//!
//! ## CSV (`CopyFormat::Csv`, delimiter `,`, quote `"`, escape = quote, null string empty)
//! Standard RFC4180 with PG's rules: quoted fields may contain delimiters/newlines; an unquoted field
//! equal to the null string is NULL, a quoted one is not (unless FORCE_NULL); output quotes a field if it
//! contains delimiter, quote, `\n`, `\r`, equals the null string, or (CSV) is the empty string?? (PG
//! quotes the empty string only when null string is not empty; and `\.` alone); FORCE_QUOTE columns
//! (`force_quote`, `force_quote_all`) always quoted except NULLs; FORCE_NOT_NULL `force_not_null` makes
//! an unquoted empty/null-string field an empty string instead of NULL; FORCE_NULL `force_null` the reverse
//! for quoted. HEADER: skip the first line on input / write it on output.
//!
//! ## Binary (`CopyFormat::Binary`)
//! Signature `PGCOPY\n\377\r\n\0`, i32 flags (0), i32 extension length (0); per row i16 field count then
//! per field i32 length (-1 NULL) + `io::send` bytes; trailer i16 -1. Input uses `io::recv`.
//! Errors: 22P04 `COPY file signature not recognized`, `invalid COPY file header`, etc.
use pgarbor_common::{PgError, Result};
use pgarbor_plan::plan::Plan;
use pgarbor_sql::ir::BoundCopy;

use crate::context::ExecContext;
use crate::result::ExecResult;

/// Byte channel between COPY and the client (module docs).
// CONTRACT
pub trait CopyChannel {
    /// Next chunk of COPY data; `None` = end of data.
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>>;
    /// Send COPY data to the client.
    fn write_chunk(&mut self, data: &[u8]) -> Result<()>;
}

/// An in-memory channel: `input` is consumed by `read_chunk` (single chunk), `output` collects writes.
#[derive(Debug, Default)]
pub struct MemChannel {
    pub input: Option<Vec<u8>>,
    pub output: Vec<u8>,
}

impl CopyChannel for MemChannel {
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.input.take())
    }
    fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        self.output.extend_from_slice(data);
        Ok(())
    }
}

/// Execute a bound COPY (module docs). `plan` is required for `CopyTarget::Query`.
#[allow(unused_variables)]
pub fn execute_copy(
    cx: &mut ExecContext,
    c: &BoundCopy,
    plan: Option<&Plan>,
    chan: &mut dyn CopyChannel,
) -> Result<ExecResult> {
    Err(PgError::unimplemented("EXE-41", "COPY (copy::execute_copy)"))
}
