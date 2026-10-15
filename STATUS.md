# PGArbor project state — 2026-10-15, verified Codex resume

PGArbor is a running pre-alpha SQL engine. All three implementation waves are assembled;
this is not a PostgreSQL-compatible release. The detailed evidence and next steps are in
[runs/review/codex-final-01.md](runs/review/codex-final-01.md).

| Layer | Current measured evidence |
|---|---|
| Workspace build, all targets | PASS; compiler warnings remain |
| Rust workspace tests | 2,409 PASS, 11 socket-permission failures, 272 ignored (includes doctests; child helpers counted once) |
| Harness pytest | 1,072 PASS, 18 socket-permission failures, 14 skipped; lease validation passes |
| Socket-excluded harness | 1,060 PASS / 14 skipped; exit 0 |
| Embedded SQL session acceptance | 10 PASS; first-command SHOW/BEGIN/ROLLBACK, DML, information_schema, rollback, identity and fail-closed security/deferred semantics |
| Scripted raw-byte wire/session e2e | 3 PASS: startup/simple Query/txn, UPDATE/DELETE/catalog, extended parameter path |
| SQL files/branches/fault/compaction | reopen 5, branches 4, export/handles 3, SQL fault recovery 4, compaction 2, RC/RR/atomicity 3 PASS |
| CLI process/branch/round-trip | 5 + 5 + 7 PASS; COPY FROM script blocks and conservative ordinary-table dump/import work |
| Embedded differential corpus | 1,992 / 5,837 selected PASS (34.13%), 400 families; 5,635 executed; full outcomes below |
| Live wire/psql/apps/native suites here | BLOCKED by sandbox socket/Podman restrictions; no fresh compatibility claim |

Final embedded outcomes after two 60-second rechecks: 1,992 PASS, 2,320 MISMATCH,
1,163 UNSUPPORTED, 102 CRASH-class internal XX000 errors (no observed process abort),
58 UNEXPECTED_ACCEPTANCE, 10 bridge HARNESS_ERROR, 152 BLOCKED, 40 quarantined
INVALID_CASE, zero confirmed HANG, zero unaccounted selected cases. Per-family rates:
[runs/review/codex-differential-by-family.csv](runs/review/codex-differential-by-family.csv).
The comparison uses existing PG18 recordings; no live oracle verification or rerecording
was possible. No tests/cases or tests/expected files were edited. Nondeterministic masks
and descriptor OID identity mapping still limit verifier confidence; see final review.

Original safety findings have regression-tested fixes: export/detach temp ownership and
atomic no-clobber; JSON input depth; domain graph validation; hostile superblock arithmetic;
repeated lock outcome; literal OID comparison; recorder setup/review gates; stale-inode
recovery. Statistics fail closed rather than synthesizing counters. Amendments record
accepted/partial/deferred decisions; SSI, full ordered wire COPY, CONTEXT, IANA timezones,
additional types and complete native-runner parity remain unfinished.

The owner's intervening commits fixed the three outside-sandbox smoke bugs (snapshot-free
utility/txn commands, duplicate UPDATE/DELETE target namespace, builtin information_schema
domains), plus subquery filtering and catalog producers. Current embedded and scripted-wire
regressions pass. Real `harness/smoke/wire_smoke.py` still needs an outside-sandbox rerun.
Inherited app results are in runs/review/orm-fixtures-01.md and predate later repairs.

Next priorities: review/commit the current changes, fix normalization provenance, triage
unexpected acceptance and XX000 clusters, wire real SSI, then run psql/apps/upstream native
suites outside the sandbox and expand SQL compatibility using retained transcripts.

Operational blockers: `.git` is read-only, preventing new commits here; push retry cannot
resolve github.com; Taskwarrior cannot open its database/WAL, preventing UUID reads/updates.
No tasks were modified and this resume creates no commits. Logical patches and outstanding
task updates are retained under runs/review. Never update tasks using display numbers.
