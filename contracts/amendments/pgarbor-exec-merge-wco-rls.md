# pgarbor-exec amendment: MERGE execution, WCO enforcement, RLS activation

- `modify.rs` executes `ModifyOp::Merge` (21000 on second modification of a row).
- `StmtState.wco`; `constraints::enforce` runs RLS WCOs before NOT NULL/CHECK and view check options after (44000 / 42501).
- Supersedes exec-03 for row security: tables with relrowsecurity are accessible; policies are enforced by the binder.
- `ddl_rewrite::exec_policy` for POLICY DDL.
