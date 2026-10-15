//! Sequence value arithmetic and the persisted state format. Lease CORE-12.
//! Reference: PG18 src/backend/commands/sequence.c (`nextval_internal`, `do_setval`), all messages
//! oracle-verified on PG18.6.
//!
//! # Key / value (CONTRACT)
//! Key: `[0,0,0,2]` ++ seq relid (u32 BE) — in the database tree of the sequence's database.
//! Value v1 (18 bytes, little-endian): `version u8 = 1 | last_value i64 | log_cnt i64 | is_called u8`.
//! Decode errors (wrong length/version, is_called > 1) → XX001 `invalid sequence state for
//! relation <oid>`.
//!
//! # nextval arithmetic (`next_value`)
//! Given state `{last_value, is_called}` and params:
//! - `!is_called` → candidate = `last_value` (first call returns the start/restart value);
//! - else candidate = `last_value + increment` computed with `checked_add`;
//! - ascending (`increment > 0`): overflow or candidate > max_value → if `cycle` → `min_value`, else
//!   2200H `nextval: reached maximum value of sequence "<name>" (<max_value>)`;
//! - descending: underflow or candidate < min_value → `cycle` → `max_value`, else
//!   2200H `nextval: reached minimum value of sequence "<name>" (<min_value>)`.
//! Oracle: `maxvalue 2`: 1, 2, then `nextval: reached maximum value of sequence "s2" (2)`;
//! `increment -1 minvalue -2 maxvalue -1`: -1, -2, then `... reached minimum value of sequence "zz_s4" (-2)`.
//!
//! # setval (`validate_setval`)
//! value outside [min_value, max_value] → 22003 `setval: value <v> is out of bounds for sequence
//! "<name>" (<min>..<max>)`. New state `{last_value: v, log_cnt: 0, is_called}` (oracle: after
//! `setval(s, 100)` → `100 | 0 | t`).
//!
//! # log_cnt emulation (sequence.c, SEQ_LOG_VALS = 32)
//! `advance(state, params, cache)` returns the value(s) to hand out and the new state, plus
//! `Some(persist_state)` when PG would WAL-log:
//! - `fetch = cache` (CACHE n, >= 1); `log = state.log_cnt`; if `log < fetch || !state.is_called`
//!   (or `force_log`, the first nextval after the engine opened the sequence — PG logs on the first
//!   nextval after a checkpoint) → `fetch = log = fetch + SEQ_LOG_VALS` and logging happens.
//! - Produce `cache` values (the first is returned; extras go to the session's CACHE range),
//!   decrementing `log` per produced value beyond the first fetch loop exactly as sequence.c does,
//!   so that after the FIRST nextval of a new sequence the visible state is `last_value=1,
//!   log_cnt=32, is_called=t`, after the second `2 | 31 | t` (oracle-verified).
//! - Persisted image when logging: `{last_value: value after advancing over all `fetch` values,
//!   log_cnt: 0, is_called: true}` — what a crash recovers to (values skip, never repeat).
use crate::error;
use pgarbor_common::{Oid, PgError, RelId, Result, SqlState};
use std::collections::HashMap;

/// PG SEQ_LOG_VALS.
pub const SEQ_LOG_VALS: i64 = 32;

/// Key of a sequence's state.
pub fn seq_key(relid: RelId) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&pgarbor_common::SpaceId::SEQUENCES.prefix());
    k[4..8].copy_from_slice(&relid.to_be_bytes());
    k
}

// CONTRACT
/// Catalog parameters (pg_sequence row + relation name), supplied by the caller (exec reads the
/// catalog; core never does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceParams {
    /// Unqualified relation name, as used in PG messages.
    pub name: String,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub start: i64,
    /// CACHE (>= 1).
    pub cache: i64,
    pub cycle: bool,
}

// CONTRACT
/// What `SELECT last_value, log_cnt, is_called FROM seq` shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceState {
    pub last_value: i64,
    pub log_cnt: i64,
    pub is_called: bool,
}

impl SequenceState {
    /// State of a freshly created (or RESTARTed) sequence: `{start, 0, false}` (oracle: new
    /// sequence shows `1 | 0 | f`).
    pub fn initial(start: i64) -> SequenceState {
        SequenceState { last_value: start, log_cnt: 0, is_called: false }
    }

    /// Encode per the module docs (18 bytes).
    #[allow(unused_variables)]
    pub fn encode(&self) -> Result<Vec<u8>> {
        Err(PgError::unimplemented("CORE-12", "sequence state encoding"))
    }

    /// Decode per the module docs.
    #[allow(unused_variables)]
    pub fn decode(relid: RelId, bytes: &[u8]) -> Result<SequenceState> {
        Err(PgError::unimplemented("CORE-12", "sequence state decoding"))
    }
}

// CONTRACT
/// Result of one `advance`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advance {
    /// Value returned by this nextval.
    pub value: i64,
    /// Additional values reserved for this session's CACHE (ascending in call order), possibly empty.
    pub cached: Vec<i64>,
    /// Visible state after the call.
    pub state: SequenceState,
    /// State to persist durably before returning (Some when PG would WAL-log).
    pub persist: Option<SequenceState>,
}

/// One nextval step (no CACHE, no logging): see "nextval arithmetic".
#[allow(unused_variables)]
pub fn next_value(state: &SequenceState, params: &SequenceParams) -> Result<i64> {
    Err(PgError::unimplemented("CORE-12", "sequence next value"))
}

/// Full nextval with CACHE and log_cnt emulation (module docs).
#[allow(unused_variables)]
pub fn advance(state: &SequenceState, params: &SequenceParams, force_log: bool) -> Result<Advance> {
    Err(PgError::unimplemented("CORE-12", "sequence advance with prefetch logging"))
}

/// Range check for setval (module docs); returns the new state.
#[allow(unused_variables)]
pub fn validate_setval(params: &SequenceParams, value: i64, is_called: bool) -> Result<SequenceState> {
    Err(PgError::unimplemented("CORE-12", "setval validation"))
}

/// 2200H helper used by `next_value`.
pub fn limit_reached(params: &SequenceParams, maximum: bool) -> PgError {
    let (word, v) = if maximum { ("maximum", params.max_value) } else { ("minimum", params.min_value) };
    PgError::new(
        error::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
        format!("nextval: reached {word} value of sequence \"{}\" ({v})", params.name),
    )
}

/// 22003 helper used by `validate_setval`.
pub fn setval_out_of_bounds(params: &SequenceParams, value: i64) -> PgError {
    PgError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!(
            "setval: value {value} is out of bounds for sequence \"{}\" ({}..{})",
            params.name, params.min_value, params.max_value
        ),
    )
}

/// 55000 `currval of sequence "<name>" is not yet defined in this session` (oracle).
pub fn currval_undefined(name: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("currval of sequence \"{name}\" is not yet defined in this session"),
    )
}

/// 55000 `lastval is not yet defined in this session`.
pub fn lastval_undefined() -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "lastval is not yet defined in this session")
}

// CONTRACT
/// Per-session sequence memory (PG backend-local `SeqTable`): last value returned per sequence
/// (currval), the most recently used sequence (lastval), and unused CACHE values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSequences {
    /// (database oid, seq relid) → last value returned to this session.
    pub last: HashMap<(Oid, RelId), i64>,
    /// (database oid, seq relid) of the last nextval/setval call.
    pub lastval: Option<(Oid, RelId)>,
    /// (database oid, seq relid) → reserved CACHE values not yet handed out (front = next).
    pub cached: HashMap<(Oid, RelId), std::collections::VecDeque<i64>>,
}
