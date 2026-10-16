# pgarbor-sql amendment: MERGE, auto-updatable views, row-level security, POLICY DDL

- New `BoundStatement::Merge(BoundMerge)` (+ `MergeMatchKind`, `MergeActionKind`, `BoundMergeWhen`) and
  `BoundStatement::Policy(PolicyStmt)`; `StmtFamily::{Merge, Policy}`; classify routes MERGE/CREATE|ALTER|DROP POLICY.
- `TargetTable.wco: Vec<TargetWco>` (`WcoKind::{ViewCheck, RlsInsert, RlsUpdate}`): view check options and RLS WITH CHECKs.
- `binder::view_dml`: DML on auto-updatable views is rewritten at bind time to the base table (PG rewriteTargetView);
  `binder::rls`: policies applied at bind time (FROM tables wrapped in a filtered sub-select; UPDATE/DELETE quals; WCOs).
- `diag::invalid_from_reference` now uses DETAIL (PG errorMissingRTE), not HINT.
- Known gaps: CREATE/ALTER/DROP RULE execution (still 0A000), RLS on MERGE / ON CONFLICT DO UPDATE (0A000),
  RETURNING old/new for INSERT/UPDATE/DELETE, nested view definitions containing sub-queries (0A000).
