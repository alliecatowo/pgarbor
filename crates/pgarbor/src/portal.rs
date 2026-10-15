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
use std::collections::HashMap;
use std::rc::Rc;

use pgarbor_common::{PgError, Result};
use pgarbor_exec::PlanStream;
use pgarbor_plan::plan::Plan;
use pgarbor_sql::ir::BoundStatement;
use pgarbor_types::{Datum, TypeId};

use crate::run_stmt::ParamValues;
use crate::session::Session;
use crate::types::{ColumnInfo, EncodedRow, FormatCode, PortalOutput};

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

impl Session {
    /// Create portal `portal_name` from prepared statement `stmt_name` (module docs). Errors: unknown
    /// statement 26000; existing named portal 42P03 `cursor "x" already exists`; the unnamed portal is
    /// replaced.
    #[allow(unused_variables)]
    pub(crate) fn open_portal(
        &mut self,
        portal_name: &str,
        stmt_name: &str,
        params: ParamValues,
        result_formats: Vec<FormatCode>,
    ) -> Result<()> {
        Err(PgError::unimplemented("API-12", "portal creation (Session::open_portal)"))
    }

    /// Run/continue a portal for up to `max_rows` rows (0 = unlimited) and encode the rows with the
    /// portal's result formats.
    #[allow(unused_variables)]
    pub(crate) fn run_portal(&mut self, portal_name: &str, max_rows: u32) -> Result<PortalOutput> {
        Err(PgError::unimplemented("API-12", "portal execution (Session::run_portal)"))
    }

    /// Wire encoding of one value (module docs).
    #[allow(unused_variables)]
    pub fn encode_value(&self, ty: TypeId, value: &Datum, format: FormatCode) -> Result<Option<Vec<u8>>> {
        Err(PgError::unimplemented("API-12", "value encoding (Session::encode_value)"))
    }

    /// Encode a whole row (module docs).
    #[allow(unused_variables)]
    pub fn encode_row(&self, columns: &[ColumnInfo], formats: &[FormatCode], row: &[Datum]) -> Result<EncodedRow> {
        Err(PgError::unimplemented("API-12", "row encoding (Session::encode_row)"))
    }

    /// Decode one Bind parameter (module docs). `None` bytes = SQL NULL.
    #[allow(unused_variables)]
    pub fn decode_param(&self, ty: TypeId, bytes: Option<&[u8]>, format: FormatCode) -> Result<Datum> {
        Err(PgError::unimplemented("API-12", "parameter decoding (Session::decode_param)"))
    }
}
