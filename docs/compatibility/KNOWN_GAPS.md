# Known compatibility gaps

Reviewed 2026-10-15. This is a list of known limitations, not a complete SQL
compatibility claim. Oracle corpus size and component unit-test counts do not
establish end-to-end PostgreSQL compatibility.

## Serializable isolation (CORE-16)

SERIALIZABLE currently uses REPEATABLE READ snapshots and write-conflict checks.
The SSI tracker in `crates/pgarbor-core/src/ssi.rs` has isolated tests, but normal
transaction reads and writes do not call `on_read`/`on_write`, and transaction
commit/rollback/drop do not call `on_finish`. `commit.rs` calls `pre_commit`, but
an unregistered transaction provides no dependency history to validate. Empty
commits also bypass that write path. The tracker is therefore not operational
end-to-end SSI.

Two transactions can read the same predicate and update different keys, both
commit, and produce write skew that PostgreSQL SERIALIZABLE would reject with
40001. READ ONLY DEFERRABLE does not wait for a safe snapshot. Cases depending
on these guarantees must be reported as unsupported or incompatible, never
inferred to pass from tracker-only tests. Integration must cover every key/range
read, writes, empty commits, rollback and drop before this gap can be closed,
with two-session SQL and engine regressions.

## Resource limits and unsupported type families

JSON input accepts at most 128 nested containers and raises 54001 beyond that
limit. Catalog snapshot construction rejects cyclic/missing domain base metadata
and paths above 64 user metadata nodes with XX001. These conservative safety
limits can reject values PostgreSQL accepts under a larger stack configuration.
Manually constructed internal Rust trees are not made arbitrarily deep-safe by
the JSON parser limit.

IANA timezone conversions remain TYP-25 0A000; UTC, implemented fixed offsets,
POSIX forms and supported abbreviations remain available. Additional Datum
families (network/geometric/range/text-search/XML and others listed in types
amendment 03) remain unsupported where no faithful representation exists.
CONTEXT error storage and wire field W propagation are deferred; strict message
comparison may expose this structural difference.

## Catalog and diagnostics

CAT-39 statistics scans return 0A000 until real counters are collected. They do
not synthesize zero counters. Other known incomplete builtin metadata fields
(pg_proc/pg_type/pg_operator/pg_aggregate defaults), absent system-index rows,
static relation metadata and deparsed pg_node_tree payloads remain catalog
fidelity gaps, detailed in `contracts/crates/pgarbor-catalog.md`. These defaults
must not be treated as exact metadata or measured statistics.

Stored-expression rename plumbing exists through ExprRewriter, but arbitrary
ruleutils-equivalent deparsing still requires differential validation. CREATE
DATABASE and cross-database shared-catalog overlays remain deferred. Forced
branch administration requests termination and wakes waiters; it does not wait
five seconds for connection threads to exit. Generation checks protect stale
transaction writes after reset/delete.
