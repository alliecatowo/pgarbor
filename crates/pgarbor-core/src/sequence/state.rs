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
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut v = Vec::with_capacity(18);
        v.push(1u8);
        v.extend_from_slice(&self.last_value.to_le_bytes());
        v.extend_from_slice(&self.log_cnt.to_le_bytes());
        v.push(self.is_called as u8);
        Ok(v)
    }

    /// Decode per the module docs.
    pub fn decode(relid: RelId, bytes: &[u8]) -> Result<SequenceState> {
        if bytes.len() != 18 || bytes[0] != 1 || bytes[17] > 1 {
            return Err(PgError::corruption(format!("invalid sequence state for relation {relid}")));
        }
        let mut a = [0u8; 8];
        a.copy_from_slice(&bytes[1..9]);
        let last_value = i64::from_le_bytes(a);
        a.copy_from_slice(&bytes[9..17]);
        let log_cnt = i64::from_le_bytes(a);
        Ok(SequenceState { last_value, log_cnt, is_called: bytes[17] == 1 })
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
pub fn next_value(state: &SequenceState, params: &SequenceParams) -> Result<i64> {
    if !state.is_called {
        return Ok(state.last_value);
    }
    match step(state.last_value, params) {
        Some(n) => Ok(n),
        None => wrap(params),
    }
}

/// Next value after `next` in the sequence direction; None when the limit (or i64 range) is hit.
fn step(next: i64, params: &SequenceParams) -> Option<i64> {
    let n = next.checked_add(params.increment)?;
    if params.increment > 0 {
        if n <= params.max_value { Some(n) } else { None }
    } else if n >= params.min_value {
        Some(n)
    } else {
        None
    }
}

/// Value to restart from after hitting a limit, or the 2200H error when not cycling.
fn wrap(params: &SequenceParams) -> Result<i64> {
    if !params.cycle {
        return Err(limit_reached(params, params.increment > 0));
    }
    Ok(if params.increment > 0 { params.min_value } else { params.max_value })
}

/// Full nextval with CACHE and log_cnt emulation (module docs).
pub fn advance(state: &SequenceState, params: &SequenceParams, force_log: bool) -> Result<Advance> {
    let cache = params.cache.max(1);
    let mut next = state.last_value;
    let mut last = next;
    let mut result = next;
    let mut rescnt: i64 = 0;
    let mut fetch = cache;
    let mut log = state.log_cnt;
    let mut cached: Vec<i64> = Vec::new();

    if !state.is_called {
        rescnt += 1;
        fetch -= 1;
    }
    let mut logit = false;
    if log < fetch || !state.is_called || force_log {
        fetch = fetch.saturating_add(SEQ_LOG_VALS);
        log = fetch;
        logit = true;
    }

    while fetch > 0 {
        match step(next, params) {
            Some(n) => next = n,
            None => {
                if rescnt > 0 {
                    break;
                }
                next = wrap(params)?;
            }
        }
        fetch -= 1;
        if rescnt < cache {
            log -= 1;
            rescnt += 1;
            last = next;
            if rescnt == 1 {
                result = next;
            } else {
                cached.push(next);
            }
        }
    }
    log -= fetch;
    if log < 0 {
        log = 0;
    }

    let new_state = SequenceState { last_value: last, log_cnt: log, is_called: true };
    let persist = if logit {
        Some(SequenceState { last_value: next, log_cnt: 0, is_called: true })
    } else {
        None
    };
    Ok(Advance { value: result, cached, state: new_state, persist })
}

/// Range check for setval (module docs); returns the new state.
pub fn validate_setval(params: &SequenceParams, value: i64, is_called: bool) -> Result<SequenceState> {
    if value < params.min_value || value > params.max_value {
        return Err(setval_out_of_bounds(params, value));
    }
    Ok(SequenceState { last_value: value, log_cnt: 0, is_called })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, inc: i64, min: i64, max: i64, cache: i64, cycle: bool) -> SequenceParams {
        SequenceParams { name: name.into(), increment: inc, min_value: min, max_value: max, start: 1, cache, cycle }
    }
    fn dflt() -> SequenceParams {
        p("s", 1, 1, i64::MAX, 1, false)
    }
    fn st(l: i64, c: i64, t: bool) -> SequenceState {
        SequenceState { last_value: l, log_cnt: c, is_called: t }
    }

    #[test]
    fn oracle_cache1() {
        let pr = dflt();
        let a = advance(&SequenceState::initial(1), &pr, false).unwrap();
        assert_eq!(a.value, 1);
        assert_eq!(a.state, st(1, 32, true));
        assert_eq!(a.persist, Some(st(33, 0, true)));
        let b = advance(&a.state, &pr, false).unwrap();
        assert_eq!(b.value, 2);
        assert_eq!(b.state, st(2, 31, true));
        assert_eq!(b.persist, None);
        let mut s = b.state;
        let mut logged = 0;
        for _ in 0..32 {
            let x = advance(&s, &pr, false).unwrap();
            if x.persist.is_some() {
                logged += 1;
            }
            s = x.state;
        }
        assert_eq!(logged, 1);
    }

    #[test]
    fn max_reached() {
        let pr = p("s2", 1, 1, 2, 1, false);
        let mut s = SequenceState::initial(1);
        for want in [1, 2] {
            let a = advance(&s, &pr, false).unwrap();
            assert_eq!(a.value, want);
            s = a.state;
        }
        let e = advance(&s, &pr, false).unwrap_err();
        assert_eq!(e.message, "nextval: reached maximum value of sequence \"s2\" (2)");
        assert_eq!(e.sqlstate, error::SEQUENCE_GENERATOR_LIMIT_EXCEEDED);
    }

    #[test]
    fn descending_and_cycle() {
        let pr = p("zz_s4", -1, -2, -1, 1, false);
        assert_eq!(next_value(&st(-1, 0, false), &pr).unwrap(), -1);
        assert_eq!(next_value(&st(-1, 0, true), &pr).unwrap(), -2);
        let e = next_value(&st(-2, 0, true), &pr).unwrap_err();
        assert_eq!(e.message, "nextval: reached minimum value of sequence \"zz_s4\" (-2)");
        let c = p("c", -1, -2, -1, 1, true);
        assert_eq!(next_value(&st(-2, 0, true), &c).unwrap(), -1);
        let c2 = p("c", 1, 1, 3, 1, true);
        assert_eq!(next_value(&st(3, 0, true), &c2).unwrap(), 1);
        let big = p("b", i64::MAX, 1, i64::MAX, 1, false);
        assert!(next_value(&st(2, 0, true), &big).is_err());
    }

    #[test]
    fn cache_values() {
        let pr = p("s", 1, 1, i64::MAX, 5, false);
        let a = advance(&SequenceState::initial(1), &pr, false).unwrap();
        assert_eq!(a.value, 1);
        assert_eq!(a.cached, vec![2, 3, 4, 5]);
        assert_eq!(a.state.last_value, 5);
    }

    #[test]
    fn setval_checks() {
        let pr = dflt();
        let e = validate_setval(&pr, 0, true).unwrap_err();
        assert_eq!(e.message, "setval: value 0 is out of bounds for sequence \"s\" (1..9223372036854775807)");
        assert_eq!(validate_setval(&pr, 100, true).unwrap(), st(100, 0, true));
    }

    #[test]
    fn codec() {
        let s = st(-5, 7, true);
        let b = s.encode().unwrap();
        assert_eq!(b.len(), 18);
        assert_eq!(SequenceState::decode(1, &b).unwrap(), s);
        let e = SequenceState::decode(1, &b[..17]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATA_CORRUPTED);
        let mut bad = b.clone();
        bad[17] = 2;
        assert!(SequenceState::decode(1, &bad).is_err());
    }
}
