//! COPY IN / COPY OUT over the wire (lease WIR-13; WIRE-08). PG: `commands/copyfromparse.c`
//! (`ReceiveCopyBegin`, `CopyGetData`), `commands/copyto.c` (`SendCopyBegin`, `SendCopyEnd`),
//! `protocol.sgml` "COPY Operations".
//!
//! The executor (`pgarbor_exec::copy::execute_copy`) talks to the client through a byte channel with
//! `read_chunk` / `write_chunk` (trait `pgarbor_exec::copy::CopyChannel`, installed with
//! `Session::set_copy_channel`). [`WireCopyChannel`] is that channel for a network connection.
//!
//! **Blocked on amendment `pgarbor-wire-01`**: the trait is not nameable from this crate until
//! `pgarbor` re-exports `CopyChannel`; until then [`install`] returns
//! `PgError::unimplemented("WIR-13", ..)` and the inherent methods below hold the complete logic. After
//! the amendment, `impl CopyChannel for WireCopyChannel` simply forwards to them and `install` boxes it.
//!
//! ## COPY FROM STDIN (client -> server)
//! First `read_chunk` call: queue `CopyInResponse{overall_format: 0 (text and CSV) , column_formats: []}`
//! (the executor cannot tell us the column list yet - amendment adds `start(direction, binary,
//! ncols)`: then use `overall_format = binary as u8` and `ncols` zero entries), **flush**, then loop
//! reading raw frames (`ConnIo::poll_raw(None)`):
//! * `d` CopyData -> return `Some(body)`;
//! * `c` CopyDone -> mark finished, return `None` (and on later calls keep returning `None`);
//! * `f` CopyFail(message) -> `Err(57014 "COPY from stdin failed: <message>")` (the colon and space
//!   are always present, as in PG);
//! * `H` Flush and `S` Sync -> ignored (PG ignores them during COPY IN);
//! * EOF -> `Err(FATAL 08P01 "unexpected EOF on client connection with an open transaction")`;
//! * anything else -> `Err(08P01 "unexpected message type 0x%02X during COPY from stdin")` with the tag
//!   as two UPPERCASE hex digits.
//!
//! ## COPY TO STDOUT (server -> client)
//! First `write_chunk`: queue `CopyOutResponse{0, []}`; every call queues one `CopyData(chunk)`
//! (auto-flush handles volume). [`CopyHandle::finish_statement`]`(true)` queues `CopyDone` when output
//! had started and flushes; `finish_statement(false)` queues nothing.
//! An error while the channel is in COPY OUT mode is delivered as the normal ErrorResponse; the
//! client's libpq understands that.
//!
//! ## State sharing
//! The channel is moved into the `Session`; the wire layer keeps a [`CopyHandle`] clone of the same
//! `Rc<RefCell<CopyShared>>` to learn whether data flowed and to emit CopyDone.
use std::cell::RefCell;
use std::rc::Rc;

use pgarbor::Session;
use pgarbor_common::{PgError, Result, SqlState, Severity};

use crate::backend::{Backend, CopyResponse};
use crate::buf::MsgReader;
use crate::consts::{F_COPY_DATA, F_COPY_DONE, F_COPY_FAIL};
use crate::error::{WireError, WireResult};
use crate::framing::{Frame, Polled};
use crate::session_loop::SharedIo;

/// Progress shared between the channel (inside the session) and the wire loop.
#[derive(Debug, Default)]
pub struct CopyShared {
    /// CopyInResponse/CopyOutResponse already queued for the current statement.
    pub started: bool,
    pub direction_out: bool,
    /// CopyDone received (IN) / sent (OUT) for the current statement.
    pub finished: bool,
}

/// Wire-side handle (see module docs).
#[derive(Clone, Debug, Default)]
pub struct CopyHandle {
    pub(crate) shared: Rc<RefCell<CopyShared>>,
}

impl CopyHandle {
    /// End-of-statement hook (module docs). Resets the shared state for the next statement.
    pub fn finish_statement(&self, ok: bool, io: &SharedIo) -> WireResult<()> {
        let send_done = {
            let mut state = self.shared.borrow_mut();
            let send_done = ok && state.started && state.direction_out && !state.finished;
            if send_done {
                state.finished = true;
            }
            send_done
        };
        if send_done {
            let mut conn = io.borrow_mut();
            conn.send(&Backend::CopyDone)?;
            conn.flush()?;
        }
        let mut state = self.shared.borrow_mut();
        state.started = false;
        state.direction_out = false;
        state.finished = false;
        Ok(())
    }
}

/// The channel handed to the session (module docs).
pub struct WireCopyChannel {
    pub(crate) io: SharedIo,
    pub(crate) shared: Rc<RefCell<CopyShared>>,
}

impl WireCopyChannel {
    pub fn new(io: SharedIo) -> (WireCopyChannel, CopyHandle) {
        let shared = Rc::new(RefCell::new(CopyShared::default()));
        (WireCopyChannel { io, shared: Rc::clone(&shared) }, CopyHandle { shared })
    }

    /// COPY IN: next chunk (`None` = CopyDone). Module docs.
    pub fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        {
            let mut state = self.shared.borrow_mut();
            if state.finished {
                return Ok(None);
            }
            if !state.started {
                let response = Backend::CopyInResponse(CopyResponse { overall_format: 0, column_formats: Vec::new() });
                let mut conn = self.io.borrow_mut();
                conn.send(&response).map_err(wire_to_pg)?;
                conn.flush().map_err(wire_to_pg)?;
                state.started = true;
                state.direction_out = false;
            }
        }
        loop {
            let polled = self.io.borrow_mut().poll_raw(None).map_err(wire_to_pg)?;
            match polled {
                Polled::Ready(Frame { tag: F_COPY_DATA, body }) => return Ok(Some(body)),
                Polled::Ready(Frame { tag: F_COPY_DONE, .. }) => {
                    self.shared.borrow_mut().finished = true;
                    return Ok(None);
                }
                Polled::Ready(Frame { tag: F_COPY_FAIL, body }) => {
                    let mut reader = MsgReader::new(&body);
                    let message = reader.read_cstr()?.to_owned();
                    reader.finish()?;
                    return Err(PgError::new(SqlState::QUERY_CANCELED, format!("COPY from stdin failed: {message}")));
                }
                Polled::Ready(Frame { tag: b'H' | b'S', .. }) => continue,
                Polled::Ready(Frame { tag, .. }) => return Err(PgError::new(
                    SqlState::PROTOCOL_VIOLATION,
                    format!("unexpected message type 0x{tag:02X} during COPY from stdin"),
                )),
                Polled::Eof => return Err(PgError::new(SqlState::PROTOCOL_VIOLATION, "unexpected EOF on client connection with an open transaction").with_severity(Severity::Fatal)),
                Polled::TimedOut => continue,
            }
        }
    }

    /// COPY OUT: send one chunk. Module docs.
    pub fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        let mut state = self.shared.borrow_mut();
        if state.finished {
            return Ok(());
        }
        let mut conn = self.io.borrow_mut();
        if !state.started {
            conn.send(&Backend::CopyOutResponse(CopyResponse { overall_format: 0, column_formats: Vec::new() }))
                .map_err(wire_to_pg)?;
            state.started = true;
            state.direction_out = true;
        }
        conn.send(&Backend::CopyData(data.to_vec())).map_err(wire_to_pg)
    }
}

fn wire_to_pg(error: WireError) -> PgError {
    match error {
        WireError::Pg(e) => e,
        WireError::Io(e) => PgError::io("COPY protocol I/O", &e),
        WireError::Closed => PgError::new(SqlState::CONNECTION_FAILURE, "connection closed by peer"),
        WireError::Shutdown => PgError::new(SqlState::ADMIN_SHUTDOWN, "terminating connection due to administrator command").with_severity(Severity::Fatal),
    }
}

/// Create the channel, install it into `session` and return the handle. Blocked on the amendment
/// (module docs): returns `unimplemented` until `pgarbor` re-exports `CopyChannel`.
#[allow(unused_variables)]
pub fn install(session: &mut Session, io: &SharedIo) -> Result<CopyHandle> {
    Err(PgError::unimplemented("WIR-13", "install the COPY channel (copy::install; needs amendment pgarbor-wire-01)"))
}
