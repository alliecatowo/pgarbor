# Amendment request pgarbor-exec-01: index entry layout for NULLs in unique indexes (extends ARCHITECTURE §4)

Status: PROPOSED (architect, pgarbor-exec). Implemented as the documented on-disk layout in
`crates/pgarbor-exec/src/index_key.rs` (frozen v1).

## Problem

ARCHITECTURE §4 says index entries are `[index relid][memcomparable key][rowid u64 BE if non-unique]` -> rowid.
That makes the key of a UNIQUE index the encoded column values alone. In PostgreSQL a unique index treats
NULLs as DISTINCT (unless `NULLS NOT DISTINCT`): `INSERT INTO t(a) VALUES (NULL), (NULL)` succeeds on a
unique `a`. With the §4 layout both rows would produce the identical key `[idx][NULL tag]` and the second
insert would overwrite the first entry (or be rejected as a duplicate).

## Proposed change

* **Non-unique** indexes: key `[oid][enc(key columns)][rowid u64 BE]`, value `rowid u64 BE` (as in §4).
* **Unique** indexes, key contains no NULL (or the index is `NULLS NOT DISTINCT`): key `[oid][enc(key columns)]`,
  value `rowid u64 BE` (as in §4). A point lookup of this key is the uniqueness probe.
* **Unique** indexes, key contains at least one NULL and not `NULLS NOT DISTINCT`: key
  `[oid][enc(key columns)][rowid u64 BE]`, value `rowid u64 BE`. Such entries never conflict.
* The entry VALUE is the rowid in every form, so readers (index scans, FK lookups, ON CONFLICT probes) never
  parse the key suffix; equality and range scans use PREFIX ranges (`keycodec` bodies are prefix-free).
* Partial indexes store entries only for rows whose predicate is TRUE; expression keys are evaluated over the
  full row.

## Affected crates

pgarbor-exec (`index_key`, `index_maint`, `node_scan`, `fk`), ARCHITECTURE §4 wording. No change in
pgarbor-types (keycodec) or pgarbor-catalog.
