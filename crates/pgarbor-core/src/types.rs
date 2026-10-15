//! Small shared value types of the transactional core. CONTRACT. Data only, fully implemented.
use pgarbor_common::{CommitSeq, PgError, Result, SqlState};
use std::fmt;
use std::time::Instant;

// CONTRACT
/// Backend process id as PostgreSQL reports it (`pg_backend_pid()`, `pg_stat_activity.pid`,
/// BackendKeyData). Allocated by `SessionRegistry`, unique for the engine's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct SessionId(pub i32);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// CONTRACT
/// Subtransaction id inside one transaction: 0 = top level; every SAVEPOINT (and every
/// ROLLBACK TO, which restarts the subtransaction) takes the next id from a per-txn counter, so ids
/// are strictly increasing in time. Locks remember the id they were acquired under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct SubXactId(pub u32);

impl SubXactId {
    pub const TOP: SubXactId = SubXactId(0);
}

// CONTRACT
/// Transaction state machine (ARCHITECTURE §7). `Idle` = no transaction (only used for status
/// reporting; a live `Txn` is InProgress or Failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum TxnState {
    Idle,
    InProgress,
    /// An error occurred in an explicit block; only ROLLBACK, ROLLBACK TO SAVEPOINT and COMMIT
    /// (which then rolls back) are accepted (25P02 for everything else).
    Failed,
}

impl TxnState {
    /// ReadyForQuery transaction status byte: `I`, `T`, `E`.
    pub fn wire_status(self) -> u8 {
        match self {
            TxnState::Idle => b'I',
            TxnState::InProgress => b'T',
            TxnState::Failed => b'E',
        }
    }
}

// CONTRACT
/// Whether the transaction is an explicit block (BEGIN ... COMMIT) or the implicit single-statement
/// transaction PostgreSQL wraps around each top-level statement outside a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum TxnKind {
    Implicit,
    Explicit,
}

// CONTRACT
/// Isolation level (PG `transaction_isolation` / `default_transaction_isolation` values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum IsolationLevel {
    /// Behaves exactly like ReadCommitted (as in PostgreSQL) but reports its own name.
    ReadUncommitted,
    #[default]
    ReadCommitted,
    RepeatableRead,
    /// v1: REPEATABLE READ snapshot + write-conflict rules (ARCHITECTURE §7 KNOWN INCOMPATIBILITY,
    /// inventory `serializable_ssi = UNIMPLEMENTED` until lease CORE-16 lands).
    Serializable,
}

impl IsolationLevel {
    /// PG's spelling: `read uncommitted`, `read committed`, `repeatable read`, `serializable`.
    pub fn as_pg_str(self) -> &'static str {
        match self {
            IsolationLevel::ReadUncommitted => "read uncommitted",
            IsolationLevel::ReadCommitted => "read committed",
            IsolationLevel::RepeatableRead => "repeatable read",
            IsolationLevel::Serializable => "serializable",
        }
    }

    /// Parse a GUC value (case-insensitive, surrounding whitespace ignored). `default` is NOT
    /// accepted here (GUC layer resolves it). Unknown → 22023
    /// `invalid value for parameter "transaction_isolation": "<v>"` with hint
    /// `Available values: serializable, repeatable read, read committed, read uncommitted.`
    pub fn parse(value: &str) -> Result<IsolationLevel> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read uncommitted" => Ok(IsolationLevel::ReadUncommitted),
            "read committed" => Ok(IsolationLevel::ReadCommitted),
            "repeatable read" => Ok(IsolationLevel::RepeatableRead),
            "serializable" => Ok(IsolationLevel::Serializable),
            _ => Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid value for parameter \"transaction_isolation\": \"{value}\""),
            )
            .with_hint("Available values: serializable, repeatable read, read committed, read uncommitted.")),
        }
    }

    /// Statement-level snapshots (RU, RC) vs transaction-level (RR, SERIALIZABLE).
    pub fn uses_statement_snapshot(self) -> bool {
        matches!(self, IsolationLevel::ReadUncommitted | IsolationLevel::ReadCommitted)
    }
}

// CONTRACT
/// Options of `BEGIN` / `START TRANSACTION` (or the session defaults for implicit transactions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnOptions {
    pub kind: TxnKind,
    pub isolation: IsolationLevel,
    pub read_only: bool,
    pub deferrable: bool,
}

impl Default for TxnOptions {
    fn default() -> Self {
        TxnOptions {
            kind: TxnKind::Implicit,
            isolation: IsolationLevel::ReadCommitted,
            read_only: false,
            deferrable: false,
        }
    }
}

// CONTRACT
/// What to do when a lock is not immediately available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaitPolicy {
    /// Wait (subject to `WaitLimits`).
    #[default]
    Block,
    /// `NOWAIT`: return `AcquireResult::NotAvailable` immediately (caller raises 55P03 with its
    /// own message, e.g. `could not obtain lock on row in relation "t"`).
    NoWait,
    /// `SKIP LOCKED`: return `AcquireResult::NotAvailable` immediately (caller skips the row).
    SkipLocked,
}

// CONTRACT
/// Per-statement wait limits, set by the session from GUCs before each statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitLimits {
    /// `lock_timeout` in ms; 0 = wait forever.
    pub lock_timeout_ms: u64,
    /// `deadlock_timeout` in ms (PG default 1000): how long a waiter sleeps before running the
    /// deadlock check.
    pub deadlock_timeout_ms: u64,
    /// `statement_timeout` deadline of the current statement, if any.
    pub statement_deadline: Option<Instant>,
}

impl Default for WaitLimits {
    fn default() -> Self {
        WaitLimits { lock_timeout_ms: 0, deadlock_timeout_ms: 1000, statement_deadline: None }
    }
}

// CONTRACT
/// Result of a lock request that did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireResult {
    /// Granted. `newly == false` when the same owner already held this mode (re-entrant count).
    Acquired { newly: bool },
    /// `NoWait`/`SkipLocked` and the lock is held by someone else.
    NotAvailable,
}

// CONTRACT
/// Result of locking a key for write (ARCHITECTURE §7): compares the latest committed value with
/// the value in the transaction's snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockOutcome {
    /// Latest committed value equals the snapshot value: proceed.
    Unchanged,
    /// A concurrent transaction committed a change after our snapshot. Payload = latest committed
    /// value (`None` = deleted). RC callers re-evaluate against it (EvalPlanQual); RR/SERIALIZABLE
    /// callers raise 40001 (see `Txn::lock_key_for_write`).
    Changed(Option<Vec<u8>>),
    /// `NoWait`/`SkipLocked` and the key is locked by another transaction.
    NotAvailable,
}

// CONTRACT
/// Result of COMMIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// Durable. `seq` is None for transactions that wrote nothing (no storage commit happened).
    Committed { seq: Option<CommitSeq> },
    /// COMMIT of a Failed transaction rolls back (PG reports command tag `ROLLBACK`).
    RolledBack,
}
