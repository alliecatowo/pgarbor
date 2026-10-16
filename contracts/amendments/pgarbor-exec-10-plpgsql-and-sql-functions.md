# pgarbor-exec amendment: SQL functions, PL/pgSQL, DO and CALL execution

- `ExecTxn::{subxact_begin, subxact_release, subxact_rollback}` (default 0A000 EXE-06) back PL/pgSQL
  exception blocks; the session transaction view implements them with the core internal subtransactions.
- `StmtState.nonatomic`: true for a top-level CALL / DO, where COMMIT / ROLLBACK inside the routine is allowed.
- New modules `plfunc` (user function dispatch, SQL-language functions, `proconfig`), `plpgsql/*` (interpreter,
  cursors, bind hook) and `spi` (bind / plan / execute for statements issued from a routine).
- Calling a user function requires EXECUTE (`perm::require_function_privilege`, 42501).
- Utility statements that cannot run inside a function (VACUUM, REINDEX CONCURRENTLY, CREATE INDEX
  CONCURRENTLY, CREATE / DROP DATABASE) fail with 25001 `... cannot be executed from a function`.
