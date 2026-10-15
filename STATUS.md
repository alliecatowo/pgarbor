# PGArbor project state — 2026-10-15, verified on a socket-capable machine

PGArbor is a running pre-alpha SQL engine, not a PostgreSQL-compatible release. Codex's
work (see [runs/review/codex-final-01.md](runs/review/codex-final-01.md)) and the
`feat/bindings-wasm` and `fix/seq-role-parser` branches are merged into main.

Verified on this machine after the merges:

| Layer | Result |
|---|---|
| `cargo build --workspace --all-targets` | PASS (compiler warnings remain) |
| `cargo test --workspace --no-fail-fast` | 2,431 passed, 0 failed, 272 ignored |
| `cargo test -p pgarbor-ffi` | PASS |
| Harness pytest (`-o addopts=""`) | 1,090 passed, 14 skipped, 0 failed |
| `harness/smoke/wire_smoke.py` (live server) | 21 ok, 1 intended duplicate-key error |
| wasm: `demo/build.sh` + `node demo/test-node.mjs` | PASS |
| Python binding tests (`bindings/python`) | 2 passed |

Not re-verified here: the embedded differential corpus figure below was measured by Codex
(1,992 of 5,837 selected cases PASS, 34.13%; 2,320 MISMATCH, 1,163 UNSUPPORTED, 102
internal XX000, 58 unexpected acceptance, 152 blocked; per-family rates in
[runs/review/codex-differential-by-family.csv](runs/review/codex-differential-by-family.csv)).
It compares against existing PG18 recordings, not a live oracle. Live psql, app/ORM and
upstream native suites have not been run since the repairs; ORM results in
runs/review/orm-fixtures-01.md predate them. Raw corpus dumps under runs/review/embedded-*
are intentionally uncommitted.

Known unfinished: SSI, ordered wire COPY, CONTEXT, IANA timezones, additional types,
complete native-runner parity. Open work is in Taskwarrior (`task project:pgarbor list`).
