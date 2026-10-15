# Differential case corpus (`pgarbor-case/1`)

Each YAML file here is one differential test case: SQL and protocol actions run on the pinned
PostgreSQL 18.6 oracle and on PGArbor, and the two runs are compared exactly.

- Schema: `contracts/oracle/case.schema.json`.
- Python model: `harness/pgarbor_harness/cases/model.py`.
- Validator: `harness/pgarbor_harness/cases/validate.py`.

## Hard rules

1. **No expected values in a case.** Expected outputs are only ever recorded from the pinned
   PG oracle into `tests/expected/<family>/<id>.json`. The trusted recorder does this
   (`python -m pgarbor_harness record`, oracle review path only). Nobody writes or edits
   files under `tests/expected/` by hand, including feature workers and corpus authors.
2. **Intent, not values.** `expect` (`ok|rows|error|notice|any`), `sqlstate` and
   `expect_blocked` state what the author believes PG does. The recorder checks them against
   the recording. If they disagree, the case is `INVALID_CASE` and nothing is recorded.
3. **Determinism.** A case must produce byte-identical results on two PG runs. The recorder
   runs every case twice. Avoid `now()`, `random()`, `gen_random_uuid()`, PIDs, XIDs and LSNs.
   If you cannot avoid them, list a documented normalizer per step, for example
   `normalize: [timestamps]`.
   - User OIDs (≥ 16384) are always canonicalized.
   - Row order is compared exactly only when the outermost query has `ORDER BY`. Use
     `order: ordered|multiset` to be explicit.
4. **Block scalars for SQL.** Always write `sql: |`. YAML 1.1 turns bare `on`, `no`, `yes`
   into booleans, which the validator rejects.
5. **One family per directory.** `id` is `<family>.<name>`, its first segment equals `family`,
   and the file lives in `tests/cases/<family>/`. Ids are unique across the corpus and never
   reused for different content.

## Layout

| Directory | Contents |
|---|---|
| `smoke/` | north-star vertical slice (tier `slice`), run first |
| `ddl/`, `dml/`, `query/` | statements by kind |
| `types/` | type input/output, casts, operators, functions (use `types` as family; feature ids carry the type) |
| `txn/` | transactions, isolation levels, savepoints, locks, multi-session |
| `errors/` | SQLSTATE and message parity for error paths |
| `catalog/` | `pg_catalog` / `information_schema` introspection used by drivers and ORMs |
| `protocol/` | extended-protocol behavior (`parse`, `describe`, `execute`, `exec_params`, `close`) |
| `durability/` | `restart` steps (requires capability `server-restart`) |
| `gen-<generator>/` | machine-generated (`python -m pgarbor_harness generate`), provenance `generated` |
| `min-*` ids | minimized reproductions promoted from campaigns (provenance `minimized`) |

## Step kinds

`sql` (simple Query, may hold several statements), `parse`, `describe`, `execute`,
`exec_params`, `close`, `wait` (collects the pending `async: true` step of that session),
`reconnect`, `restart`.

- Every step except `restart` may name a `session` from `sessions` (default `[a]`).
- `setup` runs first, in its own autocommit session, and is compared too.
- Every case runs in a fresh, empty database: a clone of `pgarbor_case_template` on PG and a
  fresh `.pga` file on PGArbor.

## Examples

- `smoke/create-insert-select.yaml`: the north-star slice, with ordered and multiset reads,
  NULL vs empty string, a transaction block and a reconnect.
- `txn/read-committed-blocking-update.yaml`: two sessions; a row-lock wait detected with
  `async` + `expect_blocked`, then re-evaluation after the holder commits.
- `errors/unique-violation-extended.yaml`: prepared statements over the extended protocol,
  23505 and then 25P02 in the aborted transaction block.

## Outcomes

`PASS`, `MISMATCH`, `CRASH`, `HANG`, `CORRUPTION`, `UNEXPECTED_ACCEPTANCE`, `UNSUPPORTED`,
`BLOCKED`, `FLAKY`, `INVALID_CASE`, `HARNESS_ERROR`.

- Only `PASS` counts as a pass.
- Reports always show three denominators: passed, runnable and cataloged.
- Items that were not executed are reported as `UNRUN`.
