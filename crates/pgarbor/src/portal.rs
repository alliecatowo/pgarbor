//! Portals, row limits, suspension and value encoding (lease API-12, north-star).
//! PG: `tcop/pquery.c` (`PortalRun`, `PortalRunSelect`, `FillPortalStore`), `tcop/postgres.c`
//! (`exec_bind_message`, `exec_execute_message`), `access/common/printtup.c`.
//!
//! ## Portal
//! A [`Portal`] binds a prepared statement to parameter values and result format codes
//! (`extended::bind`). Execution is lazy: nothing runs at Bind (except planning), the first `Execute` runs
//! the statement.
//! * Plannable statements (queries, DML): the first Execute opens a `pgarbor_exec::PlanStream`
//!   (`open_plan` inside `with_exec`) stored in `PortalState::Streaming`; each Execute pulls up to
//!   `max_rows` rows (`0` = all) with `PlanStream::fetch`; `stream.is_done()` -> `PortalStatus::Complete{tag:
//!   stream.command_tag()}` else `Suspended` (PortalSuspended; the next Execute continues).
//!   DML without RETURNING performs all its work on the first Execute and completes immediately.
//! * Other statements (DDL, utility, SHOW, session statements): run once through `run_statement`-style
//!   execution; a result with rows is buffered (`PortalState::Buffered`) and served in `max_rows` chunks.
//! * `Execute` on a completed portal returns zero rows and `Complete{tag}` again (PG raises nothing for
//!   re-executing a finished portal).
//! * All portals die at transaction end (`PortalStore::clear` from commit/abort); the unnamed portal is
//!   replaced by the next Bind. Dropping a `Streaming` portal just drops its operators.
//! * A suspended portal keeps the TRANSACTION open: the session stays in the implicit block (pipelining)
//!   until Sync.
//!
//! ## Encoding
//! [`Session::encode_value`] encodes one datum for the wire: text format = `pgarbor_types::io::output`
//! under the session `IoConfig` (`IoCtx` from the live env); binary = `io::send`. NULL -> `None`.
//! [`Session::encode_row`] applies per-column formats from `formats` with PG's rule: empty list = all
//! text, one entry = all columns, else one per column (mismatch 08P01 `bind message has N result formats
//! but query has M columns`). [`Session::decode_param`] is the inverse for Bind parameters
//! (`io::input` / `io::recv` with the parameter's type).
//! The IoCtx needs a catalog env: when no transaction snapshot exists use `self.env` (last known).
//!
//! ## Implementation notes (API-12)
//! * Only `BoundStatement::Query` with a bind-time plan streams through `PlanStream`. Every other statement
//!   (DML, DDL, session statements) is re-parsed from the portal text and run once through `run_statement`,
//!   so it gets the full statement pipeline (activity, snapshot, catalog retry, COPY channel). Its
//!   result is buffered and served in `max_rows` chunks; DML therefore completes on the first Execute.
//! * Completion follows PG `PortalRunSelect` (`pquery.c`): the portal is at its end only when a fetch
//!   returns fewer rows than requested, so `Execute(1)` over 3 rows yields Suspended, Suspended,
//!   Suspended, then Complete `SELECT 3` with zero rows.
//! * The portal is taken out of the store while it runs, so `with_exec(&mut self)` can borrow the session.
//!   An error during Execute leaves the portal `Done` with an empty tag; the transaction is aborted by the
//!   caller, which also clears the portal store.
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_exec::{open_plan, PlanStream, Row};
use pgarbor_plan::plan::{Plan, PlanKind};
use pgarbor_sql::ir::BoundStatement;
use pgarbor_sql::parse;
use pgarbor_types::io;
use pgarbor_types::registry;
use pgarbor_types::{Datum, IoConfig, IoCtx, TypeEnv, TypeId};

use crate::guc_derive;
use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::types::{
    ColumnInfo, EncodedRow, FormatCode, PortalOutput, PortalStatus, StatementOutput, StatementResult,
};

/// Execution state of a portal.
pub enum PortalState {
    /// Bound, not started.
    Ready,
    /// A running plan (queries and DML).
    Streaming(PlanStream),
    /// Rows already produced (utility statements with a result set), not yet delivered.
    Buffered { rows: std::collections::VecDeque<Vec<Datum>>, tag: String },
    /// Finished; the tag is kept for repeated Execute.
    Done { tag: String },
}

/// A bound statement ready to run (module docs).
pub struct Portal {
    pub name: String,
    pub stmt_name: String,
    pub text: String,
    pub statement: Option<BoundStatement>,
    pub plan: Option<Rc<Plan>>,
    pub params: ParamValues,
    pub result_formats: Vec<FormatCode>,
    pub columns: Option<Vec<ColumnInfo>>,
    pub state: PortalState,
}

/// Open portals by name (`""` = unnamed).
#[derive(Default)]
pub struct PortalStore {
    pub(crate) map: HashMap<String, Portal>,
}

impl PortalStore {
    pub fn get(&self, name: &str) -> Option<&Portal> {
        self.map.get(name)
    }
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Portal> {
        self.map.get_mut(name)
    }
    pub fn insert(&mut self, p: Portal) {
        self.map.insert(p.name.clone(), p);
    }
    pub fn remove(&mut self, name: &str) -> Option<Portal> {
        self.map.remove(name)
    }
    pub fn clear(&mut self) {
        self.map.clear();
    }
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers (no session needed; covered by the unit tests below)
// ---------------------------------------------------------------------------------------------

/// Completion status for a command tag: an empty tag is an empty query (EmptyQueryResponse).
fn status_for(tag: &str) -> PortalStatus {
    if tag.is_empty() {
        PortalStatus::Empty
    } else {
        PortalStatus::Complete { tag: tag.to_string() }
    }
}

/// Command name used in 25006 read-only errors for a streamed plan (`PG CommandCounterIncrement` style
/// names as the executor prints them).
fn plan_command(kind: PlanKind) -> &'static str {
    match kind {
        PlanKind::Select | PlanKind::Populate => "SELECT",
        PlanKind::Insert => "INSERT",
        PlanKind::Update => "UPDATE",
        PlanKind::Delete => "DELETE",
        PlanKind::Merge => "MERGE",
    }
}

fn portal_output(
    rows: Vec<EncodedRow>,
    status: PortalStatus,
    notices: Vec<PgError>,
    param_status: Vec<(String, String)>,
) -> PortalOutput {
    PortalOutput { rows, status, notices, param_status }
}

/// Per-column result formats from the Bind message (PG `exec_bind_message`): empty = all text, one entry
/// = all columns, otherwise exactly one per column.
fn column_formats(ncols: usize, formats: &[FormatCode]) -> Result<Vec<FormatCode>> {
    match formats {
        [] => Ok(vec![FormatCode::Text; ncols]),
        [one] => Ok(vec![*one; ncols]),
        many if many.len() == ncols => Ok(many.to_vec()),
        many => Err(PgError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("bind message has {} result formats but query has {} columns", many.len(), ncols),
        )),
    }
}

/// Invalid UTF-8 in a text-format parameter (22021, PG `report_invalid_encoding`). Prints the offending
/// bytes as `0xNN`.
fn invalid_utf8(bytes: &[u8], valid_up_to: usize, error_len: Option<usize>) -> PgError {
    let end = match error_len {
        Some(n) => valid_up_to.saturating_add(n),
        None => bytes.len(),
    };
    let shown: Vec<String> = bytes
        .get(valid_up_to..end.min(bytes.len()))
        .unwrap_or(&[])
        .iter()
        .map(|b| format!("0x{b:02x}"))
        .collect();
    PgError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!("invalid byte sequence for encoding \"UTF8\": {}", shown.join(" ")),
    )
}

/// One datum in the given format; NULL -> `None`.
fn encode_datum(ctx: &IoCtx, ty: TypeId, value: &Datum, format: FormatCode) -> Result<Option<Vec<u8>>> {
    if value.is_null() {
        return Ok(None);
    }
    match format {
        FormatCode::Text => Ok(Some(io::output(ctx, ty, value)?.into_bytes())),
        FormatCode::Binary => Ok(Some(io::send(ctx, ty, value)?)),
    }
}

/// One DataRow. `formats` must hold one entry per column (see [`column_formats`]).
fn encode_row_with(
    ctx: &IoCtx,
    columns: &[ColumnInfo],
    formats: &[FormatCode],
    row: &[Datum],
) -> Result<EncodedRow> {
    if row.len() != columns.len() || formats.len() != columns.len() {
        return Err(PgError::internal(format!(
            "row has {} values and {} formats but the portal describes {} columns",
            row.len(),
            formats.len(),
            columns.len()
        )));
    }
    let mut out: EncodedRow = Vec::with_capacity(row.len());
    for ((col, value), format) in columns.iter().zip(row.iter()).zip(formats.iter()) {
        let ty = TypeId { oid: col.type_oid, typmod: col.type_mod };
        out.push(encode_datum(ctx, ty, value, *format)?);
    }
    Ok(out)
}

/// One Bind parameter: `None` bytes = SQL NULL. Text input goes through `io::input`, binary through
/// `io::recv` (both apply the parameter's typmod).
fn decode_with(ctx: &IoCtx, ty: TypeId, bytes: Option<&[u8]>, format: FormatCode) -> Result<Datum> {
    let bytes = match bytes {
        None => return Ok(Datum::Null),
        Some(b) => b,
    };
    match format {
        FormatCode::Binary => io::recv(ctx, ty, bytes),
        FormatCode::Text => {
            let text = match std::str::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => return Err(invalid_utf8(bytes, e.valid_up_to(), e.error_len())),
            };
            if bytes.contains(&0) {
                return Err(PgError::new(
                    SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\": 0x00",
                ));
            }
            io::input(ctx, ty, text)
        }
    }
}

impl Session {
    /// Type metadata for I/O: the live catalog env when a statement has run, otherwise the builtin
    /// registry (last known env is `self.env`).
    fn type_env(&self) -> &dyn TypeEnv {
        match self.env.as_ref() {
            Some(env) => env as &dyn TypeEnv,
            None => registry::builtin_env() as &dyn TypeEnv,
        }
    }

    /// I/O context for this session: GUC-derived config, and the transaction timestamp when a
    /// transaction exists (so `'now'` input works only inside one).
    fn io_ctx<'a>(&'a self, cfg: &'a IoConfig) -> IoCtx<'a> {
        let now = if self.txn.is_some() { Some(self.txn_timestamp()) } else { None };
        IoCtx { env: self.type_env(), config: cfg, now }
    }

    /// Create portal `portal_name` from prepared statement `stmt_name` (module docs). Errors: unknown
    /// statement 26000; existing named portal 42P03 `cursor "x" already exists`; the unnamed portal is
    /// replaced.
    pub(crate) fn open_portal(
        &mut self,
        portal_name: &str,
        stmt_name: &str,
        params: ParamValues,
        result_formats: Vec<FormatCode>,
    ) -> Result<()> {
        // PG creates the portal (duplicate check) before looking the statement up.
        if !portal_name.is_empty() && self.portals.contains(portal_name) {
            return Err(PgError::new(SqlState(*b"42P03"), format!("cursor \"{portal_name}\" already exists")));
        }
        if self.prepared.get(stmt_name).is_none() {
            let msg = if stmt_name.is_empty() {
                "unnamed prepared statement does not exist".to_string()
            } else {
                format!("prepared statement \"{stmt_name}\" does not exist")
            };
            return Err(PgError::new(SqlState::INVALID_SQL_STATEMENT_NAME, msg));
        }
        // Revalidate against catalog changes (Bind plans against the current catalog).
        let ps = self.ensure_current(stmt_name)?;
        if params.values.len() != ps.param_types.len() {
            return Err(PgError::new(
                SqlState::PROTOCOL_VIOLATION,
                format!(
                    "bind message supplies {} parameters, but prepared statement \"{}\" requires {}",
                    params.values.len(),
                    stmt_name,
                    ps.param_types.len()
                ),
            ));
        }
        let ncols = ps.columns.as_ref().map_or(0, |c| c.len());
        column_formats(ncols, &result_formats)?;
        let plan = self.plan_for(&ps, &params.values)?;
        let portal = Portal {
            name: portal_name.to_string(),
            stmt_name: stmt_name.to_string(),
            text: ps.text.clone(),
            statement: ps.bound.clone(),
            plan,
            params,
            result_formats,
            columns: ps.columns.clone(),
            state: PortalState::Ready,
        };
        self.portals.insert(portal);
        Ok(())
    }

    /// Run/continue a portal for up to `max_rows` rows (0 = unlimited) and encode the rows with the
    /// portal's result formats.
    pub(crate) fn run_portal(&mut self, portal_name: &str, max_rows: u32) -> Result<PortalOutput> {
        let mut portal = match self.portals.remove(portal_name) {
            Some(p) => p,
            None => {
                return Err(PgError::new(
                    SqlState::INVALID_CURSOR_NAME,
                    format!("portal \"{portal_name}\" does not exist"),
                ))
            }
        };
        let result = self.advance_with_state(&mut portal, max_rows);
        // Portals die with the transaction: keep it only while one is open.
        if self.txn.is_some() {
            self.portals.insert(portal);
        }
        result
    }

    /// Wire encoding of one value (module docs).
    pub fn encode_value(&self, ty: TypeId, value: &Datum, format: FormatCode) -> Result<Option<Vec<u8>>> {
        if value.is_null() {
            return Ok(None);
        }
        let cfg = guc_derive::io_config(&self.guc)?;
        let ctx = self.io_ctx(&cfg);
        encode_datum(&ctx, ty, value, format)
    }

    /// Encode a whole row (module docs).
    pub fn encode_row(&self, columns: &[ColumnInfo], formats: &[FormatCode], row: &[Datum]) -> Result<EncodedRow> {
        let per_column = column_formats(columns.len(), formats)?;
        let cfg = guc_derive::io_config(&self.guc)?;
        let ctx = self.io_ctx(&cfg);
        encode_row_with(&ctx, columns, &per_column, row)
    }

    /// Decode one Bind parameter (module docs). `None` bytes = SQL NULL.
    pub fn decode_param(&self, ty: TypeId, bytes: Option<&[u8]>, format: FormatCode) -> Result<Datum> {
        if bytes.is_none() {
            return Ok(Datum::Null);
        }
        let cfg = guc_derive::io_config(&self.guc)?;
        let ctx = self.io_ctx(&cfg);
        decode_with(&ctx, ty, bytes, format)
    }

    // -----------------------------------------------------------------------------------------
    // Execution helpers
    // -----------------------------------------------------------------------------------------

    /// Run one Execute on a portal whose state is taken out of it. On error the portal is left `Done`
    /// with an empty tag (the caller aborts the transaction, which clears portals).
    fn advance_with_state(&mut self, portal: &mut Portal, max_rows: u32) -> Result<PortalOutput> {
        let state = std::mem::replace(&mut portal.state, PortalState::Ready);
        match self.advance_portal(portal, state, max_rows) {
            Ok((next, output)) => {
                portal.state = next;
                Ok(output)
            }
            Err(e) => {
                portal.state = PortalState::Done { tag: String::new() };
                Err(e)
            }
        }
    }

    /// The state machine of module docs. Returns the next state and this Execute's output.
    fn advance_portal(
        &mut self,
        portal: &Portal,
        state: PortalState,
        max_rows: u32,
    ) -> Result<(PortalState, PortalOutput)> {
        let limit: Option<usize> = if max_rows == 0 { None } else { Some(max_rows as usize) };
        let notices: Vec<PgError> = std::mem::take(&mut self.pending_notices);
        match state {
            PortalState::Done { tag } => Ok((
                PortalState::Done { tag: tag.clone() },
                portal_output(Vec::new(), status_for(&tag), notices, Vec::new()),
            )),
            PortalState::Buffered { rows, tag } => self.serve_buffered(portal, rows, tag, limit, notices, Vec::new()),
            PortalState::Streaming(stream) => self.fetch_stream(portal, stream, limit, notices, Vec::new()),
            PortalState::Ready => match (portal.plan.as_ref(), portal.statement.as_ref()) {
                (Some(plan), Some(BoundStatement::Query(_))) => {
                    let plan: Rc<Plan> = Rc::clone(plan);
                    let txn = self
                        .txn
                        .as_mut()
                        .ok_or_else(|| PgError::internal("portal executed outside a transaction"))?;
                    txn.begin_statement()?;
                    let env = self.refresh_env()?;
                    let cmd = plan_command(plan.kind);
                    let plan_ref: &Plan = &plan;
                    let (stream, side) = self.with_exec(
                        cmd,
                        &env,
                        portal.params.values.clone(),
                        portal.params.types.clone(),
                        |cx| open_plan(plan_ref, cx),
                    )?;
                    let mut notices = notices;
                    notices.extend(side.notices);
                    self.fetch_stream(portal, stream, limit, notices, Vec::new())
                }
                _ => self.run_once(portal, limit, notices),
            },
        }
    }

    /// Pull up to `limit` rows from a running plan (PG `PortalRunSelect`: the portal is at its end only
    /// when fewer rows than requested came back, which `PlanStream::is_done` reports).
    fn fetch_stream(
        &mut self,
        portal: &Portal,
        mut stream: PlanStream,
        limit: Option<usize>,
        mut notices: Vec<PgError>,
        param_status: Vec<(String, String)>,
    ) -> Result<(PortalState, PortalOutput)> {
        let cmd = plan_command(stream.kind);
        let env = self.refresh_env()?;
        let (rows, side) = self.with_exec(
            cmd,
            &env,
            portal.params.values.clone(),
            portal.params.types.clone(),
            |cx| stream.fetch(cx, limit),
        )?;
        notices.extend(side.notices);
        let encoded = self.encode_portal_rows(portal, &rows)?;
        if stream.is_done() {
            let tag = stream.command_tag();
            let status = status_for(&tag);
            Ok((PortalState::Done { tag }, portal_output(encoded, status, notices, param_status)))
        } else {
            Ok((
                PortalState::Streaming(stream),
                portal_output(encoded, PortalStatus::Suspended, notices, param_status),
            ))
        }
    }

    /// Serve up to `limit` buffered rows (PG completion rule: finished only if fewer than `limit` rows
    /// were returned).
    fn serve_buffered(
        &self,
        portal: &Portal,
        mut rows: VecDeque<Row>,
        tag: String,
        limit: Option<usize>,
        notices: Vec<PgError>,
        param_status: Vec<(String, String)>,
    ) -> Result<(PortalState, PortalOutput)> {
        let take = match limit {
            Some(n) => n.min(rows.len()),
            None => rows.len(),
        };
        let chunk: Vec<Row> = rows.drain(..take).collect();
        let encoded = self.encode_portal_rows(portal, &chunk)?;
        let finished = match limit {
            Some(n) => chunk.len() < n,
            None => true,
        };
        if finished {
            let status = status_for(&tag);
            Ok((PortalState::Done { tag }, portal_output(encoded, status, notices, param_status)))
        } else {
            Ok((
                PortalState::Buffered { rows, tag },
                portal_output(encoded, PortalStatus::Suspended, notices, param_status),
            ))
        }
    }

    /// Run a non-streamed statement once through the statement pipeline and buffer its rows. The
    /// statement is re-parsed from the portal text (a portal keeps its text, not the parse tree).
    fn run_once(
        &mut self,
        portal: &Portal,
        limit: Option<usize>,
        mut notices: Vec<PgError>,
    ) -> Result<(PortalState, PortalOutput)> {
        let mut parsed = parse(&portal.text)?;
        if parsed.is_empty() {
            return Ok((
                PortalState::Done { tag: String::new() },
                portal_output(Vec::new(), PortalStatus::Empty, notices, Vec::new()),
            ));
        }
        if parsed.len() != 1 {
            return Err(PgError::internal(format!(
                "portal \"{}\" holds {} statements, expected one",
                portal.name,
                parsed.len()
            )));
        }
        let raw = parsed.swap_remove(0);
        let out: StatementOutput = self.run_statement(&raw, &portal.params)?;
        notices.extend(out.notices);
        let param_status = out.param_status;
        match out.result {
            StatementResult::Empty => Ok((
                PortalState::Done { tag: String::new() },
                portal_output(Vec::new(), PortalStatus::Empty, notices, param_status),
            )),
            StatementResult::Command { tag } => {
                let status = status_for(&tag);
                Ok((PortalState::Done { tag }, portal_output(Vec::new(), status, notices, param_status)))
            }
            StatementResult::Rows { rows, tag, .. } => {
                let queue: VecDeque<Row> = rows.into();
                self.serve_buffered(portal, queue, tag, limit, notices, param_status)
            }
        }
    }

    /// Encode rows with the portal's columns and result formats. Only the rows of this Execute are
    /// encoded, one I/O context for all of them.
    fn encode_portal_rows(&self, portal: &Portal, rows: &[Row]) -> Result<Vec<EncodedRow>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let columns: &[ColumnInfo] = match portal.columns.as_ref() {
            Some(c) => c.as_slice(),
            None => &[],
        };
        let formats = column_formats(columns.len(), &portal.result_formats)?;
        let cfg = guc_derive::io_config(&self.guc)?;
        let ctx = self.io_ctx(&cfg);
        rows.iter().map(|row| encode_row_with(&ctx, columns, &formats, row)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_ctx(cfg: &IoConfig) -> IoCtx<'_> {
        IoCtx::new(registry::builtin_env(), cfg)
    }

    #[test]
    fn result_format_rules() {
        assert_eq!(column_formats(3, &[]).unwrap(), vec![FormatCode::Text; 3]);
        assert_eq!(column_formats(2, &[FormatCode::Binary]).unwrap(), vec![FormatCode::Binary; 2]);
        let per = [FormatCode::Text, FormatCode::Binary, FormatCode::Text];
        assert_eq!(column_formats(3, &per).unwrap(), per.to_vec());
        let err = column_formats(3, &[FormatCode::Text, FormatCode::Binary]).unwrap_err();
        assert_eq!(err.sqlstate.code(), "08P01");
        assert_eq!(err.message, "bind message has 2 result formats but query has 3 columns");
    }

    #[test]
    fn status_and_command_names() {
        assert_eq!(status_for(""), PortalStatus::Empty);
        assert_eq!(status_for("SELECT 3"), PortalStatus::Complete { tag: "SELECT 3".to_string() });
        assert_eq!(plan_command(PlanKind::Insert), "INSERT");
        assert_eq!(plan_command(PlanKind::Select), "SELECT");
    }

    #[test]
    fn int4_text_and_binary_encoding() {
        let cfg = IoConfig::default();
        let ctx = builtin_ctx(&cfg);
        let text = encode_datum(&ctx, TypeId::INT4, &Datum::Int4(42), FormatCode::Text).unwrap();
        assert_eq!(text, Some(b"42".to_vec()));
        let bin = encode_datum(&ctx, TypeId::INT4, &Datum::Int4(42), FormatCode::Binary).unwrap();
        assert_eq!(bin, Some(vec![0, 0, 0, 42]));
        assert_eq!(encode_datum(&ctx, TypeId::INT4, &Datum::Null, FormatCode::Text).unwrap(), None);
    }

    #[test]
    fn int4_parameter_decoding() {
        let cfg = IoConfig::default();
        let ctx = builtin_ctx(&cfg);
        let bin = decode_with(&ctx, TypeId::INT4, Some(&[0, 0, 0, 42]), FormatCode::Binary).unwrap();
        assert_eq!(bin, Datum::Int4(42));
        let text = decode_with(&ctx, TypeId::INT4, Some(b"42"), FormatCode::Text).unwrap();
        assert_eq!(text, Datum::Int4(42));
        let null = decode_with(&ctx, TypeId::INT4, None, FormatCode::Text).unwrap();
        assert_eq!(null, Datum::Null);
    }

    #[test]
    fn text_parameter_rejects_nul_and_bad_utf8() {
        let cfg = IoConfig::default();
        let ctx = builtin_ctx(&cfg);
        let nul = decode_with(&ctx, TypeId::TEXT, Some(b"a\0b"), FormatCode::Text).unwrap_err();
        assert_eq!(nul.sqlstate.code(), "22021");
        let bad = decode_with(&ctx, TypeId::TEXT, Some(&[b'a', 0xff]), FormatCode::Text).unwrap_err();
        assert_eq!(bad.sqlstate.code(), "22021");
        assert_eq!(bad.message, "invalid byte sequence for encoding \"UTF8\": 0xff");
    }

    #[test]
    fn row_encoding_mixes_formats_per_column() {
        let cfg = IoConfig::default();
        let ctx = builtin_ctx(&cfg);
        let columns = vec![
            ColumnInfo {
                name: "a".to_string(),
                type_oid: TypeId::INT4.oid,
                type_mod: -1,
                type_len: 4,
                table_oid: 0,
                column_attnum: 0,
            },
            ColumnInfo {
                name: "b".to_string(),
                type_oid: TypeId::INT4.oid,
                type_mod: -1,
                type_len: 4,
                table_oid: 0,
                column_attnum: 0,
            },
        ];
        let formats = [FormatCode::Binary, FormatCode::Text];
        let row = [Datum::Int4(42), Datum::Null];
        let out = encode_row_with(&ctx, &columns, &formats, &row).unwrap();
        assert_eq!(out, vec![Some(vec![0, 0, 0, 42]), None]);
    }

    // The tests below need the session pipeline (API-04 connect, API-08 run_statement, API-09 exec
    // contexts, API-11 prepared statements) and run centrally once those leases land.

    #[test]
    #[ignore = "needs API-04/08/09/11"]
    fn three_row_suspension_follows_pg_completion_rule() {
        // Execute(max_rows=1) over `SELECT x FROM (VALUES (1),(2),(3)) t(x)`: Suspended, Suspended,
        // Suspended, then Complete{"SELECT 3"} with zero rows (PG pquery.c: atEnd only when fewer rows
        // than requested come back).
    }

    #[test]
    #[ignore = "needs API-04/08/09/11"]
    fn portals_disappear_at_commit() {}
}
