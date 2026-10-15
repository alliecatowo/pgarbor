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
use pgarbor::{ColumnInfo, FormatCode, Session, StatementOutput, StatementResult, TxnStatus};
use pgarbor_common::{PgError, Result};

use crate::backend::{format_for_column, Backend, RowField};
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::error_fields::{error_message, notice_message};
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
pub fn send_notices(io: &mut ConnIo, notices: &[PgError]) -> WireResult<()> {
    for notice in notices {
        io.send(&notice_message(notice))?;
    }
    Ok(())
}

/// Queue the messages of one statement result (module docs, steps 1-2) and hand `param_status` to
/// `reporter`. A row-encoding error is returned as `Err(WireError::Pg(e))` with nothing queued for
/// the result (notices of the statement stay queued).
pub fn send_statement_output(
    io: &mut ConnIo,
    session: &Session,
    reporter: &mut ParamReporter,
    out: &StatementOutput,
) -> WireResult<()> {
    send_notices(io, &out.notices)?;

    // Prepare every row before queuing this result. That way a failed conversion cannot leave a
    // RowDescription or partial set of DataRows on the wire (printtup.c sends each tuple only
    // after its output functions have completed).
    let encoded_rows = match &out.result {
        StatementResult::Rows { columns, rows, .. } => {
            let mut encoded = Vec::with_capacity(rows.len());
            for row in rows {
                encoded.push(session.encode_row(columns, &[], row).map_err(WireError::Pg)?);
            }
            encoded
        }
        _ => Vec::new(),
    };

    match &out.result {
        StatementResult::Empty => io.send(&Backend::EmptyQueryResponse)?,
        StatementResult::Command { tag } => {
            io.send(&Backend::CommandComplete { tag: tag.clone() })?
        }
        StatementResult::Rows { columns, tag, .. } => {
            io.send(&Backend::RowDescription(row_fields(columns, &[])))?;
            for row in encoded_rows {
                io.send(&Backend::DataRow(row))?;
            }
            io.send(&Backend::CommandComplete { tag: tag.clone() })?;
        }
    }
    reporter.note_all(&out.param_status);
    Ok(())
}

/// Queue the ErrorResponse for `err` (preceded by `notices`), hand `param_status` to the reporter,
/// and flush when `err` is FATAL/PANIC (the caller then closes).
pub fn send_error_cycle(
    io: &mut ConnIo,
    reporter: &mut ParamReporter,
    notices: &[PgError],
    err: &PgError,
    param_status: &[(String, String)],
) -> WireResult<()> {
    send_notices(io, notices)?;
    io.send(&error_message(err))?;
    reporter.note_all(param_status);
    if matches!(err.severity, pgarbor_common::Severity::Fatal | pgarbor_common::Severity::Panic) {
        io.flush()?;
    }
    Ok(())
}

/// End of a command cycle: queue the reporter's pending ParameterStatus messages, then
/// `ReadyForQuery(status)`, then flush.
pub fn finish_cycle(io: &mut ConnIo, reporter: &mut ParamReporter, status: TxnStatus) -> WireResult<()> {
    for message in reporter.drain() {
        io.send(&message)?;
    }
    io.send(&Backend::ready(status))?;
    io.flush()
}

/// Session-level helper: `Result` -> `WireResult`.
pub fn lift<T>(r: Result<T>) -> WireResult<T> {
    r.map_err(WireError::Pg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_fields_preserve_metadata_and_use_text_for_simple_query() {
        let columns = vec![ColumnInfo {
            name: "answer".to_string(),
            type_oid: 23,
            type_mod: -1,
            type_len: 4,
            table_oid: 0,
            column_attnum: 0,
        }];
        let fields = row_fields(&columns, &[]);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "answer");
        assert_eq!(fields[0].type_oid, 23);
        assert_eq!(fields[0].format, 0);
    }
}
