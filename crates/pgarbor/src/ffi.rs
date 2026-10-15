//! FFI-friendly synchronous surface (lease API-15). A flat, handle-based API using only integers,
//! strings and byte vectors so that thin `extern "C"` shims (C), PyO3 (Python), napi-rs (Node) and
//! wasm-bindgen (WASM) can wrap it without touching engine types. The shims themselves are separate
//! crates (not part of this contract); this module is the stable, documented seam.
//!
//! ## Model
//! * Handles are non-zero `u64`s into a thread-local table (sessions are `!Send`): a handle is only valid
//!   on the thread that created it. Language bindings that need cross-thread use run a dedicated worker
//!   thread per database.
//! * Every function returns `Result<_, FfiError>`; `FfiError{ code, sqlstate, message, detail, hint,
//!   position }` where `code` is the numeric severity (`ERROR=1, FATAL=2, PANIC=3`) and `sqlstate` the
//!   5-character string. No panics cross the boundary: every entry point wraps its body in
//!   `std::panic::catch_unwind` and maps a panic to `XX000 internal error`.
//! * Text everywhere: values are rendered in PG text format (`io::output`); NULL = `None`.
//!
//! ## Functions
//! * `db_open(path, flags) -> db handle` (`flags`: bit0 create-if-missing, bit1 read-only, bit2 in-memory);
//!   `db_close(db)`.
//! * `session_open(db, branch, user, database) -> session handle`; `session_close(session)`.
//! * `exec(session, sql) -> Vec<FfiResult>`: simple-query semantics; `FfiResult{ columns: Vec<String>,
//!   rows: Vec<Vec<Option<String>>>, tag: String, notices: Vec<String> }` (one per statement); the first
//!   error aborts and is returned as `Err` after rolling back per PG rules (earlier results are lost for
//!   FFI simplicity; use `exec_partial` to keep them).
//! * `exec_params(session, sql, params: Vec<Option<String>>) -> FfiResult`: one statement with text
//!   parameters (types inferred by the server, `Parse`/`Bind`/`Execute`).
//! * `exec_json(session, sql) -> String`: results of `exec` as a JSON document
//!   `[{"columns":[{"name":..,"type_oid":..}],"rows":[[..]],"tag":".."}]` produced by a small hand-written
//!   serializer (this crate has no serde); strings escaped per RFC 8259, NULL as `null`.
//! * `txn_status(session) -> u8` (`I`/`T`/`E`), `cancel(session)`, `branch_create(db, name, from)`,
//!   `branch_list(db) -> Vec<String>`, `branch_delete(db, name)`, `version() -> &'static str` (`18.6`).
//! Handles are released by the matching `*_close`; leaking one leaks only that object.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};

use pgarbor_common::{PgError, Result, SqlState};

use crate::database::Database;
use crate::session::Session;
use crate::types::{
    ColumnInfo, OpenOptions, PortalOutput, PortalStatus, SessionOptions, SimpleQueryOutput, StatementOutput,
    StatementResult,
};

/// Error crossing the FFI boundary.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfiError {
    pub code: i32,
    pub sqlstate: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<u32>,
}

impl From<PgError> for FfiError {
    fn from(e: PgError) -> FfiError {
        FfiError {
            code: match e.severity {
                pgarbor_common::Severity::Fatal => 2,
                pgarbor_common::Severity::Panic => 3,
                _ => 1,
            },
            sqlstate: e.sqlstate.code().to_string(),
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            position: e.position,
        }
    }
}

/// One statement result in text form.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FfiResult {
    pub columns: Vec<String>,
    pub column_type_oids: Vec<u32>,
    pub rows: Vec<Vec<Option<String>>>,
    pub tag: String,
    pub notices: Vec<String>,
}

/// Opaque handle.
pub type Handle = u64;

pub const FLAG_CREATE: u32 = 1;
pub const FLAG_READ_ONLY: u32 = 2;
pub const FLAG_IN_MEMORY: u32 = 4;

/// Result type of the private helpers: every entry point returns `std::result::Result<_, FfiError>`.
type Outcome<T> = std::result::Result<T, FfiError>;

thread_local! {
    /// Next handle to hand out (starts at 1: handle 0 is never valid).
    static NEXT_HANDLE: Cell<u64> = const { Cell::new(1) };
    static DATABASES: RefCell<HashMap<Handle, Database>> = RefCell::new(HashMap::new());
    static SESSIONS: RefCell<HashMap<Handle, Session>> = RefCell::new(HashMap::new());
}

fn next_handle() -> Handle {
    NEXT_HANDLE.with(|c| {
        let h = c.get();
        c.set(h.wrapping_add(1).max(1));
        h
    })
}

/// 08003 `connection does not exist`: the handle is unknown on this thread (never issued, or already closed).
fn bad_handle(kind: &str) -> FfiError {
    PgError::new(SqlState(*b"08003"), format!("invalid {kind} handle")).into()
}

/// Map a caught panic payload to `XX000 internal error`. The payload text (if it is a string) goes to
/// `detail`; it is diagnostic only.
fn panic_error(payload: &(dyn std::any::Any + Send)) -> FfiError {
    let detail = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    };
    FfiError {
        code: 1,
        sqlstate: SqlState::INTERNAL_ERROR.code().to_string(),
        message: "internal error".to_string(),
        detail: Some(detail),
        hint: None,
        position: None,
    }
}

/// Run an entry point body so that no panic unwinds across the boundary.
fn guard<T>(body: impl FnOnce() -> Outcome<T>) -> Outcome<T> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => Err(panic_error(&*payload)),
    }
}

fn insert_database(db: Database) -> Handle {
    let h = next_handle();
    DATABASES.with(|m| {
        m.borrow_mut().insert(h, db);
    });
    h
}

fn get_database(h: Handle) -> Outcome<Database> {
    DATABASES.with(|m| m.borrow().get(&h).cloned()).ok_or_else(|| bad_handle("database"))
}

fn insert_session(session: Session) -> Handle {
    let h = next_handle();
    SESSIONS.with(|m| {
        m.borrow_mut().insert(h, session);
    });
    h
}

/// Run `f` on the session behind `h`. The table borrow is held for the duration of `f`; no FFI call
/// re-enters the table from inside `f`.
fn with_session<T>(h: Handle, f: impl FnOnce(&mut Session) -> Outcome<T>) -> Outcome<T> {
    SESSIONS.with(|m| -> Outcome<T> {
        let mut map = m
            .try_borrow_mut()
            .map_err(|_| FfiError::from(PgError::internal("session table is busy")))?;
        match map.get_mut(&h) {
            Some(session) => f(session),
            None => Err(bad_handle("session")),
        }
    })
}

/// Text rendering of one column value: PG text output is UTF-8 under the UTF8 client encoding.
fn bytes_to_text(value: Option<Vec<u8>>) -> Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(e) => {
                let at = e.utf8_error().valid_up_to();
                let byte = e.as_bytes().get(at).copied().unwrap_or(0);
                Err(PgError::new(
                    SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                    format!("invalid byte sequence for encoding \"UTF8\": 0x{byte:02x}"),
                ))
            }
        },
    }
}

fn encoded_row_to_text(row: Vec<Option<Vec<u8>>>) -> Result<Vec<Option<String>>> {
    row.into_iter().map(bytes_to_text).collect()
}

fn column_names(columns: &[ColumnInfo]) -> Vec<String> {
    columns.iter().map(|c| c.name.clone()).collect()
}

fn column_oids(columns: &[ColumnInfo]) -> Vec<u32> {
    columns.iter().map(|c| c.type_oid).collect()
}

fn notice_messages(notices: Vec<PgError>) -> Vec<String> {
    notices.into_iter().map(|n| n.message).collect()
}

/// Convert one statement output to its text form. Rows are encoded with the text format (empty format
/// list = all text), which is the same encoding `io::output` gives the wire layer.
fn statement_to_ffi(session: &Session, out: StatementOutput) -> Result<FfiResult> {
    let StatementOutput { result, notices, .. } = out;
    let notices = notice_messages(notices);
    match result {
        StatementResult::Empty => Ok(FfiResult { notices, ..FfiResult::default() }),
        StatementResult::Command { tag } => Ok(FfiResult { tag, notices, ..FfiResult::default() }),
        StatementResult::Rows { columns, rows, tag } => {
            let mut text_rows = Vec::with_capacity(rows.len());
            for row in &rows {
                let encoded = session.encode_row(&columns, &[], row)?;
                text_rows.push(encoded_row_to_text(encoded)?);
            }
            Ok(FfiResult {
                columns: column_names(&columns),
                column_type_oids: column_oids(&columns),
                rows: text_rows,
                tag,
                notices,
            })
        }
    }
}

/// `exec` body: the first error aborts and is returned (earlier results are dropped).
fn exec_inner(session: Handle, sql: &str) -> Outcome<Vec<FfiResult>> {
    with_session(session, |s: &mut Session| -> Outcome<Vec<FfiResult>> {
        let SimpleQueryOutput { statements, error, .. } = s.simple_query_raw(sql);
        if let Some(e) = error {
            return Err(e.into());
        }
        let mut results = Vec::with_capacity(statements.len());
        for st in statements {
            results.push(statement_to_ffi(s, st)?);
        }
        Ok(results)
    })
}

/// `exec_partial` body: the results of the statements that completed, and the error (if any) that
/// stopped the rest. An encoding failure of statement `i` is reported as that statement's error.
fn exec_partial_inner(session: Handle, sql: &str) -> Outcome<(Vec<FfiResult>, Option<FfiError>)> {
    with_session(session, |s: &mut Session| -> Outcome<(Vec<FfiResult>, Option<FfiError>)> {
        let SimpleQueryOutput { statements, error, .. } = s.simple_query_raw(sql);
        let mut results = Vec::with_capacity(statements.len());
        for st in statements {
            match statement_to_ffi(s, st) {
                Ok(result) => results.push(result),
                Err(e) => return Ok((results, Some(e.into()))),
            }
        }
        Ok((results, error.map(FfiError::from)))
    })
}

/// One unnamed-statement round trip: Parse, Bind (all parameters and results in text), Describe,
/// Execute. The caller ends the implicit transaction with Sync in every case.
fn extended_once(s: &mut Session, sql: &str, params: &[Option<Vec<u8>>]) -> Result<FfiResult> {
    s.parse("", sql, &[])?;
    s.bind("", "", &[], params, &[])?;
    let columns = s.describe_portal("")?.unwrap_or_default();
    let PortalOutput { rows, status, notices, .. } = s.execute("", 0)?;
    let tag = match status {
        PortalStatus::Complete { tag } => tag,
        PortalStatus::Empty => String::new(),
        PortalStatus::Suspended => {
            return Err(PgError::internal("portal suspended under an unlimited Execute"));
        }
    };
    let mut text_rows = Vec::with_capacity(rows.len());
    for row in rows {
        text_rows.push(encoded_row_to_text(row)?);
    }
    Ok(FfiResult {
        columns: column_names(&columns),
        column_type_oids: column_oids(&columns),
        rows: text_rows,
        tag,
        notices: notice_messages(notices),
    })
}

/// Run `extended_once` and always end the protocol cycle with Sync, so that an error leaves the
/// session usable (the implicit transaction is rolled back by the error; Sync clears the error state).
fn run_extended(s: &mut Session, sql: &str, params: &[Option<Vec<u8>>]) -> Result<FfiResult> {
    match extended_once(s, sql, params) {
        Ok(result) => {
            s.sync()?;
            Ok(result)
        }
        Err(e) => {
            let _ = s.sync();
            Err(e)
        }
    }
}

fn exec_params_inner(session: Handle, sql: &str, params: &[Option<String>]) -> Outcome<FfiResult> {
    let bound: Vec<Option<Vec<u8>>> = params.iter().map(|p| p.as_ref().map(|v| v.as_bytes().to_vec())).collect();
    with_session(session, |s: &mut Session| -> Outcome<FfiResult> { Ok(run_extended(s, sql, &bound)?) })
}

/// Append `value` as a JSON string literal (RFC 8259): `"` and `\` escaped, control characters below
/// U+0020 as `\u00XX`, everything else (including non-ASCII) emitted as UTF-8.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize results in the documented `exec_json` shape (one object per statement).
fn results_to_json(results: &[FfiResult]) -> String {
    let mut out = String::from("[");
    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"columns\":[");
        for (j, name) in result.columns.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            write_json_string(&mut out, name);
            let oid = result.column_type_oids.get(j).copied().unwrap_or(0);
            let _ = write!(out, ",\"type_oid\":{oid}}}");
        }
        out.push_str("],\"rows\":[");
        for (k, row) in result.rows.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push('[');
            for (m, value) in row.iter().enumerate() {
                if m > 0 {
                    out.push(',');
                }
                match value {
                    Some(text) => write_json_string(&mut out, text),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        out.push_str("],\"tag\":");
        write_json_string(&mut out, &result.tag);
        out.push('}');
    }
    out.push(']');
    out
}

/// Open (or create) a database file, or a fresh in-memory database (module docs).
// CONTRACT
pub fn db_open(path: &str, flags: u32) -> std::result::Result<Handle, FfiError> {
    guard(|| -> Outcome<Handle> {
        let unknown = flags & !(FLAG_CREATE | FLAG_READ_ONLY | FLAG_IN_MEMORY);
        if unknown != 0 {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized database open flags: {unknown:#x}"),
            )
            .into());
        }
        let read_only = (flags & FLAG_READ_ONLY) != 0;
        let db = if (flags & FLAG_IN_MEMORY) != 0 {
            // The path is ignored and FLAG_CREATE is implied for an in-memory database.
            if read_only {
                return Err(PgError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "an in-memory database cannot be opened read-only",
                )
                .into());
            }
            Database::in_memory()?
        } else {
            let opts = OpenOptions { read_only, ..OpenOptions::default() };
            if (flags & FLAG_CREATE) != 0 {
                Database::open_or_create(path, opts)?
            } else {
                Database::open(path, opts)?
            }
        };
        Ok(insert_database(db))
    })
}

/// Close a database handle (`Database::close`). Sessions opened from it keep working until they are
/// closed or the engine reports 57P01 after the shutdown.
// CONTRACT
pub fn db_close(db: Handle) -> std::result::Result<(), FfiError> {
    guard(|| -> Outcome<()> {
        let database = DATABASES
            .with(|m| m.borrow_mut().remove(&db))
            .ok_or_else(|| bad_handle("database"))?;
        database.close()?;
        Ok(())
    })
}

/// Open a session on `branch` for `user` / `database` (`Database::session`).
// CONTRACT
pub fn session_open(db: Handle, branch: &str, user: &str, database: &str) -> std::result::Result<Handle, FfiError> {
    guard(|| -> Outcome<Handle> {
        let handle_db = get_database(db)?;
        let opts = SessionOptions {
            user: user.to_string(),
            database: database.to_string(),
            ..SessionOptions::default()
        };
        let session = handle_db.session(branch, opts)?;
        Ok(insert_session(session))
    })
}

/// Close a session: rolls back an open transaction and unregisters the backend.
// CONTRACT
pub fn session_close(session: Handle) -> std::result::Result<(), FfiError> {
    guard(|| -> Outcome<()> {
        let closing = SESSIONS
            .with(|m| m.borrow_mut().remove(&session))
            .ok_or_else(|| bad_handle("session"))?;
        closing.close();
        Ok(())
    })
}

/// Simple Query: one `FfiResult` per statement; the first error is returned as `Err`.
// CONTRACT
pub fn exec(session: Handle, sql: &str) -> std::result::Result<Vec<FfiResult>, FfiError> {
    guard(|| exec_inner(session, sql))
}

/// Like [`exec`] but keeps the results of the statements that completed before an error:
/// `(results, error)`.
// CONTRACT
pub fn exec_partial(session: Handle, sql: &str) -> std::result::Result<(Vec<FfiResult>, Option<FfiError>), FfiError> {
    guard(|| exec_partial_inner(session, sql))
}

/// One statement with text parameters (`None` = NULL). Parameter types are inferred by the server.
// CONTRACT
pub fn exec_params(session: Handle, sql: &str, params: &[Option<String>]) -> std::result::Result<FfiResult, FfiError> {
    guard(|| exec_params_inner(session, sql, params))
}

/// `exec` results as the JSON document described in the module docs.
// CONTRACT
pub fn exec_json(session: Handle, sql: &str) -> std::result::Result<String, FfiError> {
    guard(|| -> Outcome<String> {
        let results = exec_inner(session, sql)?;
        Ok(results_to_json(&results))
    })
}

/// Transaction status byte (`I`, `T`, `E`) of the session.
// CONTRACT
pub fn txn_status(session: Handle) -> std::result::Result<u8, FfiError> {
    guard(|| with_session(session, |s: &mut Session| -> Outcome<u8> { Ok(s.txn_status().byte()) }))
}

/// Request cancellation of the running statement (57014 in that statement's error).
// CONTRACT
pub fn cancel(session: Handle) -> std::result::Result<(), FfiError> {
    guard(|| with_session(session, |s: &mut Session| -> Outcome<()> {
        s.cancel_token().cancel();
        Ok(())
    }))
}

/// Create branch `name` from branch `from` (`Database::create_branch`).
// CONTRACT
pub fn branch_create(db: Handle, name: &str, from: &str) -> std::result::Result<(), FfiError> {
    guard(|| -> Outcome<()> {
        let database = get_database(db)?;
        database.create_branch(name, from)?;
        Ok(())
    })
}

/// Names of the live (not deleted) branches.
// CONTRACT
pub fn branch_list(db: Handle) -> std::result::Result<Vec<String>, FfiError> {
    guard(|| -> Outcome<Vec<String>> {
        let database = get_database(db)?;
        let branches = database.list_branches(false)?;
        Ok(branches.into_iter().map(|b| b.name).collect())
    })
}

/// Delete branch `name` (`Database::delete_branch` with `force = false`).
// CONTRACT
pub fn branch_delete(db: Handle, name: &str) -> std::result::Result<(), FfiError> {
    guard(|| -> Outcome<()> {
        let database = get_database(db)?;
        database.delete_branch(name, false)?;
        Ok(())
    })
}

/// Server version string reported by the engine (`SHOW server_version`).
pub fn version() -> &'static str {
    "18.6"
}

#[allow(dead_code)]
fn _result_alias(_: Result<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal RFC 8259 syntax checker (tests only): accepts exactly one JSON value.
    struct JsonCheck<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl JsonCheck<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            }
        }

        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }

        fn eat(&mut self, c: u8) -> std::result::Result<(), String> {
            if self.peek() == Some(c) {
                self.i += 1;
                Ok(())
            } else {
                Err(format!("expected '{}' at byte {}", c as char, self.i))
            }
        }

        fn literal(&mut self, word: &[u8]) -> std::result::Result<(), String> {
            if self.b[self.i..].starts_with(word) {
                self.i += word.len();
                Ok(())
            } else {
                Err(format!("bad literal at byte {}", self.i))
            }
        }

        fn number(&mut self) -> std::result::Result<(), String> {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                    self.i += 1;
                } else {
                    break;
                }
            }
            if self.i == start { Err(format!("bad number at byte {start}")) } else { Ok(()) }
        }

        fn string(&mut self) -> std::result::Result<(), String> {
            self.eat(b'"')?;
            loop {
                match self.peek() {
                    None => return Err("unterminated string".to_string()),
                    Some(b'"') => {
                        self.i += 1;
                        return Ok(());
                    }
                    Some(b'\\') => {
                        self.i += 1;
                        match self.peek() {
                            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                            Some(b'u') => {
                                self.i += 1;
                                for _ in 0..4 {
                                    match self.peek() {
                                        Some(h) if h.is_ascii_hexdigit() => self.i += 1,
                                        _ => return Err(format!("bad \\u escape at byte {}", self.i)),
                                    }
                                }
                            }
                            _ => return Err(format!("bad escape at byte {}", self.i)),
                        }
                    }
                    Some(c) if c < 0x20 => return Err(format!("raw control byte at {}", self.i)),
                    Some(_) => self.i += 1,
                }
            }
        }

        fn array(&mut self) -> std::result::Result<(), String> {
            self.eat(b'[')?;
            self.ws();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(());
            }
            loop {
                self.value()?;
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        return Ok(());
                    }
                    _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
                }
            }
        }

        fn object(&mut self) -> std::result::Result<(), String> {
            self.eat(b'{')?;
            self.ws();
            if self.peek() == Some(b'}') {
                self.i += 1;
                return Ok(());
            }
            loop {
                self.ws();
                self.string()?;
                self.ws();
                self.eat(b':')?;
                self.value()?;
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        return Ok(());
                    }
                    _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
                }
            }
        }

        fn value(&mut self) -> std::result::Result<(), String> {
            self.ws();
            match self.peek() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string(),
                Some(b't') => self.literal(b"true"),
                Some(b'f') => self.literal(b"false"),
                Some(b'n') => self.literal(b"null"),
                Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
                _ => Err(format!("unexpected byte at {}", self.i)),
            }
        }
    }

    fn assert_valid_json(text: &str) {
        let mut p = JsonCheck { b: text.as_bytes(), i: 0 };
        p.value().unwrap_or_else(|e| panic!("invalid JSON ({e}): {text}"));
        p.ws();
        assert_eq!(p.i, text.len(), "trailing bytes in JSON: {text}");
    }

    #[test]
    fn panic_becomes_xx000_and_does_not_unwind() {
        let r: Outcome<()> = guard(|| -> Outcome<()> { panic!("boom") });
        let e = r.unwrap_err();
        assert_eq!(e.sqlstate, "XX000");
        assert_eq!(e.code, 1);
        assert_eq!(e.message, "internal error");
        assert_eq!(e.detail.as_deref(), Some("boom"));
    }

    #[test]
    fn json_string_escaping() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\n\u{1}\u{7f}é");
        assert_eq!(out, "\"a\\\"b\\\\c\\u000a\\u0001\u{7f}é\"");
        assert_valid_json(&out);
    }

    #[test]
    fn json_document_shape_and_nulls() {
        let results = vec![
            FfiResult {
                columns: vec!["a".to_string(), "?column?".to_string()],
                column_type_oids: vec![23, 2278],
                rows: vec![vec![Some("1".to_string()), None], vec![Some("x\ty".to_string()), None]],
                tag: "SELECT 2".to_string(),
                notices: Vec::new(),
            },
            FfiResult { tag: "CREATE TABLE".to_string(), ..FfiResult::default() },
        ];
        let json = results_to_json(&results);
        assert_valid_json(&json);
        assert_eq!(
            json,
            "[{\"columns\":[{\"name\":\"a\",\"type_oid\":23},{\"name\":\"?column?\",\"type_oid\":2278}],\
             \"rows\":[[\"1\",null],[\"x\\u0009y\",null]],\"tag\":\"SELECT 2\"},\
             {\"columns\":[],\"rows\":[],\"tag\":\"CREATE TABLE\"}]"
        );
        assert_valid_json("[]");
        assert_valid_json(&results_to_json(&[]));
    }

    #[test]
    fn unknown_open_flags_are_rejected_before_the_engine() {
        let e = db_open("unused.pga", 0x80).unwrap_err();
        assert_eq!(e.sqlstate, "22023");
        let e = db_open("", FLAG_IN_MEMORY | FLAG_READ_ONLY).unwrap_err();
        assert_eq!(e.sqlstate, "22023");
    }

    #[test]
    fn unknown_handles_are_08003() {
        assert_eq!(txn_status(u64::MAX).unwrap_err().sqlstate, "08003");
        assert_eq!(session_close(0).unwrap_err().sqlstate, "08003");
        assert_eq!(db_close(0).unwrap_err().sqlstate, "08003");
        assert_eq!(exec(7, "select 1").unwrap_err().sqlstate, "08003");
        assert_eq!(branch_list(7).unwrap_err().sqlstate, "08003");
    }

    #[test]
    fn fatal_and_panic_severities_map_to_codes() {
        let fatal = PgError::new(SqlState(*b"57P01"), "x").with_severity(pgarbor_common::Severity::Fatal);
        assert_eq!(FfiError::from(fatal).code, 2);
        let panic = PgError::new(SqlState::INTERNAL_ERROR, "x").with_severity(pgarbor_common::Severity::Panic);
        assert_eq!(FfiError::from(panic).code, 3);
        assert_eq!(version(), "18.6");
    }

    #[test]
    #[ignore = "needs API-04, API-07, API-10"]
    fn exec_select_columns_and_nulls() {
        let db = db_open("", FLAG_IN_MEMORY).unwrap();
        let s = session_open(db, "main", "postgres", "postgres").unwrap();
        let results = exec(s, "select 1 as a, null").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].columns, vec!["a".to_string(), "?column?".to_string()]);
        assert_eq!(results[0].rows, vec![vec![Some("1".to_string()), None]]);
        session_close(s).unwrap();
        db_close(db).unwrap();
    }

    #[test]
    #[ignore = "needs API-04, API-07, API-10"]
    fn sql_error_carries_sqlstate() {
        let db = db_open("", FLAG_IN_MEMORY).unwrap();
        let s = session_open(db, "main", "postgres", "postgres").unwrap();
        let e = exec(s, "select 1/0").unwrap_err();
        assert_eq!(e.sqlstate, "22012");
        let (partial, err) = exec_partial(s, "select 1; select 1/0; select 2").unwrap();
        assert_eq!(partial.len(), 1);
        assert_eq!(err.unwrap().sqlstate, "22012");
        session_close(s).unwrap();
        db_close(db).unwrap();
    }

    #[test]
    #[ignore = "needs API-04, API-07, API-10"]
    fn exec_json_is_valid_json() {
        let db = db_open("", FLAG_IN_MEMORY).unwrap();
        let s = session_open(db, "main", "postgres", "postgres").unwrap();
        let json = exec_json(s, "select 'a\"b' as t, null as n; create table j(x int)").unwrap();
        assert_valid_json(&json);
        session_close(s).unwrap();
        db_close(db).unwrap();
    }

    #[test]
    #[ignore = "needs API-04, API-07, API-10"]
    fn exec_params_binds_text_values() {
        let db = db_open("", FLAG_IN_MEMORY).unwrap();
        let s = session_open(db, "main", "postgres", "postgres").unwrap();
        let r = exec_params(s, "select $1::int + 1 as v, $2::text as t", &[Some("41".to_string()), None]).unwrap();
        assert_eq!(r.rows, vec![vec![Some("42".to_string()), None]]);
        assert_eq!(r.tag, "SELECT 1");
        session_close(s).unwrap();
        db_close(db).unwrap();
    }
}
