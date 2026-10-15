//! Error constructors of the transactional core, with PostgreSQL 18's exact wording.
//! CONTRACT. Fully implemented (no lease). Sources cited per function
//! (`~/develop/reference/postgres-18/src/...`); verified against the PG18.6 oracle where noted.
use pgarbor_common::{PgError, Severity, SqlState};

/// 3B001 `savepoint_exception` / invalid specification.
pub const INVALID_SAVEPOINT_SPECIFICATION: SqlState = SqlState(*b"3B001");
/// 2200H `sequence_generator_limit_exceeded`.
pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: SqlState = SqlState(*b"2200H");
/// 01000 warning.
pub const WARNING: SqlState = SqlState::WARNING;

/// 25P02 `current transaction is aborted, commands ignored until end of transaction block`
/// (tcop/postgres.c, oracle-verified).
pub fn in_failed_transaction() -> PgError {
    PgError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

/// 25P01 `<command> can only be used in transaction blocks` — command is `SAVEPOINT`,
/// `RELEASE SAVEPOINT` or `ROLLBACK TO SAVEPOINT` (xact.c CheckTransactionBlock, oracle-verified).
pub fn outside_transaction_block(command: &str) -> PgError {
    PgError::new(SqlState::NO_ACTIVE_SQL_TRANSACTION, format!("{command} can only be used in transaction blocks"))
}

/// 3B001 `savepoint "<name>" does not exist` (xact.c ReleaseSavepoint/RollbackToSavepoint).
pub fn savepoint_does_not_exist(name: &str) -> PgError {
    PgError::new(INVALID_SAVEPOINT_SPECIFICATION, format!("savepoint \"{name}\" does not exist"))
}

/// 40001 `could not serialize access due to concurrent update` (heapam.c / nodeModifyTable.c).
pub fn serialization_concurrent_update() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update")
}

/// 40001 `could not serialize access due to concurrent delete`.
pub fn serialization_concurrent_delete() -> PgError {
    PgError::new(SqlState::SERIALIZATION_FAILURE, "could not serialize access due to concurrent delete")
}

/// 25006 `cannot execute <command> in a read-only transaction` (utility.c PreventCommandIfReadOnly;
/// command e.g. `INSERT`, `CREATE TABLE`, `nextval()`, `setval()` — oracle-verified).
pub fn read_only_transaction(command: &str) -> PgError {
    PgError::new(SqlState::READ_ONLY_SQL_TRANSACTION, format!("cannot execute {command} in a read-only transaction"))
}

/// 25001 `SET TRANSACTION ISOLATION LEVEL must be called before any query` (variable.c
/// check_transaction_isolation, oracle-verified).
pub fn isolation_after_query() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "SET TRANSACTION ISOLATION LEVEL must be called before any query")
}

/// 25001 `SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction`.
pub fn isolation_in_subtransaction() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction")
}

/// 25001 `transaction read-write mode must be set before any query` (oracle-verified).
pub fn read_write_after_query() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "transaction read-write mode must be set before any query")
}

/// 25001 `cannot set transaction read-write mode inside a read-only transaction`.
pub fn read_write_inside_read_only() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "cannot set transaction read-write mode inside a read-only transaction")
}

/// 25001 `SET TRANSACTION [NOT] DEFERRABLE cannot be called within a subtransaction`.
pub fn deferrable_in_subtransaction() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "SET TRANSACTION [NOT] DEFERRABLE cannot be called within a subtransaction")
}

/// 25001 `SET TRANSACTION [NOT] DEFERRABLE must be called before any query`.
pub fn deferrable_after_query() -> PgError {
    PgError::new(SqlState::ACTIVE_SQL_TRANSACTION, "SET TRANSACTION [NOT] DEFERRABLE must be called before any query")
}

/// 55P03 `canceling statement due to lock timeout` (postgres.c ProcessInterrupts, oracle-verified).
pub fn lock_timeout() -> PgError {
    PgError::new(SqlState::LOCK_NOT_AVAILABLE, "canceling statement due to lock timeout")
}

/// 57014 `canceling statement due to statement timeout`.
pub fn statement_timeout() -> PgError {
    PgError::new(SqlState::QUERY_CANCELED, "canceling statement due to statement timeout")
}

/// 57014 `canceling statement due to user request`.
pub fn query_canceled() -> PgError {
    PgError::new(SqlState::QUERY_CANCELED, "canceling statement due to user request")
}

/// FATAL 57P01 `terminating connection due to administrator command`.
pub fn terminated_by_administrator() -> PgError {
    PgError::new(SqlState::ADMIN_SHUTDOWN, "terminating connection due to administrator command")
        .with_severity(Severity::Fatal)
}

/// 40P01 `deadlock detected`, detail = one line per wait edge
/// (`Process <pid> waits for <mode> on <object>; blocked by process <pid>.`, joined with `\n`),
/// hint `See server log for query details.` (deadlock.c DeadLockReport, oracle-verified).
pub fn deadlock_detected(detail: String) -> PgError {
    PgError::new(SqlState::DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(detail)
        .with_hint("See server log for query details.")
}

/// WARNING 01000 `you don't own a lock of type <ExclusiveLock|ShareLock>` (lock.c LockRelease,
/// oracle-verified). Returned as a notice value, never as an `Err`.
pub fn advisory_not_owned(mode_name: &str) -> PgError {
    PgError::new(WARNING, format!("you don't own a lock of type {mode_name}")).with_severity(Severity::Warning)
}

/// 55006 `branch "<name>" is being accessed by other sessions`, detail
/// `There is 1 other session using the branch.` / `There are <n> other sessions using the branch.`
/// (modeled on dbcommands.c `database "%s" is being accessed by other users`).
pub fn branch_in_use(name: &str, others: usize) -> PgError {
    let detail = if others == 1 {
        "There is 1 other session using the branch.".to_string()
    } else {
        format!("There are {others} other sessions using the branch.")
    };
    PgError::new(SqlState::OBJECT_IN_USE, format!("branch \"{name}\" is being accessed by other sessions"))
        .with_detail(detail)
}

/// 55000 `branch "<name>" was reset or deleted while the transaction was in progress`.
pub fn branch_changed_under_transaction(name: &str) -> PgError {
    PgError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("branch \"{name}\" was reset or deleted while the transaction was in progress"),
    )
}

/// 3D000 `database with OID <oid> does not exist in branch "<name>"` (v1 has only database 5).
pub fn database_not_in_branch(oid: u32, branch: &str) -> PgError {
    PgError::new(SqlState::INVALID_CATALOG_NAME, format!("database with OID {oid} does not exist in branch \"{branch}\""))
}

/// XX000 internal invariant violation inside the core (never user-triggerable).
pub fn internal(what: impl Into<String>) -> PgError {
    PgError::internal(what)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wording() {
        assert_eq!(outside_transaction_block("SAVEPOINT").message, "SAVEPOINT can only be used in transaction blocks");
        assert_eq!(savepoint_does_not_exist("x").sqlstate.code(), "3B001");
        assert_eq!(branch_in_use("f", 2).detail.as_deref(), Some("There are 2 other sessions using the branch."));
        assert_eq!(terminated_by_administrator().severity, Severity::Fatal);
    }
}
