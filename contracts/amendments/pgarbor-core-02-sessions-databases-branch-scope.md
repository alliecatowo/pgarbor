# Amendment request pgarbor-core-02: session binding, logical databases, branch-scoped diagnostics

Status: ACCEPTED WITH DEFERRED CLAUSES (2026-10-15, lead engineering review).

## Problem

RFC 001 §9 says one branch may hold several logical PostgreSQL databases, and that a connection binds
to one branch for its lifetime. ARCHITECTURE §4 and §7 assume a single tree per branch and do not say
how `CREATE DATABASE`, lock tags or `pg_stat_activity` behave across branches.

## Proposed change

- **Database scoping.** Each logical database is its own data tree:
  `BranchRecord.data_root` holds oid 5 (`postgres`), and `extra_databases` holds the others (see
  pgarbor-branches-01). ARCHITECTURE §4's keyspace applies *per database tree*, unchanged.
  - Shared catalogs (roles, pg_database, pg_tablespace) live in the oid-5 tree in v1.
  - CREATE DATABASE stays unimplemented (`0A000`). When it lands, a txn on database X reads shared
    rows from tree 5. This needs a second overlay per txn, which is a later amendment.
- **Session binding.** `SessionRequest { branch, database_oid, database_name, user, .. }`. The
  pgarbor crate resolves the startup `database` name through the catalog. v1 accepts only oid 5.
  Any other database → 3D000 from the catalog (e.g. `database "x" does not exist`).
- **Lock tags** carry (branch, database). Different branches and databases never contend.
- **Diagnostics.** `pg_stat_activity` and `pg_locks` show only sessions and locks of the viewer's
  branch: each branch listener looks like its own cluster. A PGArbor-specific view lists every
  branch's sessions.
- **Destructive branch admin with live sessions.**
  - Without force: 55006 `branch "x" is being accessed by other sessions`, plus the PG-style
    plural detail.
  - With force: terminate those sessions (FATAL 57P01) and wait up to 5 s, as PG's
    `DROP DATABASE ... WITH (FORCE)` does.
  - An in-flight commit on a reset branch fails the generation guard with 55000.

## Affected crates

pgarbor (session startup → `SessionRequest`), pgarbor-catalog (virtual pg_stat_activity/pg_locks
rows from `pgarbor_core::diag`), pgarbor-wire (CancelRequest → `Engine::cancel_request`),
pgarbor-cli (`branch delete/reset --force`).


## Steward decision (2026-10-15)

Accept branch/database lock tags, session binding, branch-scoped diagnostic filtering, and generation checks. Logical CREATE DATABASE and shared-catalog cross-tree overlays remain deferred. The proposed forced-admin 5-second wait is NOT implemented: Engine requests termination and wakes waiters after mutation; generation checks reject stale commits. Accept that current behavior as an explicit divergence, not a wait guarantee. No new public all-branch SQL view is ratified.
