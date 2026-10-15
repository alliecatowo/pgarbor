//! C ABI over `pgarbor::ffi` (lease API-15). Header: `include/pgarbor.h`.
//!
//! Conventions:
//! * Handles are `uint64_t`; 0 is never valid. They live in thread-local tables inside the engine, so a
//!   handle must only be used on the thread that created it.
//! * Functions return 0 on success and -1 on failure. On failure the per-thread "last error" is set and
//!   can be read with `pga_errcode`, `pga_errmsg`, `pga_errdetail`, `pga_errhint`. Returned `const char*`
//!   stay valid until the next `pga_*` call on the same thread.
//! * All strings are UTF-8, NUL-terminated. NULL/invalid-UTF-8 inputs yield error 22023/22021.
//! * No panic crosses the boundary.
#![allow(clippy::missing_safety_doc)]

use pgarbor::ffi::{self, FfiError, FfiResult};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub const PGA_OPEN_CREATE: u32 = 1;
pub const PGA_OPEN_READONLY: u32 = 2;
pub const PGA_OPEN_MEMORY: u32 = 4;

/// Opaque result set (one entry per statement).
pub struct PgaResult {
    stmts: Vec<Stmt>,
}
struct Stmt {
    tag: CString,
    columns: Vec<CString>,
    type_oids: Vec<u32>,
    rows: Vec<Vec<Option<CString>>>,
}

#[derive(Default)]
struct LastError {
    sqlstate: CString,
    message: CString,
    detail: Option<CString>,
    hint: Option<CString>,
}

thread_local! {
    static LAST_ERROR: RefCell<LastError> = RefCell::new(LastError::default());
    static SCRATCH: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn cstring(s: &str) -> CString {
    // Interior NULs cannot be represented in a C string; replace them.
    CString::new(s.replace('\0', "\u{FFFD}")).unwrap_or_default()
}

fn set_error(e: &FfiError) {
    LAST_ERROR.with(|l| {
        *l.borrow_mut() = LastError {
            sqlstate: cstring(&e.sqlstate),
            message: cstring(&e.message),
            detail: e.detail.as_deref().map(cstring),
            hint: e.hint.as_deref().map(cstring),
        };
    });
}

fn fail(sqlstate: &str, message: &str) -> i32 {
    set_error(&FfiError {
        code: 1,
        sqlstate: sqlstate.to_string(),
        message: message.to_string(),
        detail: None,
        hint: None,
        position: None,
    });
    -1
}

fn run<T>(body: impl FnOnce() -> Result<T, FfiError>) -> Result<T, i32> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            set_error(&e);
            Err(-1)
        }
        Err(_) => Err(fail("XX000", "internal error (panic in FFI layer)")),
    }
}

unsafe fn str_arg<'a>(p: *const c_char, what: &str) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(fail("22023", &format!("{what} must not be NULL")));
    }
    match unsafe { CStr::from_ptr(p) }.to_str() {
        Ok(s) => Ok(s),
        Err(_) => Err(fail("22021", &format!("{what} is not valid UTF-8"))),
    }
}

fn convert(results: Vec<FfiResult>) -> PgaResult {
    PgaResult {
        stmts: results
            .into_iter()
            .map(|r| Stmt {
                tag: cstring(&r.tag),
                columns: r.columns.iter().map(|c| cstring(c)).collect(),
                type_oids: r.column_type_oids,
                rows: r.rows.into_iter().map(|row| row.into_iter().map(|v| v.map(|s| cstring(&s))).collect()).collect(),
            })
            .collect(),
    }
}

/// Engine version string, e.g. "18.6". Static.
#[unsafe(no_mangle)]
pub extern "C" fn pga_version() -> *const c_char {
    c"18.6".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn pga_errcode() -> *const c_char {
    LAST_ERROR.with(|l| l.borrow().sqlstate.as_ptr())
}
#[unsafe(no_mangle)]
pub extern "C" fn pga_errmsg() -> *const c_char {
    LAST_ERROR.with(|l| l.borrow().message.as_ptr())
}
/// NULL when there is no detail.
#[unsafe(no_mangle)]
pub extern "C" fn pga_errdetail() -> *const c_char {
    LAST_ERROR.with(|l| l.borrow().detail.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()))
}
/// NULL when there is no hint.
#[unsafe(no_mangle)]
pub extern "C" fn pga_errhint() -> *const c_char {
    LAST_ERROR.with(|l| l.borrow().hint.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()))
}

/// Open a database. `flags`: PGA_OPEN_CREATE | PGA_OPEN_READONLY | PGA_OPEN_MEMORY. Writes the handle to `*out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_open(path: *const c_char, flags: u32, out: *mut u64) -> i32 {
    if out.is_null() {
        return fail("22023", "out must not be NULL");
    }
    let path = match unsafe { str_arg(path, "path") } {
        Ok(p) => p,
        Err(c) => return c,
    };
    match run(|| ffi::db_open(path, flags)) {
        Ok(h) => {
            unsafe { *out = h };
            0
        }
        Err(c) => c,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pga_close(db: u64) -> i32 {
    run(|| ffi::db_close(db)).map_or_else(|c| c, |_| 0)
}

/// Open a session on `branch` (NULL = "main") as `user` (NULL = "postgres") on database name `dbname`
/// (NULL = "postgres").
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_session_open(
    db: u64,
    branch: *const c_char,
    user: *const c_char,
    dbname: *const c_char,
    out: *mut u64,
) -> i32 {
    if out.is_null() {
        return fail("22023", "out must not be NULL");
    }
    let mut vals = [("main", branch, "branch"), ("postgres", user, "user"), ("postgres", dbname, "dbname")].map(|(d, p, w)| {
        if p.is_null() { Ok(d) } else { unsafe { str_arg(p, w) } }
    });
    let mut s = [""; 3];
    for (i, v) in vals.iter_mut().enumerate() {
        match v {
            Ok(x) => s[i] = x,
            Err(c) => return *c,
        }
    }
    match run(|| ffi::session_open(db, s[0], s[1], s[2])) {
        Ok(h) => {
            unsafe { *out = h };
            0
        }
        Err(c) => c,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pga_session_close(session: u64) -> i32 {
    run(|| ffi::session_close(session)).map_or_else(|c| c, |_| 0)
}

/// Execute one or more statements (simple-query semantics). On success `*out` is a result to be released
/// with `pga_result_free`. On error nothing is written and the last error is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_exec(session: u64, sql: *const c_char, out: *mut *mut PgaResult) -> i32 {
    if out.is_null() {
        return fail("22023", "out must not be NULL");
    }
    let sql = match unsafe { str_arg(sql, "sql") } {
        Ok(s) => s,
        Err(c) => return c,
    };
    match run(|| ffi::exec(session, sql)) {
        Ok(r) => {
            unsafe { *out = Box::into_raw(Box::new(convert(r))) };
            0
        }
        Err(c) => c,
    }
}

/// Execute one statement with text parameters (`params[i] == NULL` is SQL NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_exec_params(
    session: u64,
    sql: *const c_char,
    nparams: u32,
    params: *const *const c_char,
    out: *mut *mut PgaResult,
) -> i32 {
    if out.is_null() || (nparams > 0 && params.is_null()) {
        return fail("22023", "out/params must not be NULL");
    }
    let sql = match unsafe { str_arg(sql, "sql") } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut ps: Vec<Option<String>> = Vec::with_capacity(nparams as usize);
    for i in 0..nparams as usize {
        let p = unsafe { *params.add(i) };
        if p.is_null() {
            ps.push(None);
        } else {
            match unsafe { str_arg(p, "param") } {
                Ok(s) => ps.push(Some(s.to_string())),
                Err(c) => return c,
            }
        }
    }
    match run(|| ffi::exec_params(session, sql, &ps)) {
        Ok(r) => {
            unsafe { *out = Box::into_raw(Box::new(convert(vec![r]))) };
            0
        }
        Err(c) => c,
    }
}

/// Execute and return the results as a JSON document (free with `pga_string_free`); NULL on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_exec_json(session: u64, sql: *const c_char) -> *mut c_char {
    let sql = match unsafe { str_arg(sql, "sql") } {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match run(|| ffi::exec_json(session, sql)) {
        Ok(s) => cstring(&s).into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_free(r: *mut PgaResult) {
    if !r.is_null() {
        drop(unsafe { Box::from_raw(r) });
    }
}

fn stmt<'a>(r: *const PgaResult, i: u32) -> Option<&'a Stmt> {
    if r.is_null() {
        return None;
    }
    unsafe { &*r }.stmts.get(i as usize)
}

/// Number of statements in the result (0 for an empty query string).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_nstmts(r: *const PgaResult) -> u32 {
    if r.is_null() { 0 } else { unsafe { &*r }.stmts.len() as u32 }
}
/// Command tag, e.g. "SELECT 2", "INSERT 0 1". NULL if out of range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_tag(r: *const PgaResult, stmt_idx: u32) -> *const c_char {
    stmt(r, stmt_idx).map_or(std::ptr::null(), |s| s.tag.as_ptr())
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_ncols(r: *const PgaResult, stmt_idx: u32) -> u32 {
    stmt(r, stmt_idx).map_or(0, |s| s.columns.len() as u32)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_nrows(r: *const PgaResult, stmt_idx: u32) -> u32 {
    stmt(r, stmt_idx).map_or(0, |s| s.rows.len() as u32)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_colname(r: *const PgaResult, stmt_idx: u32, col: u32) -> *const c_char {
    stmt(r, stmt_idx).and_then(|s| s.columns.get(col as usize)).map_or(std::ptr::null(), |c| c.as_ptr())
}
/// Type OID of a column (0 if out of range).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_coltype(r: *const PgaResult, stmt_idx: u32, col: u32) -> u32 {
    stmt(r, stmt_idx).and_then(|s| s.type_oids.get(col as usize)).copied().unwrap_or(0)
}
/// Text value of a cell; NULL for SQL NULL or out of range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_result_value(r: *const PgaResult, stmt_idx: u32, row: u32, col: u32) -> *const c_char {
    stmt(r, stmt_idx)
        .and_then(|s| s.rows.get(row as usize))
        .and_then(|row| row.get(col as usize))
        .and_then(|v| v.as_ref())
        .map_or(std::ptr::null(), |c| c.as_ptr())
}

/// Transaction status: 'I' idle, 'T' in transaction, 'E' failed transaction; 0 on error.
#[unsafe(no_mangle)]
pub extern "C" fn pga_txn_status(session: u64) -> i32 {
    run(|| ffi::txn_status(session)).map_or(0, |b| b as i32)
}

#[unsafe(no_mangle)]
pub extern "C" fn pga_cancel(session: u64) -> i32 {
    run(|| ffi::cancel(session)).map_or_else(|c| c, |_| 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_branch_create(db: u64, name: *const c_char, from: *const c_char) -> i32 {
    let name = match unsafe { str_arg(name, "name") } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let from = match unsafe { str_arg(from, "from") } {
        Ok(s) => s,
        Err(c) => return c,
    };
    run(|| ffi::branch_create(db, name, from)).map_or_else(|c| c, |_| 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pga_branch_delete(db: u64, name: *const c_char) -> i32 {
    let name = match unsafe { str_arg(name, "name") } {
        Ok(s) => s,
        Err(c) => return c,
    };
    run(|| ffi::branch_delete(db, name)).map_or_else(|c| c, |_| 0)
}

/// Live branch names, newline-separated, valid until the next `pga_branch_list` call on this thread;
/// NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn pga_branch_list(db: u64) -> *const c_char {
    match run(|| ffi::branch_list(db)) {
        Ok(v) => SCRATCH.with(|s| {
            let c = cstring(&v.join("\n"));
            let mut slot = s.borrow_mut();
            *slot = Some(c);
            slot.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())
        }),
        Err(_) => std::ptr::null(),
    }
}
