# pgarbor-core — crate contract v1

Status: RATIFIED skeleton (contract freeze). Leases: `contracts/leases/pgarbor-core.json`
(CORE-01..CORE-16).
Amendments: `pgarbor-core-01-id-allocation`, `pgarbor-core-02-sessions-databases-branch-scope`.
Depends on: pgarbor-common, pgarbor-storage, pgarbor-branches.

## Purpose

This crate is the transactional engine under SQL. Its parts:

- the `Engine`, which owns the `.pga`;
- sessions, bound to one branch and one database;
- transactions: snapshots, overlay, savepoints, the state machine and commit;
- one PostgreSQL-style lock manager for row/key locks, relation locks and advisory locks, with
  deadlock detection and timeouts;
- non-transactional sequences;
- id allocation;
- data for `pg_stat_activity` and `pg_locks`.

It is synchronous: no async, no background threads.

## Public API

| Item | Use |
|---|---|
| `Engine` (`Arc<Engine>`, Send+Sync) | `create/open/close`, `attach_session`, `begin`, `branch_head`, `create_branch/rename_branch/delete_branch/reset_branch/restore_branch/purge_deleted/compact` (session-aware), `cancel_backend/terminate_backend/cancel_request`, accessors `container/branches/locks/sessions/sequences/ids/ssi` |
| `SessionHandle`, `SessionShared`, `Interrupts`, `Activity`, `BackendState` | one per connection; `interrupts().begin_statement(deadline)`, `check()`; activity updates for pg_stat_activity |
| `Txn` | `begin_statement`, `kv(cmd) -> TxnKv`, `lock_key`, `lock_key_for_write`, `lock_relation`, `set_isolation/set_read_only/set_deferrable`, `savepoint/release_savepoint/rollback_to_savepoint`, `commit -> CommitOutcome`, `rollback`, `mark_failed`, `check_not_failed`, `check_writable`, `check_interrupts`, `set_limits` |
| `TxnKv<'_>` | `KvRead + KvWrite` overlay-over-snapshot view (+ `put_fresh`) handed to pgarbor-exec / pgarbor-catalog |
| `LockManager`, `LockTag`, `LockMode`, `LockScope`, `LockInfo` | lock table |
| `locks::advisory::{lock, unlock, unlock_all}`, `AdvisoryKey/Mode/Scope`, `UnlockResult` | pg_advisory_* |
| `sequence::store::{nextval, setval, currval, lastval, read_state, create, restart, drop}`, `SequenceParams`, `SequenceState` | sequences |
| `alloc::{next_rowid, next_rowids, next_oid, row_key}` | non-transactional ids |
| `diag::{activity_rows, lock_rows, blocking_pids}` | virtual catalog rows |
| `types::*`, `error::*` | shared value types, PG-exact errors |

### How downstream crates use it

The **pgarbor** crate's `Session`:

1. Holds a `SessionHandle` and an `Option<Txn>`.
2. Per top-level statement outside a block: `begin(Implicit)`, then `begin_statement`, execute,
   then `commit`; on error, `rollback`.
3. Inside a block: `BEGIN` → `begin(Explicit)`.
   - Every statement first calls `check_not_failed`. The exceptions are ROLLBACK, ROLLBACK TO and
     COMMIT.
   - An error in the block calls `mark_failed()`.
   - COMMIT of a failed txn returns `RolledBack` (tag `ROLLBACK`).
4. Before each statement: `interrupts().begin_statement(statement_timeout)` and
   `txn.set_limits(WaitLimits{lock_timeout, deadlock_timeout, deadline})`.
5. ReadyForQuery status comes from `TxnState::wire_status()`.

Every other consumer:

- **pgarbor-exec** gets `&mut Txn`. Its calls:
  - `txn.kv("INSERT")` for reads and writes;
  - `lock_key_for_write` before UPDATE/DELETE of an existing row (EvalPlanQual under READ
    COMMITTED);
  - `alloc::next_rowid` + `put_fresh` for inserts;
  - `lock_relation` for DML/DDL table locks;
  - the sequence and advisory functions;
  - `check_interrupts` in loops.
- **pgarbor-catalog** reads and writes catalog rows through the same `TxnKv` (`&mut dyn KvWrite`),
  and builds pg_stat_activity/pg_locks from `diag`.
- **pgarbor-wire**: CancelRequest → `engine.cancel_request(pid, secret)`. Startup →
  `attach_session(SessionRequest{..})`.

## Key invariants

1. **Snapshot pairing.** A root is only ever read through the storage `Snapshot` it came from.
   `Engine::branch_head` returns the matched (snapshot, record, tree) triple.
2. **No dirty reads.** Snapshots contain only durable commits. Overlays are private.
3. **Isolation.**
   - READ COMMITTED and READ UNCOMMITTED refresh the snapshot in `begin_statement`.
   - REPEATABLE READ and SERIALIZABLE keep the first one.
   - SERIALIZABLE equals RR plus write-conflict rules until CORE-16. This is a KNOWN GAP and is
     never reported as PASS.
4. **Write locks.**
   - Every `TxnKv` write takes a key lock, except `put_fresh` and the sequence keyspace.
   - The `LockOutcome` is computed from a fresh head taken *after* every grant or reuse; the
     held-key cache skips only lock-manager acquisition (accepted amendment
     `pgarbor-core-03-reused-key-lock-outcomes`).
   - RR/SERIALIZABLE turn `Changed` into 40001 (`...concurrent update` or `...concurrent delete`).
5. **Commit.**
   - An empty overlay causes no storage write.
   - Otherwise: hold the writer, re-read the record, check the generation guard, `apply` the overlay
     to the current head, `advance_head`, and run the durable `commit`.
   - Locks are released only after durability.
   - A failed commit = rollback + error.
6. **Savepoints.**
   - Implemented with an undo log (cost O(writes since the savepoint)).
   - ROLLBACK TO releases key and xact-advisory locks acquired since the savepoint (oracle-verified)
     and restarts the subxact with a new id.
   - RELEASE merges the savepoint's locks into the parent.
7. **Lock ownership is per session.**
   - A session's own locks never conflict with each other.
   - Scopes are Transaction{txn, subxact} or Session.
   - Tags carry (branch, database).
   - Waits are FIFO with PG's "already holds a conflicting lock" exception.
8. **Deadlocks.**
   - Each waiter checks the wait-for graph (hard and soft edges) once, after `deadlock_timeout`.
   - If the checking session is on a cycle, it aborts itself with 40P01 (PG victim choice,
     oracle-verified).
9. **Sequences.**
   - Non-transactional. State is persisted by a mini-commit only when PG would WAL-log
     (SEQ_LOG_VALS = 32 prefetch).
   - A crash skips values and never repeats one.
   - A branch fork flushes the visible state first, so the child continues exactly from the
     parent's last value.
   - Sequences created in the current txn keep their state in the overlay (unlogged writes).
10. **Destructive branch admin** refuses branches that have other sessions (55006), unless `force`.
    With `force`, those sessions are terminated with FATAL 57P01.

## Formats owned here (CONTRACT)

- Sequence state:
  - key `[0,0,0,2] ++ relid u32 BE`;
  - value v1 = `u8 1 | last_value i64 LE | log_cnt i64 LE | is_called u8` (18 bytes).
- Table row key (shared with exec): `[relid u32 BE][rowid u64 BE]`. Row ids start at 1.
- `TxnId` starts at 3 per process and is not persisted. Backend pids start at 1000.

## Error conventions (PG18 wording, oracle-verified where marked ✓)

| Condition | SQLSTATE | Message |
|---|---|---|
| statement in failed block ✓ | 25P02 | `current transaction is aborted, commands ignored until end of transaction block` |
| savepoint outside block ✓ | 25P01 | `SAVEPOINT can only be used in transaction blocks` (also RELEASE SAVEPOINT / ROLLBACK TO SAVEPOINT) |
| unknown savepoint ✓ | 3B001 | `savepoint "x" does not exist` |
| SET TRANSACTION after query ✓ | 25001 | `SET TRANSACTION ISOLATION LEVEL must be called before any query` / `transaction read-write mode must be set before any query` |
| write in read-only txn ✓ | 25006 | `cannot execute <CMD> in a read-only transaction` (`nextval()`, `setval()` ✓) |
| RR conflict | 40001 | `could not serialize access due to concurrent update` / `... concurrent delete` |
| deadlock ✓ | 40P01 | `deadlock detected`, detail `Process A waits for ShareLock on transaction T; blocked by process B.` lines, hint `See server log for query details.` |
| lock_timeout ✓ | 55P03 | `canceling statement due to lock timeout` |
| statement_timeout / cancel | 57014 | `canceling statement due to statement timeout` / `... user request` |
| terminated | 57P01 FATAL | `terminating connection due to administrator command` |
| advisory unlock miss ✓ | WARNING 01000 | `you don't own a lock of type ExclusiveLock` (ShareLock), returned as a notice, function returns `f` |
| sequence limit ✓ | 2200H | `nextval: reached maximum value of sequence "s" (2)` / `minimum` |
| setval range | 22003 | `setval: value v is out of bounds for sequence "s" (min..max)` |
| currval/lastval ✓ | 55000 | `currval of sequence "s" is not yet defined in this session` / `lastval is not yet defined in this session` |
| branch in use | 55006 | `branch "x" is being accessed by other sessions` + plural detail |
| branch reset under txn | 55000 | `branch "x" was reset or deleted while the transaction was in progress` |
| too many sessions | 53300 FATAL | `sorry, too many clients already` |

## Non-goals / known gaps (v1)

- **SSI is not implemented.** SERIALIZABLE degrades to RR plus write conflicts (CORE-16,
  inventory `serializable_ssi = UNIMPLEMENTED`). READ ONLY DEFERRABLE has no effect.
- **No soft-edge queue rearrangement** in deadlock resolution. Rare cases report 40P01 where PG
  would reorder the queue instead.
- **Key locks are held in memory** until the transaction ends. Bulk UPDATE of N rows holds N lock
  entries (PG stores row locks in tuples). Inserts avoid this through `put_fresh`.
- **Commit cost**: one commit = 2 fsyncs, with no group commit. Sequences do one mini-commit per 32
  nextvals.
- **OIDs**: no wraparound reuse; exhaustion → 54000.
- **Diagnostics privileges**: `pg_stat_activity` shows the query text of all sessions on the
  branch, with no privilege filtering.
- **One tree per txn**: CREATE DATABASE stays 0A000.
- **Not persisted**: `TxnId`s are not persisted, so `txid_current()` restarts after reopen.
