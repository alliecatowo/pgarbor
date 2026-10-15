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
use std::io::{Read, Write};

use pgarbor_catalog::acl::RoleGraph;
use pgarbor_catalog::model::{acl_mode, GeneratedKind};
use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::{InsertSpec, Plan};
use pgarbor_sql::ir::{BoundCopy, BoundExpr, CopyDirection, CopyEndpoint, CopyFormat, CopyOptions, CopyTarget, TargetTable};
use pgarbor_types::context::IoCtx;
use pgarbor_types::io;
use pgarbor_types::{Datum, TypeId};

use crate::build::open_plan;
use crate::context::ExecContext;
use crate::dml_insert;
use crate::eval;
use crate::fk;
use crate::perm;
use crate::result::ExecResult;
use crate::rowstore::HeapCursor;
use crate::runtime::{Executor, Runtime};
use crate::tabledesc::{columns_to_input_refs, table_desc, ColDesc, TableDesc};

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
pub fn execute_copy(
    cx: &mut ExecContext,
    c: &BoundCopy,
    plan: Option<&Plan>,
    chan: &mut dyn CopyChannel,
) -> Result<ExecResult> {
    match c.direction {
        CopyDirection::To => exec_copy_to(cx, c, plan, chan),
        CopyDirection::From => exec_copy_from(cx, c, chan),
    }
}

// ---------------------------------------------------------------------------------------------
// Options and shared helpers
// ---------------------------------------------------------------------------------------------

/// Flush the output buffer once it holds this many bytes.
const FLUSH_AT: usize = 64 * 1024;
/// Read size for file endpoints.
const FILE_CHUNK: usize = 64 * 1024;
/// Rows fetched per step when COPY TO reads a query.
const FETCH_BATCH: usize = 256;
/// Binary header: signature (11) + flags (4) + extension length (4).
const BIN_FIXED: usize = 19;
const BIN_SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

/// Resolved options (bytes instead of chars).
struct Fmt<'a> {
    opts: &'a CopyOptions,
    format: CopyFormat,
    delim: u8,
    quote: u8,
    escape: u8,
    null: &'a [u8],
}

impl<'a> Fmt<'a> {
    fn new(opts: &'a CopyOptions) -> Result<Fmt<'a>> {
        Ok(Fmt {
            opts,
            format: opts.format,
            delim: one_byte(opts.delimiter, "delimiter")?,
            quote: one_byte(opts.quote, "quote")?,
            escape: one_byte(opts.escape, "escape")?,
            null: opts.null_string.as_bytes(),
        })
    }

    fn force_null(&self, attnum: i16) -> bool {
        self.opts.force_null.contains(&attnum)
    }

    fn force_not_null(&self, attnum: i16) -> bool {
        self.opts.force_not_null.contains(&attnum)
    }

    fn force_quote(&self, attnum: i16) -> bool {
        self.opts.force_quote_all || self.opts.force_quote.contains(&attnum)
    }
}

fn one_byte(c: char, what: &str) -> Result<u8> {
    if c.is_ascii() {
        Ok(c as u8)
    } else {
        Err(PgError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("COPY {what} must be a single one-byte character"),
        ))
    }
}

fn bad_format(message: impl Into<String>) -> PgError {
    PgError::new(SqlState(*b"22P04"), message)
}

/// Short input: wait for more data, or fail when the input has ended.
fn need_more(eof: bool) -> Result<Parse> {
    if eof {
        Err(bad_format("unexpected EOF in COPY data"))
    } else {
        Ok(None)
    }
}

fn utf8_error(bytes: &[u8], e: std::str::Utf8Error) -> PgError {
    let at = e.valid_up_to();
    let b = bytes.get(at).copied().unwrap_or(0);
    PgError::new(SqlState::CHARACTER_NOT_IN_REPERTOIRE, format!("invalid byte sequence for encoding \"UTF8\": 0x{b:02x}"))
}

/// Column of a table by attnum; dropped or missing columns are an executor bug.
fn col_of(td: &TableDesc, attnum: i16) -> Result<&ColDesc> {
    match td.col(attnum) {
        Some(c) if !c.dropped => Ok(c),
        _ => Err(PgError::internal(format!("attnum {attnum} is not a live column of relation \"{}\"", td.name))),
    }
}

/// Server-side file endpoints need `pg_read_server_files` / `pg_write_server_files` (or superuser).
fn require_server_file_role(cx: &ExecContext, direction: CopyDirection) -> Result<()> {
    let env = &cx.se.env;
    let snap = env.snapshot();
    let me = env.current_user();
    let (role_name, verb) = match direction {
        CopyDirection::From => ("pg_read_server_files", "from"),
        CopyDirection::To => ("pg_write_server_files", "to"),
    };
    let graph = RoleGraph::build(snap, env.database())?;
    if graph.is_superuser(me) {
        return Ok(());
    }
    if let Some(role) = snap.role_by_name(role_name) {
        if graph.has_privs_of_role(me, role.oid)? {
            return Ok(());
        }
    }
    Err(PgError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to COPY {verb} a file"))
        .with_detail(format!("Only roles with privileges of the \"{role_name}\" role may COPY {verb} a file.")))
}

/// Validate the endpoint against the direction and the current role.
fn check_endpoint(cx: &ExecContext, c: &BoundCopy) -> Result<()> {
    match (c.direction, &c.endpoint) {
        (CopyDirection::From, CopyEndpoint::Stdin) | (CopyDirection::To, CopyEndpoint::Stdout) => Ok(()),
        (CopyDirection::From, CopyEndpoint::Stdout) => {
            Err(PgError::new(SqlState::SYNTAX_ERROR, "syntax error at or near \"STDOUT\""))
        }
        (CopyDirection::To, CopyEndpoint::Stdin) => Err(PgError::new(SqlState::SYNTAX_ERROR, "syntax error at or near \"STDIN\"")),
        (_, CopyEndpoint::Program(_)) => Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "COPY with PROGRAM is not supported")),
        (CopyDirection::From, CopyEndpoint::File(_)) => require_server_file_role(cx, CopyDirection::From),
        (CopyDirection::To, CopyEndpoint::File(_)) => require_server_file_role(cx, CopyDirection::To),
    }
}

/// File channel for `COPY ... FROM/TO 'path'` (server side).
struct FileChannel {
    file: std::fs::File,
}

fn open_error(context: &str, e: &std::io::Error) -> PgError {
    if e.kind() == std::io::ErrorKind::NotFound {
        PgError::new(SqlState::UNDEFINED_FILE, format!("{context}: {e}"))
    } else {
        PgError::io(context, e)
    }
}

impl FileChannel {
    fn open(path: &str) -> Result<FileChannel> {
        match std::fs::File::open(path) {
            Ok(file) => Ok(FileChannel { file }),
            Err(e) => Err(open_error(&format!("could not open file \"{path}\" for reading"), &e)),
        }
    }

    fn create(path: &str) -> Result<FileChannel> {
        match std::fs::File::create(path) {
            Ok(file) => Ok(FileChannel { file }),
            Err(e) => Err(open_error(&format!("could not open file \"{path}\" for writing"), &e)),
        }
    }
}

impl CopyChannel for FileChannel {
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; FILE_CHUNK];
        match self.file.read(&mut buf) {
            Ok(0) => Ok(None),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) => Err(PgError::io("could not read from file", &e)),
        }
    }

    fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data).map_err(|e| PgError::io("could not write to file", &e))
    }
}

// ---------------------------------------------------------------------------------------------
// Input records
// ---------------------------------------------------------------------------------------------

/// One input field after decoding.
struct RawField {
    /// Decoded bytes (text escapes resolved; CSV quotes removed).
    bytes: Vec<u8>,
    /// CSV: the field was quoted.
    quoted: bool,
    /// The raw field matched the null string (text: before unescaping; CSV: decoded, unquoted or quoted).
    null_candidate: bool,
}

/// A parsed record: its fields, or the end-of-data marker.
enum Parsed {
    Row(Vec<RawField>),
    Eod,
}

/// Line terminator style (PG `EolType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eol {
    Nl,
    Cr,
    CrLf,
}

/// Result of one parse attempt: the record, bytes consumed and the terminator (`None` = no terminator).
type Parse = Option<(Parsed, usize, Option<Eol>)>;

/// Buffered reader over the channel. Records are parsed from the buffered bytes; a parse that
/// needs more input returns `None` and the reader fetches another chunk, so a field or line may
/// span any number of chunks.
struct Reader {
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    eol: Option<Eol>,
}

impl Reader {
    fn new() -> Reader {
        Reader { buf: Vec::new(), pos: 0, eof: false, eol: None }
    }

    fn fill(&mut self, chan: &mut dyn CopyChannel) -> Result<()> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        match chan.read_chunk()? {
            Some(data) => self.buf.extend_from_slice(&data),
            None => self.eof = true,
        }
        Ok(())
    }

    /// Next record (`None` = end of input).
    fn next_record(&mut self, chan: &mut dyn CopyChannel, fmt: &Fmt, ncols: usize) -> Result<Option<Parsed>> {
        loop {
            let attempt = match fmt.format {
                CopyFormat::Text => parse_text(&self.buf[self.pos..], self.eof, fmt)?,
                CopyFormat::Csv => parse_csv(&self.buf[self.pos..], self.eof, fmt)?,
                CopyFormat::Binary => parse_binary_row(&self.buf[self.pos..], self.eof, ncols)?,
            };
            if let Some((parsed, used, term)) = attempt {
                if let Some(t) = term {
                    self.note_eol(t)?;
                }
                self.pos += used;
                return Ok(Some(parsed));
            }
            if self.eof {
                return Ok(None);
            }
            self.fill(chan)?;
        }
    }

    /// The first terminator fixes the line style; later terminators must match it (PG rules).
    fn note_eol(&mut self, found: Eol) -> Result<()> {
        match self.eol {
            None => {
                self.eol = Some(found);
                Ok(())
            }
            Some(pref) if pref == found => Ok(()),
            Some(pref) => Err(eol_error(pref, found)),
        }
    }

    /// Binary file header (module docs). Consumes the 19 fixed bytes and the header extension.
    fn read_binary_header(&mut self, chan: &mut dyn CopyChannel) -> Result<()> {
        loop {
            let avail = &self.buf[self.pos..];
            if avail.len() >= BIN_FIXED {
                if avail[..11] != BIN_SIGNATURE[..] {
                    return Err(bad_format("COPY file signature not recognized"));
                }
                let flags = u32::from_be_bytes([avail[11], avail[12], avail[13], avail[14]]);
                if flags & (1 << 16) != 0 {
                    return Err(bad_format("invalid COPY file header (WITH OIDS)"));
                }
                if flags & 0xFFFF_0000 != 0 {
                    return Err(bad_format("unrecognized critical flags in COPY file header"));
                }
                let ext = i32::from_be_bytes([avail[15], avail[16], avail[17], avail[18]]);
                if ext < 0 {
                    return Err(bad_format("invalid COPY file header (wrong length)"));
                }
                let total = BIN_FIXED + ext as usize;
                if avail.len() >= total {
                    self.pos += total;
                    return Ok(());
                }
                if self.eof {
                    return Err(bad_format("invalid COPY file header (wrong length)"));
                }
            } else if self.eof {
                return Err(bad_format("COPY file signature not recognized"));
            }
            self.fill(chan)?;
        }
    }
}

fn eol_error(pref: Eol, found: Eol) -> PgError {
    let newline = || {
        PgError::new(SqlState(*b"22P04"), "literal newline found in data")
            .with_hint("Use \"\\n\" to represent newline.")
    };
    let carriage = || {
        PgError::new(SqlState(*b"22P04"), "literal carriage return found in data")
            .with_hint("Use \"\\r\" to represent carriage return.")
    };
    match found {
        Eol::Nl => newline(),
        Eol::Cr => carriage(),
        Eol::CrLf => {
            if pref == Eol::Cr {
                newline()
            } else {
                carriage()
            }
        }
    }
}

fn text_field(raw: &[u8], fmt: &Fmt) -> RawField {
    RawField { bytes: unescape_text(raw), quoted: false, null_candidate: raw == fmt.null }
}

fn csv_field(bytes: Vec<u8>, quoted: bool, fmt: &Fmt) -> RawField {
    let null_candidate = bytes.as_slice() == fmt.null;
    RawField { bytes, quoted, null_candidate }
}

fn finish_text(mut fields: Vec<RawField>, raw: &[u8], used: usize, term: Option<Eol>, fmt: &Fmt) -> (Parsed, usize, Option<Eol>) {
    if fields.is_empty() && raw == &b"\\."[..] {
        return (Parsed::Eod, used, term);
    }
    fields.push(text_field(raw, fmt));
    (Parsed::Row(fields), used, term)
}

/// Text format record (module docs). `buf` starts at a record boundary.
fn parse_text(buf: &[u8], eof: bool, fmt: &Fmt) -> Result<Parse> {
    let n = buf.len();
    let mut fields: Vec<RawField> = Vec::new();
    let mut raw: Vec<u8> = Vec::new();
    let mut i = 0usize;
    loop {
        if i >= n {
            if !eof || n == 0 {
                return Ok(None);
            }
            return Ok(Some(finish_text(fields, &raw, i, None, fmt)));
        }
        let c = buf[i];
        match c {
            b'\\' => {
                if i + 1 >= n {
                    if !eof {
                        return Ok(None);
                    }
                    raw.push(c);
                    i += 1;
                } else {
                    raw.push(c);
                    raw.push(buf[i + 1]);
                    i += 2;
                }
            }
            b'\n' => return Ok(Some(finish_text(fields, &raw, i + 1, Some(Eol::Nl), fmt))),
            b'\r' => {
                if i + 1 >= n {
                    if !eof {
                        return Ok(None);
                    }
                    return Ok(Some(finish_text(fields, &raw, i + 1, Some(Eol::Cr), fmt)));
                }
                if buf[i + 1] == b'\n' {
                    return Ok(Some(finish_text(fields, &raw, i + 2, Some(Eol::CrLf), fmt)));
                }
                return Ok(Some(finish_text(fields, &raw, i + 1, Some(Eol::Cr), fmt)));
            }
            _ if c == fmt.delim => {
                fields.push(text_field(&raw, fmt));
                raw.clear();
                i += 1;
            }
            _ => {
                raw.push(c);
                i += 1;
            }
        }
    }
}

fn finish_csv(mut fields: Vec<RawField>, cur: Vec<u8>, quoted: bool, used: usize, term: Option<Eol>, fmt: &Fmt) -> (Parsed, usize, Option<Eol>) {
    if fields.is_empty() && !quoted && cur.as_slice() == &b"\\."[..] {
        return (Parsed::Eod, used, term);
    }
    fields.push(csv_field(cur, quoted, fmt));
    (Parsed::Row(fields), used, term)
}

/// CSV format record (module docs). Newlines inside quotes are data.
fn parse_csv(buf: &[u8], eof: bool, fmt: &Fmt) -> Result<Parse> {
    let n = buf.len();
    let mut fields: Vec<RawField> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut quoted = false;
    let mut in_quote = false;
    let mut i = 0usize;
    loop {
        if i >= n {
            if !eof || n == 0 {
                return Ok(None);
            }
            if in_quote {
                return Err(bad_format("unterminated CSV quoted field"));
            }
            return Ok(Some(finish_csv(fields, cur, quoted, i, None, fmt)));
        }
        let c = buf[i];
        if in_quote {
            if c == fmt.escape && fmt.escape != fmt.quote {
                if i + 1 >= n {
                    if !eof {
                        return Ok(None);
                    }
                    cur.push(c);
                    i += 1;
                } else {
                    let d = buf[i + 1];
                    if d == fmt.quote || d == fmt.escape {
                        cur.push(d);
                    } else {
                        cur.push(c);
                        cur.push(d);
                    }
                    i += 2;
                }
            } else if c == fmt.quote {
                if i + 1 >= n && !eof {
                    return Ok(None);
                }
                if i + 1 < n && buf[i + 1] == fmt.quote {
                    cur.push(c);
                    i += 2;
                } else {
                    in_quote = false;
                    i += 1;
                }
            } else {
                cur.push(c);
                i += 1;
            }
            continue;
        }
        if c == fmt.quote {
            in_quote = true;
            quoted = true;
            i += 1;
            continue;
        }
        if c == fmt.delim {
            fields.push(csv_field(std::mem::take(&mut cur), quoted, fmt));
            quoted = false;
            i += 1;
            continue;
        }
        match c {
            b'\n' => return Ok(Some(finish_csv(fields, cur, quoted, i + 1, Some(Eol::Nl), fmt))),
            b'\r' => {
                if i + 1 >= n {
                    if !eof {
                        return Ok(None);
                    }
                    return Ok(Some(finish_csv(fields, cur, quoted, i + 1, Some(Eol::Cr), fmt)));
                }
                if buf[i + 1] == b'\n' {
                    return Ok(Some(finish_csv(fields, cur, quoted, i + 2, Some(Eol::CrLf), fmt)));
                }
                return Ok(Some(finish_csv(fields, cur, quoted, i + 1, Some(Eol::Cr), fmt)));
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
}

/// Binary tuple: field count, then length-prefixed fields. `-1` count is the trailer.
fn parse_binary_row(buf: &[u8], eof: bool, ncols: usize) -> Result<Parse> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf.len() < 2 {
        return need_more(eof);
    }
    let cnt = i16::from_be_bytes([buf[0], buf[1]]);
    if cnt == -1 {
        return Ok(Some((Parsed::Eod, 2, None)));
    }
    if cnt < 0 || cnt as usize != ncols {
        return Err(bad_format(format!("row field count is {cnt}, expected {ncols}")));
    }
    let mut pos = 2usize;
    let mut fields = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        if buf.len() < pos + 4 {
            return need_more(eof);
        }
        let len = i32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        pos += 4;
        if len == -1 {
            fields.push(RawField { bytes: Vec::new(), quoted: false, null_candidate: true });
            continue;
        }
        if len < 0 {
            return Err(bad_format("invalid field size"));
        }
        let len = len as usize;
        if buf.len() < pos + len {
            return need_more(eof);
        }
        fields.push(RawField { bytes: buf[pos..pos + len].to_vec(), quoted: false, null_candidate: false });
        pos += len;
    }
    Ok(Some((Parsed::Row(fields), pos, None)))
}

/// Text input escapes (module docs): `\b \f \n \r \t \v`, octal `\ooo`, hex `\xhh`, else the char.
fn unescape_text(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        let c = raw[i];
        if c != b'\\' || i + 1 >= raw.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let d = raw[i + 1];
        i += 2;
        match d {
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'x' => {
                let mut v: u32 = 0;
                let mut k = 0;
                while k < 2 && i < raw.len() {
                    match hex_value(raw[i]) {
                        Some(h) => {
                            v = v * 16 + h;
                            i += 1;
                            k += 1;
                        }
                        None => break,
                    }
                }
                if k == 0 {
                    out.push(b'x');
                } else {
                    out.push(v as u8);
                }
            }
            b'0'..=b'7' => {
                let mut v: u32 = (d - b'0') as u32;
                let mut k = 1;
                while k < 3 && i < raw.len() && (b'0'..=b'7').contains(&raw[i]) {
                    v = v * 8 + (raw[i] - b'0') as u32;
                    i += 1;
                    k += 1;
                }
                out.push(v as u8);
            }
            other => out.push(other),
        }
    }
    out
}

fn hex_value(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'a'..=b'f' => Some((c - b'a' + 10) as u32),
        b'A'..=b'F' => Some((c - b'A' + 10) as u32),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------------------------

/// One output column: the row slot it reads, its type and CSV force-quote flag.
struct OutCol {
    idx: usize,
    ty: TypeId,
    force_quote: bool,
}

/// Text output escapes (PG `CopyAttributeOutText`): delimiter gets a backslash, backslash doubles,
/// `\b \f \n \r \t \v` become letters.
fn escape_text_into(out: &mut Vec<u8>, bytes: &[u8], fmt: &Fmt) {
    for &c in bytes {
        let letter = match c {
            8 => Some(b'b'),
            12 => Some(b'f'),
            b'\n' => Some(b'n'),
            b'\r' => Some(b'r'),
            b'\t' => Some(b't'),
            11 => Some(b'v'),
            _ => None,
        };
        if let Some(l) = letter {
            out.push(b'\\');
            out.push(l);
        } else if c == b'\\' || c == fmt.delim {
            out.push(b'\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
}

/// CSV output (PG `CopyAttributeOutCSV`): quote when forced, equal to the null string, containing
/// the delimiter, quote, newline or carriage return, or a lone `\.` (single column).
fn csv_field_into(out: &mut Vec<u8>, bytes: &[u8], force: bool, single: bool, fmt: &Fmt) {
    let needs_quote = force
        || bytes == fmt.null
        || (single && bytes == &b"\\."[..])
        || bytes.iter().any(|&c| c == fmt.delim || c == fmt.quote || c == b'\n' || c == b'\r');
    if needs_quote {
        out.push(fmt.quote);
        for &c in bytes {
            if c == fmt.quote || c == fmt.escape {
                out.push(fmt.escape);
            }
            out.push(c);
        }
        out.push(fmt.quote);
    } else {
        out.extend_from_slice(bytes);
    }
}

fn write_header(buf: &mut Vec<u8>, fmt: &Fmt, names: &[String]) {
    if fmt.format == CopyFormat::Binary {
        buf.extend_from_slice(BIN_SIGNATURE);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        return;
    }
    if !fmt.opts.header {
        return;
    }
    let single = names.len() == 1;
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            buf.push(fmt.delim);
        }
        if fmt.format == CopyFormat::Csv {
            csv_field_into(buf, name.as_bytes(), false, single, fmt);
        } else {
            escape_text_into(buf, name.as_bytes(), fmt);
        }
    }
    buf.push(b'\n');
}

fn write_trailer(buf: &mut Vec<u8>, fmt: &Fmt) {
    if fmt.format == CopyFormat::Binary {
        buf.extend_from_slice(&(-1i16).to_be_bytes());
    }
}

/// One row out (module docs). `row` holds the full row; `cols` select the slots.
fn format_row(ctx: &IoCtx<'_>, fmt: &Fmt, cols: &[OutCol], row: &[Datum], out: &mut Vec<u8>) -> Result<()> {
    if fmt.format == CopyFormat::Binary {
        out.extend_from_slice(&(cols.len() as i16).to_be_bytes());
        for col in cols {
            let v = row.get(col.idx).ok_or_else(|| PgError::internal("COPY output row is too short"))?;
            if v.is_null() {
                out.extend_from_slice(&(-1i32).to_be_bytes());
                continue;
            }
            let bytes = io::send(ctx, col.ty, v)?;
            let len = i32::try_from(bytes.len()).map_err(|_| PgError::new(SqlState(*b"54000"), "COPY field is too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&bytes);
        }
        return Ok(());
    }
    let csv = fmt.format == CopyFormat::Csv;
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            out.push(fmt.delim);
        }
        let v = row.get(col.idx).ok_or_else(|| PgError::internal("COPY output row is too short"))?;
        if v.is_null() {
            out.extend_from_slice(fmt.null);
            continue;
        }
        let text = io::output(ctx, col.ty, v)?;
        if csv {
            csv_field_into(out, text.as_bytes(), col.force_quote, cols.len() == 1, fmt);
        } else {
            escape_text_into(out, text.as_bytes(), fmt);
        }
    }
    out.push(b'\n');
    Ok(())
}

fn flush_if_full(buf: &mut Vec<u8>, out: &mut dyn CopyChannel) -> Result<()> {
    if buf.len() >= FLUSH_AT {
        out.write_chunk(&buf[..])?;
        buf.clear();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// COPY TO
// ---------------------------------------------------------------------------------------------

fn exec_copy_to(cx: &mut ExecContext, c: &BoundCopy, plan: Option<&Plan>, chan: &mut dyn CopyChannel) -> Result<ExecResult> {
    let fmt = Fmt::new(&c.options)?;
    let table_info = match &c.target {
        CopyTarget::Table { table, columns } => {
            let td = table_desc(cx, table.oid)?;
            let attnums: Vec<i16> = if columns.is_empty() {
                td.live_cols().map(|col| col.attnum).collect()
            } else {
                columns.clone()
            };
            for a in &attnums {
                col_of(&td, *a)?;
            }
            perm::require_relation_privilege(&cx.se, td.oid, acl_mode::SELECT, &attnums)?;
            Some((td, attnums))
        }
        CopyTarget::Query(_) => None,
    };
    check_endpoint(cx, c)?;
    let mut file_out = match &c.endpoint {
        CopyEndpoint::File(p) => Some(FileChannel::create(p)?),
        _ => None,
    };
    let out: &mut dyn CopyChannel = match file_out.as_mut() {
        Some(f) => f,
        None => &mut *chan,
    };
    let count = match table_info {
        Some((td, attnums)) => copy_to_table_rows(cx, &td, &attnums, &fmt, out)?,
        None => {
            let plan = plan.ok_or_else(|| PgError::internal("COPY (query) TO requires a planned query"))?;
            copy_to_query_rows(cx, plan, &fmt, out)?
        }
    };
    Ok(ExecResult::command(format!("COPY {count}")))
}

fn copy_to_table_rows(cx: &mut ExecContext, td: &TableDesc, attnums: &[i16], fmt: &Fmt, out: &mut dyn CopyChannel) -> Result<u64> {
    let mut outcols = Vec::with_capacity(attnums.len());
    let mut names = Vec::with_capacity(attnums.len());
    let mut needed = vec![false; td.ncols()];
    for a in attnums {
        let col = col_of(td, *a)?;
        outcols.push(OutCol { idx: (*a - 1) as usize, ty: col.ty, force_quote: fmt.force_quote(*a) });
        names.push(col.name.clone());
        needed[(*a - 1) as usize] = true;
    }
    let mut buf: Vec<u8> = Vec::new();
    write_header(&mut buf, fmt, &names);

    let mut rt = Runtime::default();
    let mut ex = Executor::new(&mut rt, cx);
    let mut cursor = HeapCursor::new(td.oid);
    let mut count = 0u64;
    let mut ticks = 0u32;
    loop {
        ex.tick(&mut ticks)?;
        let next = cursor.next(&mut ex, td, Some(needed.as_slice()))?;
        let row = match next {
            Some((_rowid, row)) => row,
            None => break,
        };
        {
            let ctx = ex.se().io_ctx();
            format_row(&ctx, fmt, &outcols, &row, &mut buf)?;
        }
        count += 1;
        flush_if_full(&mut buf, out)?;
    }
    write_trailer(&mut buf, fmt);
    if !buf.is_empty() {
        out.write_chunk(&buf)?;
    }
    Ok(count)
}

fn copy_to_query_rows(cx: &mut ExecContext, plan: &Plan, fmt: &Fmt, out: &mut dyn CopyChannel) -> Result<u64> {
    let outcols: Vec<OutCol> = plan
        .output
        .iter()
        .enumerate()
        .map(|(i, d)| OutCol { idx: i, ty: d.type_id, force_quote: fmt.opts.force_quote_all })
        .collect();
    let names: Vec<String> = plan.output.iter().map(|d| d.name.clone()).collect();
    let mut buf: Vec<u8> = Vec::new();
    write_header(&mut buf, fmt, &names);

    let mut stream = open_plan(plan, cx)?;
    let mut count = 0u64;
    loop {
        let rows = stream.fetch(cx, Some(FETCH_BATCH))?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            {
                let ctx = cx.se.io_ctx();
                format_row(&ctx, fmt, &outcols, row, &mut buf)?;
            }
            count += 1;
        }
        flush_if_full(&mut buf, out)?;
    }
    write_trailer(&mut buf, fmt);
    if !buf.is_empty() {
        out.write_chunk(&buf)?;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------------------------
// COPY FROM
// ---------------------------------------------------------------------------------------------

fn exec_copy_from(cx: &mut ExecContext, c: &BoundCopy, chan: &mut dyn CopyChannel) -> Result<ExecResult> {
    let (table, columns): (&TargetTable, &Vec<i16>) = match &c.target {
        CopyTarget::Table { table, columns } => (table.as_ref(), columns),
        CopyTarget::Query(_) => {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "COPY FROM a query is not supported"));
        }
    };
    let fmt = Fmt::new(&c.options)?;
    let td = table_desc(cx, table.oid)?;
    let attnums: Vec<i16> = if columns.is_empty() {
        table.live_columns().filter(|tc| tc.generated.is_none()).map(|tc| tc.attnum).collect()
    } else {
        columns.clone()
    };
    for a in &attnums {
        col_of(&td, *a)?;
    }
    perm::require_relation_privilege(&cx.se, td.oid, acl_mode::INSERT, &attnums)?;
    check_endpoint(cx, c)?;

    let spec = build_spec(table, &attnums)?;
    let where_expr: Option<BoundExpr> = match &c.where_ {
        Some(w) => Some(columns_to_input_refs(w.clone())?),
        None => None,
    };
    let mut file_in = match &c.endpoint {
        CopyEndpoint::File(p) => Some(FileChannel::open(p)?),
        _ => None,
    };
    let input: &mut dyn CopyChannel = match file_in.as_mut() {
        Some(f) => f,
        None => &mut *chan,
    };
    let count = copy_from_rows(cx, &td, &attnums, &spec, where_expr.as_ref(), &fmt, input)?;
    Ok(ExecResult::command(format!("COPY {count}")))
}

/// The insert pipeline for COPY: defaults of unlisted columns and stored generated columns.
/// Columns without a default stay NULL (`dml_insert::assemble_row`).
fn build_spec(table: &TargetTable, attnums: &[i16]) -> Result<InsertSpec> {
    let mut defaults = Vec::new();
    let mut generated = Vec::new();
    for tc in table.columns.iter() {
        if tc.dropped {
            continue;
        }
        if let Some((kind, expr)) = &tc.generated {
            if matches!(kind, GeneratedKind::Stored) {
                generated.push((tc.attnum, columns_to_input_refs(expr.clone())?));
            }
            continue;
        }
        if attnums.contains(&tc.attnum) {
            continue;
        }
        if let Some(expr) = &tc.default {
            defaults.push((tc.attnum, columns_to_input_refs(expr.clone())?));
        }
    }
    Ok(InsertSpec { columns: attnums.to_vec(), defaults, generated, on_conflict: None })
}

/// Datum of one input field (NULL rules of the format, then the type's input or receive function).
fn field_datum(ctx: &IoCtx<'_>, fmt: &Fmt, td: &TableDesc, attnum: i16, f: &RawField) -> Result<Datum> {
    let is_null = f.null_candidate
        && if f.quoted {
            fmt.force_null(attnum)
        } else {
            !fmt.force_not_null(attnum)
        };
    if is_null {
        return Ok(Datum::Null);
    }
    let ty = col_of(td, attnum)?.ty;
    match fmt.format {
        CopyFormat::Binary => io::recv(ctx, ty, &f.bytes),
        _ => {
            let s = std::str::from_utf8(&f.bytes).map_err(|e| utf8_error(&f.bytes, e))?;
            io::input(ctx, ty, s)
        }
    }
}

fn copy_from_rows(
    cx: &mut ExecContext,
    td: &TableDesc,
    attnums: &[i16],
    spec: &InsertSpec,
    where_expr: Option<&BoundExpr>,
    fmt: &Fmt,
    input: &mut dyn CopyChannel,
) -> Result<u64> {
    let mut reader = Reader::new();
    if fmt.format == CopyFormat::Binary {
        reader.read_binary_header(input)?;
    }
    let skip_header = fmt.opts.header && fmt.format != CopyFormat::Binary;

    let mut rt = Runtime::default();
    let mut ex = Executor::new(&mut rt, cx);
    let mut count = 0u64;
    let mut ticks = 0u32;
    let mut first = true;
    loop {
        let fields = match reader.next_record(input, fmt, attnums.len())? {
            None | Some(Parsed::Eod) => break,
            Some(Parsed::Row(fields)) => fields,
        };
        let was_first = std::mem::replace(&mut first, false);
        if was_first && skip_header {
            continue;
        }
        ex.tick(&mut ticks)?;
        if fields.len() < attnums.len() {
            let name = col_of(td, attnums[fields.len()])?.name.clone();
            return Err(bad_format(format!("missing data for column \"{name}\"")));
        }
        if fields.len() > attnums.len() {
            return Err(bad_format("extra data after last expected column"));
        }
        let mut values: Vec<Datum> = Vec::with_capacity(attnums.len());
        {
            let ctx = ex.se().io_ctx();
            for (f, a) in fields.iter().zip(attnums.iter()) {
                values.push(field_datum(&ctx, fmt, td, *a, f)?);
            }
        }
        let row = dml_insert::assemble_row(&mut ex, td, spec, &values)?;
        if let Some(w) = where_expr {
            if !eval::eval_qual(&mut ex, w, &row)? {
                continue;
            }
        }
        dml_insert::insert_row(&mut ex, td, row)?;
        count += 1;
    }
    fk::run_pending(&mut ex)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Channel that hands out its input in fixed-size pieces (chunk-boundary tests).
    struct Chunks {
        pieces: VecDeque<Vec<u8>>,
        out: Vec<u8>,
    }

    impl Chunks {
        fn split(data: &[u8], size: usize) -> Chunks {
            Chunks { pieces: data.chunks(size).map(|c| c.to_vec()).collect(), out: Vec::new() }
        }
    }

    impl CopyChannel for Chunks {
        fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.pieces.pop_front())
        }
        fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
            self.out.extend_from_slice(data);
            Ok(())
        }
    }

    fn opts(format: CopyFormat) -> CopyOptions {
        let (delim, null, quote) = match format {
            CopyFormat::Csv => (',', String::new(), '"'),
            _ => ('\t', "\\N".to_string(), '"'),
        };
        CopyOptions {
            format,
            delimiter: delim,
            null_string: null,
            header: false,
            quote,
            escape: quote,
            force_quote_all: false,
            force_quote: Vec::new(),
            force_not_null: Vec::new(),
            force_null: Vec::new(),
            encoding: None,
            freeze: false,
            default_string: None,
        }
    }

    /// All records of `data` read through `chan` (chunked), as decoded field bytes (`None` = NULL).
    fn read_all(data: &[u8], chunk: usize, o: &CopyOptions, ncols: usize) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        let fmt = Fmt::new(o)?;
        let mut chan = Chunks::split(data, chunk);
        let mut reader = Reader::new();
        if fmt.format == CopyFormat::Binary {
            reader.read_binary_header(&mut chan)?;
        }
        let mut rows = Vec::new();
        while let Some(p) = reader.next_record(&mut chan, &fmt, ncols)? {
            match p {
                Parsed::Eod => break,
                Parsed::Row(fields) => rows.push(
                    fields
                        .into_iter()
                        .map(|f| if f.null_candidate && !f.quoted { None } else { Some(f.bytes) })
                        .collect(),
                ),
            }
        }
        Ok(rows)
    }

    fn s(v: &str) -> Option<Vec<u8>> {
        Some(v.as_bytes().to_vec())
    }

    #[test]
    fn unescape_text_rules() {
        assert_eq!(unescape_text(b"a\\tb"), b"a\tb".to_vec());
        assert_eq!(unescape_text(b"\\b\\f\\n\\r\\v"), vec![8, 12, b'\n', b'\r', 11]);
        assert_eq!(unescape_text(b"\\x41\\x4"), b"A\x04".to_vec());
        assert_eq!(unescape_text(b"\\xg"), b"xg".to_vec());
        assert_eq!(unescape_text(b"\\101\\0"), b"A\0".to_vec());
        assert_eq!(unescape_text(b"\\\\\\q"), b"\\q".to_vec());
        assert_eq!(unescape_text(b"abc\\"), b"abc\\".to_vec());
    }

    #[test]
    fn text_output_escapes() {
        let o = opts(CopyFormat::Text);
        let fmt = Fmt::new(&o).unwrap();
        let mut out = Vec::new();
        escape_text_into(&mut out, b"a\tb\\c\nd", &fmt);
        assert_eq!(out, b"a\\tb\\\\c\\nd".to_vec());
        let mut out = Vec::new();
        escape_text_into(&mut out, b"x,y", &fmt);
        assert_eq!(out, b"x,y".to_vec());
    }

    #[test]
    fn text_round_trip_null_and_columns() {
        let o = opts(CopyFormat::Text);
        let rows = read_all(b"1\tfoo\n2\t\\N\n", 1000, &o, 2).unwrap();
        assert_eq!(rows, vec![vec![s("1"), s("foo")], vec![s("2"), None]]);
    }

    #[test]
    fn text_chunk_boundaries_do_not_change_result() {
        let o = opts(CopyFormat::Text);
        let data = b"ab\\\ncd\t\\N\nlast\tx\\\ty\n";
        let whole = read_all(data, data.len(), &o, 2).unwrap();
        for size in 1..=data.len() {
            assert_eq!(read_all(data, size, &o, 2).unwrap(), whole, "chunk size {size}");
        }
        assert_eq!(whole.len(), 2);
    }

    #[test]
    fn text_end_marker_stops_input() {
        let o = opts(CopyFormat::Text);
        let rows = read_all(b"a\nb\n\\.\nignored\n", 3, &o, 1).unwrap();
        assert_eq!(rows, vec![vec![s("a")], vec![s("b")]]);
    }

    #[test]
    fn literal_carriage_return_is_rejected() {
        let o = opts(CopyFormat::Text);
        let err = read_all(b"a\nb\rc\n", 100, &o, 1).unwrap_err();
        assert_eq!(err.message, "literal carriage return found in data");
        let err = read_all(b"a\r\nb\n", 100, &o, 1).unwrap_err();
        assert_eq!(err.message, "literal newline found in data");
    }

    #[test]
    fn csv_quotes_and_embedded_newlines() {
        let o = opts(CopyFormat::Csv);
        let rows = read_all(b"\"a,b\",\"c\"\"d\"\n\"multi\nline\",\n", 2, &o, 2).unwrap();
        assert_eq!(rows, vec![vec![s("a,b"), s("c\"d")], vec![s("multi\nline"), None]]);
    }

    #[test]
    fn csv_quoted_null_is_not_null() {
        let o = opts(CopyFormat::Csv);
        let rows = read_all(b"\"\",x\n", 100, &o, 2).unwrap();
        assert_eq!(rows, vec![vec![s(""), s("x")]]);
    }

    #[test]
    fn csv_output_quoting_round_trip() {
        let o = opts(CopyFormat::Csv);
        let fmt = Fmt::new(&o).unwrap();
        let mut out = Vec::new();
        csv_field_into(&mut out, b"a,b\nc\"d", false, false, &fmt);
        assert_eq!(out, b"\"a,b\nc\"\"d\"".to_vec());
        let mut out = Vec::new();
        csv_field_into(&mut out, b"", false, false, &fmt);
        assert_eq!(out, b"\"\"".to_vec());
        let mut out = Vec::new();
        csv_field_into(&mut out, b"plain", false, false, &fmt);
        assert_eq!(out, b"plain".to_vec());
        let mut out = Vec::new();
        csv_field_into(&mut out, b"\\.", false, true, &fmt);
        assert_eq!(out, b"\"\\.\"".to_vec());
    }

    #[test]
    fn binary_round_trip_layout() {
        let o = opts(CopyFormat::Binary);
        let mut data = Vec::new();
        data.extend_from_slice(BIN_SIGNATURE);
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&2i16.to_be_bytes());
        data.extend_from_slice(&4i32.to_be_bytes());
        data.extend_from_slice(&7i32.to_be_bytes());
        data.extend_from_slice(&(-1i32).to_be_bytes());
        data.extend_from_slice(&(-1i16).to_be_bytes());
        let rows = read_all(&data, 5, &o, 2).unwrap();
        assert_eq!(rows, vec![vec![Some(7i32.to_be_bytes().to_vec()), None]]);
    }

    #[test]
    fn binary_bad_signature_is_22p04() {
        let o = opts(CopyFormat::Binary);
        let err = read_all(b"NOTPGCOPY-------------", 100, &o, 1).unwrap_err();
        assert_eq!(err.message, "COPY file signature not recognized");
    }

    #[test]
    fn binary_field_count_mismatch() {
        let o = opts(CopyFormat::Binary);
        let mut data = Vec::new();
        data.extend_from_slice(BIN_SIGNATURE);
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&3i16.to_be_bytes());
        let err = read_all(&data, 100, &o, 2).unwrap_err();
        assert_eq!(err.message, "row field count is 3, expected 2");
    }
}
