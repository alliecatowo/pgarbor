# Amendment request pgarbor-exec-02: requirements of the executor on pgarbor-plan, pgarbor-sql and the embedding session

Status: ACCEPTED WITH LIMITS (lead-engineer decision, 2026-10-15).
panics) where an upstream promise might not hold. Each item names the executor behavior if it is not honored.

## Requirements on pgarbor-plan

1. **Equal key types in hash/merge joins.** `HashJoinNode::outer_keys[i].ty == inner_keys[i].ty` (and likewise `MergeKey`s): the
   planner wraps a key in an implicit `Cast` when the `=` operator is cross-type (`int4 = int8`). The hash join encodes keys with
   `keycodec` (equal SQL values <=> equal bytes only within one type). Otherwise `EXE-17` fails XX000 `hash join key types differ`.
2. **No `SetToDefault` in `UpdateSpec::assignments`/`InsertSpec` inputs.** plan.rs documents it as replaced by the planner;
   the executor still evaluates a leftover `SetToDefault{attnum}` from `ModifyTarget.meta.columns[attnum-1].default` (NULL if none).
3. **EvalPlanQual shape.** For UPDATE/DELETE on a single table the input is `[Filter]* -> SeqScan|IndexScan(with_rowid)` with the
   WHERE quals in the scan `filter`/`index_cond`/`Filter` nodes (no projection in between). Anything else gets 40001 on a concurrent
   change under READ COMMITTED (`modify::epq_policy`).
4. **`ModifyTarget::checks`** are informational for EXPLAIN; the executor enforces `TableDesc::checks` (bound from the catalog text
   once per statement) so cascades and `ON CONFLICT DO UPDATE` use the same list. They must describe the same constraints.
5. `VirtualScan::eq_filters` use 1-based attnums (`(attnum, value)`); the executor passes `(attnum - 1) as usize` to `vrel::ScanRequest`.
6. Sub-plan `OutCol`s of the plan root are the types of ANY/ALL comparison columns (`SubPlanRt::out`).

## Requirements on pgarbor-sql

1. `binder::stored::{table_expr_scope, bind_stored_expr}` and `binder::expr` entry points are callable at RUN time with a throw-away
   `Binder` (the executor re-binds catalog expression text: partial-index predicates, expression-index keys, CHECKs, domain CHECKs).
   Domain CHECK text refers to `VALUE`; the executor binds it in a synthetic one-column scope named `value` (`table_expr_scope`) and
   rewrites the column to `InputRef(0)`. A convenience `stored::bind_domain_check(b, sql, base_type) -> BoundExpr` would remove the
   workaround (optional).
2. `pgarbor_sql::rewrite::SqlExprRewriter` is passed to every rename/set-schema catalog call (catalog amendment 04).
3. EXPLAIN of `EXECUTE name` is resolved by the SESSION (the executor never sees prepared statements).

## Requirements on pgarbor-core / pgarbor-catalog

1. `pgarbor_core::alloc::next_rowids` and `TxnKv::put_fresh` are the only heap-insert path (core amendment 01); `TxnKv` is the sole
   production implementor of `pgarbor_exec::ExecTxn`.
2. `Txn::lock_key` must keep reporting `Changed(latest)` for keys the transaction itself already modified only if the latest committed
   value differs; the executor prefers the transaction's own overlay value in that case (`ExecTxn for TxnKv`).
3. `pgarbor_catalog::persist::oid_seed` is the OID seed for `ExecTxn::alloc_oid`.

## Requirements on the embedding session (`pgarbor` crate)

1. Treat `pgarbor_exec::error::is_catalog_retry` as "re-snapshot, re-bind, re-run" under READ COMMITTED (bounded retries).
2. Provide `GucAccess` (`set_config`, `current_setting`, `pg_settings`) and `EngineServices` (activity, locks, notify, cancel).
3. Transaction control, SET/SHOW, PREPARE/EXECUTE, LISTEN/NOTIFY, DISCARD are session statements: the executor returns XX000 for them.

## Steward decision (2026-10-15)

Accept row-slot layouts, 1-based virtual attnums, runtime stored-expression rebinding, session utility dispatch, bounded pre-effect catalog retry, and overlay precedence. Mixed-type hash/merge keys require a semantically correct nested-loop fallback. EPQ on unsupported shapes remains 40001, not a false successful recheck. These are required invariants, not proof that concurrency and expression parity pass.
