# Harness contract v1: verification harness + RightShift factory (Python)

Owner: harness architect. Leases: `contracts/leases/harness.json` (HAR-01..31, FAC-01..19).

Seed tasks: ORACLE-01..13 and SWARM-01..15. The seed DAG lists `harness/oracle/**` and
`harness/swarm/**` as owned paths. The code actually lives in `harness/pgarbor_harness/**` and
`harness/pgarbor_factory/**`; see `contracts/amendments/harness-01-dag-owned-path-globs.md`.

## 1. Purpose

- **`pgarbor_harness`**: the independent, deterministic verifier (RFC 002 §2A, RFC 003 §6). It:
  - runs the pinned PostgreSQL 18.6 oracle;
  - records reference behavior;
  - runs the same cases against PGArbor over the PG wire;
  - compares exactly and classifies every item into one Outcome;
  - runs the official PG regress and isolation suites, sqllogictest and wire transcripts;
  - minimizes failures;
  - proves itself with mutants.
- **`pgarbor_factory`**: the RightShift factory (RFC 003). It covers:
  - the task DAG, leases and the lease ledger;
  - worktrees, submissions and deterministic epoch assembly with path permissions;
  - the verifier guard;
  - a central cargo build with error clustering;
  - full evidence campaigns, evidence packs and reports;
  - failure clustering, repair-epoch DAGs and halt rules.

An LLM never decides PASS. Only `compare.classify` (differential cases) or the suite runners
do, on recorded PG output.

## 2. Environment

- Python ≥ 3.12, managed with uv: `harness/pyproject.toml`.
  - Dependencies: `psycopg[binary]>=3.2`, `pyyaml>=6.0`. Dev: `pytest`.
  - No other dependencies. Notably no `jsonschema`, no asyncio drivers, no podman bindings.
- Run with `cd harness && uv run python -m pgarbor_harness ...` / `uv run python -m pgarbor_factory ...`
  and `uv run pytest`.
- Import rule: every module imports with the stdlib alone. PyYAML is allowed in `cases.loader`
  and `generate`. psycopg is imported lazily inside functions of `runner.session`,
  `oracle.pool`, `oracle.fingerprint`, `runner.targets`. `pgarbor_factory` may import
  `pgarbor_harness`; the reverse is forbidden.
- No async: threads plus `select` only.
- Podman is the only container tool. Psql always runs from the pinned image, so a local psql
  is never needed.

## 3. Frozen, implemented modules (never leased; changes need an amendment)

| Module | Contents |
|---|---|
| `pgarbor_harness/config.py` | image digest + server version, initdb args, server/session GUC profiles, pg_regress env profile, timeouts, `Paths`, `PROTECTED_GLOBS`, `VERIFIER_GLOBS` |
| `pgarbor_harness/model.py` | `Outcome`, `ColumnDesc`, `ErrorInfo`, `Notice`, `StatementResult`, `StepObservation`, `Transcript`, `ExpectedFile`, `ReferenceIdentity`, `Diff`, `CaseResult`, `Endpoint`, `canonical_json`, `expected_path` |
| `pgarbor_harness/hashing.py` | sha256 helpers, `glob_match` (`**` semantics), `tree_sha256` (byte-exact algorithm) |
| `pgarbor_harness/cases/model.py` | `Case`, `Step` (+ defaults applied by `from_dict`) |
| `pgarbor_factory/records.py` | `locked_json`, `LeaseState` + transitions, `LeaseEntry`, `LeaseSpec`, `Submission`, `EvidencePack`, `FamilyEvidence`, `RepairTicket`, `SEVERITY_WEIGHT`, `COUNT_KEYS` |
| `pgarbor_factory/gitutil.py` | git subprocess wrappers |
| `pgarbor_factory/dag.py` (`find_cycle`, `validate_graph`, `validate_leases`, `main`) | original bootstrap validator. `harness/factory/dag.py` is a shim over it, and `harness/factory/test_dag.py` must keep passing. |
| `contracts/oracle/case.schema.json` | case JSON Schema (draft 2020-12) |
| all `__init__.py` / `__main__.py` | package structure (the analog of lib.rs/mod.rs) |

## 4. Pinned oracle (ARCHITECTURE §0.6)

- Image: `docker.io/library/postgres@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336`.
  `server_version` is `18.6 (Debian 18.6-1.pgdg13+2)`.
- initdb: `--encoding=UTF8 --locale-provider=builtin --locale=C.UTF-8`.
- Server GUCs: `TimeZone=UTC`, `DateStyle=ISO, MDY`, `IntervalStyle=postgres`,
  `extra_float_digits=1`, `lc_messages=C.UTF-8`, `autovacuum=off`, plus performance-only
  `fsync=off`, `synchronous_commit=off`, `full_page_writes=off`, `max_connections=200`.
- Container: the harness owns its own container, `pgarbor-harness-pg18` on `127.0.0.1:54319`,
  with volume `pgarbor-harness-pg18-data`. The architects' ad-hoc container `pgarbor-oracle` is
  never touched.
- Regress mounts: the regress source and output dirs are mounted at their identical host
  absolute paths. Server-side `COPY` from `abs_srcdir` then works on the oracle, on a
  host-run PGArbor, and in the psql client container.
- Isolation: one container serves a campaign (ORACLE-12). Each case gets
  `CREATE DATABASE pgarbor_case_<pid>_<10hex> TEMPLATE pgarbor_case_template` and is dropped
  `WITH (FORCE)` afterwards. Cluster-global objects (roles) are checked after every case.
- Fingerprint: a canonical JSON document (oracle.fingerprint) holding the digest, version,
  database locale and 29 settings as a case session sees them. Its sha256 is stored in every
  expected file and evidence pack. Drift makes the pool refuse to start (HARNESS_ERROR); it
  never continues silently.

## 5. Case corpus and expected files

- Cases live in `tests/cases/<family>/*.yaml` (`pgarbor-case/1`).
  - Schema: `contracts/oracle/case.schema.json`. Validator: `cases.validate`. Rules:
    `tests/cases/README.md`.
  - Step kinds: `sql`, `parse`, `describe`, `execute`, `exec_params`, `close`, `wait`,
    `reconnect`, `restart`.
  - `async: true` plus a later `wait` give multi-session interleavings. `expect_blocked`
    asserts a lock wait.
  - `expect`/`sqlstate` are author intent only, checked at record time.
- Expected outputs are written only by `oracle.recorder` (gated by `PGARBOR_ORACLE_REVIEW=1`)
  to `tests/expected/<family>/<case_id>.json`.
  - The path rule is `model.expected_path`. The format is `ExpectedFile` (`pgarbor-expected/1`),
    serialized with `model.canonical_json`: sorted keys, indent 2, UTF-8, trailing newline.
  - The recorder runs each case twice. Nondeterminism, intent mismatch, failed setup or
    missing setup observations mean nothing is written. Public single-case writes enforce
    the same review gate and live oracle identity check as corpus writes (amendment harness-03).

Example (abridged):

```json
{
  "case_id": "errors.unique-violation-extended",
  "case_sha256": "<sha256 of the yaml bytes>",
  "format": "pgarbor-expected/1",
  "reference": {"fingerprint_sha256": "<sha>", "image_digest": "sha256:74935e72...",
                "server_version": "18.6 (Debian 18.6-1.pgdg13+2)"},
  "transcript": {
    "case_id": "errors.unique-violation-extended", "endpoint": "reference",
    "setup": [{"index": 0, "session": "setup", "kind": "sql", "txn_status": "I",
               "results": [{"kind": "command", "command_tag": "CREATE TABLE", "columns": [],
                            "rows": [], "error": null, "param_type_oids": null}],
               "notices": [], "blocked_observed": null, "timed_out": false,
               "connection_lost": false, "harness_error": null, "elapsed_ms": 0}],
    "steps": [
      {"index": 4, "session": "a", "kind": "execute", "txn_status": "I",
       "results": [{"kind": "error", "columns": [], "rows": [], "command_tag": null,
                    "param_type_oids": null,
                    "error": {"severity": "ERROR", "sqlstate": "23505",
                              "message": "duplicate key value violates unique constraint \"u_pkey\"",
                              "detail": "Key (k)=(alpha) already exists.", "hint": null,
                              "position": null, "internal_position": null,
                              "internal_query": null, "context": null,
                              "schema_name": "public", "table_name": "u", "column_name": null,
                              "datatype_name": null, "constraint_name": "u_pkey"}}],
       "notices": [], "blocked_observed": null, "timed_out": false,
       "connection_lost": false, "harness_error": null, "elapsed_ms": 0}
    ]
  }
}
```

Value encoding:
- Text-format values are JSON strings, exact server text.
- Binary-format values are `{"hex": "..."}`.
- NULL is `null`, never `""`.
- `COPY TO STDOUT` is a `copy_out` result with one row per CopyData message.

## 6. Comparison and classification (ORACLE-04)

`compare.typed` compares everything below exactly; only `elapsed_ms` is ignored:
- per step: missing step, timeout, connection loss, `blocked_observed`, `txn_status`
  (I/T/E), result count;
- per result: kind, columns (name, type OID, typmod, format, origin), command tag,
  parameter type OIDs, rows;
- errors: SQLSTATE always; message/detail/hint/context unless the step sets
  `compare_message: false`; position and structured fields always;
- notices, in order.

Row order:
- `ordered` when the outermost statement has ORDER BY (`order: auto`), or when the case says
  `order: ordered`;
- otherwise a multiset comparison that keeps duplicates.

Normalizers live in `compare.normalize.REGISTRY` and must be idempotent and documented with a
justification. `user_oids` is the only default: a bijective canonical renumbering of descriptor
OIDs ≥ 16384 per transcript. OID-typed row data stays exact until a separately reviewed
allocated-object provenance schema exists (amendment harness-03-verifier-integrity). The opt-ins are `timestamps`, `uuids`, `pids`, `xids`, `lsns` and
`explain_numbers`. SQLSTATE, NULL-ness, built-in type OIDs, transaction status and row
multiplicity are never normalized. A new normalizer requires an amendment plus a selftest
mutant.

`compare.classify` gives exactly one Outcome per item. The first matching rule wins:
1. HARNESS_ERROR.
2. CORRUPTION: XX001/XX002, or corruption evidence.
3. CRASH: connection lost, XX000, or panic evidence.
4. HANG: timeout.
5. PASS: no diffs.
6. Otherwise the first divergence decides:
   - target 0A000 → UNSUPPORTED;
   - reference error with target success → UNEXPECTED_ACCEPTANCE;
   - else → MISMATCH.

The differential driver adds the remaining outcomes:
- INVALID_CASE: schema violation, quarantine, intent mismatch or nondeterministic reference.
- BLOCKED: missing or stale reference, or a missing capability.
- FLAKY: a rerun gives a different target transcript.

Only PASS passes. UNSUPPORTED, BLOCKED, FLAKY and UNRUN stay visible in every denominator.

## 7. Official suites

- **Regress (ORACLE-05)**: the manifest of `parallel_schedule`, sql, expected variants
  `_0.._9` and `resultmap`. psql runs from the image with pg_regress's exact environment:
  `PGTZ=America/Los_Angeles`, `PGDATESTYLE=Postgres, MDY`, `LC_MESSAGES=C`, and the flags
  `-X -a -q -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on`. stdout and stderr are merged
  inside the container. The database is `regression`, created from template0 with
  pg_regress's six `ALTER DATABASE` settings. Output passes if it equals any variant after
  `--strip-trailing-cr`.
  - The stock image ships no `regress.so`, so `test_setup` and some later tests fail even on
    PG.
  - A PG baseline recorded into `tests/expected/regress/` turns those failures into
    `regress-official: BLOCKED(oracle_env)`, while `regress-differential` still compares
    PGArbor against PG's actual output. Neither is ever silently skipped.
- **Isolation (ORACLE-06)**: spec parser for all 124 specs, plus an isolationtester-compatible
  runner with exact PQprint output. Wait detection uses
  `pg_isolation_test_session_is_blocked`. Without it (PGArbor today) the heuristic fallback
  is never scored PASS: identical output is `BLOCKED(heuristic_wait_detection)`.
- **sqllogictest (ORACLE-07)**: PGArbor dialect, where values are PG's exact text output.
  Results are recorded from PG into `tests/expected/slt/`.
- **Wire transcripts (ORACLE-08)**: a recording proxy (JSONL, secrets redacted), a codec,
  and replay with documented normalizations only (BackendKeyData, ParameterStatus order,
  user OIDs, the F/L/R error fields).

## 8. Factory data formats (all canonical JSON)

| File | Shape |
|---|---|
| `contracts/leases/<crate>.json` | `{"crate", "contract_version", "leases": [{lease_id, task_ids, files, model_tier, summary, spec, reads, depends_on_leases, acceptance}]}` |
| `runs/state/leases.json` | `records` ledger `pgarbor-lease-ledger/1`: `{lease_id: LeaseEntry}`; transitions `records.LEASE_TRANSITIONS` (RFC 003 §2 state machine + REJECTED) |
| `runs/submissions/<ID>.json` | `records.Submission` (`pgarbor-submission/1`) |
| `runs/epoch-<NN>/assembly.json` | `assemble.AssemblyReport` |
| `runs/<campaign>/evidence.json` | `records.EvidencePack` (`pgarbor-evidence/1`): git sha/dirty, rustc/cargo, platform, PG identity, corpus tree hashes, seeds, durations, per-family counts over `COUNT_KEYS` with three denominators, critical counts, halt decision |
| `runs/<campaign>/repair_dag.json` | `{"tasks": [RepairTicket]}`, valid under `dag.validate_graph` |
| `runs/state/oracle_manifest.json` | `oracle.manifest` (`pgarbor-oracle-manifest/1`) |
| `runs/state/evidence_ledger.jsonl`, `runs/state/oracle_reviews.jsonl` | append-only ledgers |

Repair scoring follows RFC 003 §7: `SEVERITY_WEIGHT[sev] × unblocked × fanout × confidence / cost`.
CORRUPTION/CRASH tickets on storage, fault, branches or core files always sort first.

## 9. Anti-reward-hacking invariants

1. Feature (Rust) leases can never touch the following; `assemble.check_patch` rejects it:
   - `contracts/**`, `harness/oracle/**`, `tests/expected/**` (protected);
   - `harness/**`, `tests/cases/**` (verifier);
   - Cargo.toml, lib.rs, mod.rs, `__init__.py`, `__main__.py`.
2. Harness leases are bound to their own files only. The recorder, manifest freeze, regress
   baseline, slt record and review records require `PGARBOR_ORACLE_REVIEW=1`, which is the
   oracle review path, never a worker.
3. `guard.check_campaign_preconditions` refuses a campaign when any of these holds; there is no
   override flag:
   - a protected, verifier, cases or expected file changed without a covering review record;
   - the denominator shrank;
   - a quarantine was added unreviewed;
   - `selftest.run_offline` fails.
4. `selftest` mutants (17 observation mutants plus one negative control, and 5 comparator
   mutants with kill relations) must all be detected.
5. Stub markers have the exact form `NotImplementedError("<LEASE-ID>: <what>")` and
   `PgError::unimplemented("<LEASE-ID>", ...)`. They are counted by `inventory` and must be
   covered 1:1 by leases (`leases.validate_catalog`). Rust cfg(test) module constructor
   examples are excluded from production inventory (amendment harness-03).

## 10. How downstream uses it

- **Rust crates**: they are verified by campaigns. No Rust crate imports the harness.
- **pgarbor-cli**: must provide `pgarbor create <file>` and
  `pgarbor serve <file> --host --port --branch`, with a readiness line and SIGTERM semantics
  (amendment harness-02).
- **pgarbor-wire**: must honor startup `options` GUCs and report `server_version`.
- **Workers**: get their context from `python -m pgarbor_factory packet <ID>`. They commit on
  `lease/<ID>` in `.claude/worktrees/lease-<id>` and submit.

## 11. Non-goals and known gaps (v1)

- No TLS/SCRAM transcripts yet: trust auth only, and secrets are redacted.
- No libpq-level fine-grained Parse/Bind/Execute without Sync. Message-level sequences are
  covered by wire transcripts instead.
- `close` steps need libpq ≥ 17 in the psycopg binary wheel. If it is older, the step becomes
  HARNESS_ERROR and is never emulated.
- Isolation runs against PGArbor score BLOCKED until it implements
  `pg_isolation_test_session_is_blocked`.
- Regress tests that need `regress.so` can at best reach differential PASS. They can never
  reach official PASS on this oracle image.
- SSI write-skew histories (ARCHITECTURE §7 known incompatibility) show up as MISMATCH, never
  PASS.
- No SQLancer-style metamorphic generators yet (ORACLE-09 seeds are catalog-driven). No
  cross-platform crash rigs.
- The psycopg `pq` method names in `runner/session.py` were written without a local psycopg
  install. HAR-09 must verify them against the installed package.
