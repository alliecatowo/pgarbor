# Amendment request pgarbor-plan-02: btree strategy and hash support metadata for the planner

Status: DEFERRED (lead-engineer decision, 2026-10-15).

## Problem

Index selection (pgarbor-plan `index.rs`) must know, for an operator OID, whether it is a btree comparison
operator (`<`, `<=`, `=`, `>=`, `>`) of an operator family and which family, so that `int8_col = 5::int4`
(operator `int84eq`) can use an `int8` index while `numeric = numeric` cannot use an `int4` index. PostgreSQL
answers this from `pg_amop`/`pg_amproc`, which PGArbor does not carry (catalog contract: `pg_amop` is unsupported).
Hash joins and hashed aggregation need "is this equality operator hashable" (`pg_amop` hash family /
`oprcanhash`).

## Proposed change

Extend `catalog_data` (generated) with:

```rust
pub enum BtreeStrategy { Less = 1, LessEqual, Equal, GreaterEqual, Greater }
pub struct BtreeOperatorInfo { pub op: Oid, pub family: Oid /* pg_opfamily oid */, pub strategy: BtreeStrategy, pub left: Oid, pub right: Oid }
pub fn btree_operator(op: Oid) -> Option<&'static BtreeOperatorInfo>;
pub fn operator_hashable(op: Oid) -> bool;   // pg_operator.oprcanhash
pub fn operator_mergeable(op: Oid) -> bool;  // pg_operator.oprcanmerge
```
generated from the oracle (`pg_amop WHERE amopmethod = 403`, `pg_operator.oprcanhash/oprcanmerge`) into
`contracts/reference/pg18/pg_amop_btree.csv` by the generator. Until then pgarbor-plan uses the documented
heuristic (same type, or int2/int4/int8, float4/float8, text-like, datetime families) and `compare::is_orderable`.

## Affected crates

pgarbor-types (generator + `catalog_data`), pgarbor-plan (`index.rs`, `join.rs`, `agg.rs`).

## Steward decision (2026-10-15)

No verified pg_amop extraction is available in this environment. Do not fabricate catalog metadata or regenerate oracle output. Existing heuristics remain a compatibility risk; mixed-type hash/merge keys must fall back to nested loops until their equality/hash equivalence is proven.
