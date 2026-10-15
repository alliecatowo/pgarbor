//! Turning embedded-API results into backend messages (lease WIR-14). PG: `access/common/printtup.c`
//! (`SendRowDescriptionMessage`, `printtup`), `tcop/dest.c` (`EndCommand`, `NullCommand`),
//! `tcop/postgres.c` (`ReadyForQuery` path: `ReportChangedGUCOptions` then `Z`).
//!
//! ## Message order for one simple-query statement result (`StatementOutput`)
//! 1. every `notices[i]` as NoticeResponse (`error_fields::notice_message`), in order;
//! 2. the result:
//!    * `Empty` -> `EmptyQueryResponse`;
//!    * `Command{tag}` -> `CommandComplete{tag}`;
//!    * `Rows{columns, rows, tag}` -> `RowDescription` (all columns format 0 = text; simple Query
//!      always returns text), one `DataRow` per row, `CommandComplete{tag}`. Every row is encoded with
//!      `Session::encode_row(columns, &[], row)` BEFORE the RowDescription is queued; if encoding fails
//!      the function returns that `PgError` having queued nothing for this result (the caller reports it
//!      as the statement's error);
//! 3. `param_status` is NOT written here: it is handed to the [`ParamReporter`], which emits
//!    ParameterStatus just before ReadyForQuery ([`finish_cycle`]).
//!
//! ## Error cycle (`send_error_cycle`)
//! notices raised by the failing statement, then ErrorResponse, then (after the caller's rollback
//! output) the reporter drains and ReadyForQuery. FATAL/PANIC errors are sent and flushed but NOT
//! followed by ReadyForQuery; the caller closes the connection.
use pgarbor::{ColumnInfo, FormatCode, Session, StatementOutput, TxnStatus};
use pgarbor_common::{PgError, Result};

use crate::backend::{format_for_column, RowField};
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::param_report::ParamReporter;

/// RowDescription fields for `columns` with the result formats of a portal (empty slice = all text;
/// one entry applies to all columns; else one per column). Used for simple Query (empty formats),
/// Describe portal (the formats recorded at Bind) and Describe statement (empty formats).
pub fn row_fields(columns: &[ColumnInfo], formats: &[FormatCode]) -> Vec<RowField> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| RowField {
            name: c.name.clone(),
            table_oid: c.table_oid,
            column_attnum: c.column_attnum,
            type_oid: c.type_oid,
            type_len: c.type_len,
            type_mod: c.type_mod,
            format: format_for_column(formats, i).as_i16(),
        })
        .collect()
}

/// Queue NoticeResponse messages for `notices` (module docs).
#[allow(unused_variables)]
pub fn send_notices(io: &mut ConnIo, notices: &[PgError]) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-14", "send notices (result_encode::send_notices)")))
}

/// Queue the messages of one statement result (module docs, steps 1-2) and hand `param_status` to
/// `reporter`. A row-encoding error is returned as `Err(WireError::Pg(e))` with nothing queued for
/// the result (notices of the statement stay queued).
#[allow(unused_variables)]
pub fn send_statement_output(
    io: &mut ConnIo,
    session: &Session,
    reporter: &mut ParamReporter,
    out: &StatementOutput,
) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-14", "send a statement result (result_encode::send_statement_output)")))
}

/// Queue the ErrorResponse for `err` (preceded by `notices`), hand `param_status` to the reporter,
/// and flush when `err` is FATAL/PANIC (the caller then closes).
#[allow(unused_variables)]
pub fn send_error_cycle(
    io: &mut ConnIo,
    reporter: &mut ParamReporter,
    notices: &[PgError],
    err: &PgError,
    param_status: &[(String, String)],
) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-14", "send an error (result_encode::send_error_cycle)")))
}

/// End of a command cycle: queue the reporter's pending ParameterStatus messages, then
/// `ReadyForQuery(status)`, then flush.
#[allow(unused_variables)]
pub fn finish_cycle(io: &mut ConnIo, reporter: &mut ParamReporter, status: TxnStatus) -> WireResult<()> {
    Err(WireError::Pg(PgError::unimplemented("WIR-14", "ReadyForQuery cycle end (result_encode::finish_cycle)")))
}

/// Session-level helper: `Result` -> `WireResult`.
pub fn lift<T>(r: Result<T>) -> WireResult<T> {
    r.map_err(WireError::Pg)
}
