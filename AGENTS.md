# PGArbor: agent handoff

Read `README.md`, `contracts/ARCHITECTURE.md`, and the contract of the crate you touch
(`contracts/crates/<crate>.md` + `contracts/leases/<crate>.json`). Open work lives in Taskwarrior:
`task project:pgarbor list` (modify tasks by UUID; never put secrets in tasks).

## Hard rules
- PostgreSQL 18.6 is the truth. `tests/expected/**` is recorded only from the pinned oracle:
  `cd harness && PGARBOR_ORACLE_REVIEW=1 uv run python -m pgarbor_harness record [--family F] [--rerecord] --jobs 8`.
  Never hand-edit expected outputs, and never change a case to make PGArbor pass.
- Unsupported behavior returns `PgError::unimplemented("LEASE-ID", ...)` (0A000). No `todo!`,
  `unimplemented!`, `panic!`, fake results, or `unwrap()` on user/wire/file data.
- Engine crates are sync (no async runtime). Dependency direction per ARCHITECTURE §1.
- Contract changes go in `contracts/amendments/*.md`, never as silent edits.
- Commits end with `Co-Authored-By: ...` only. Never put claude.ai session links anywhere.
- Worktrees go in `<repo>/.claude/worktrees/`, never /tmp.

## Pipeline (how work is produced)
1. Architects write skeleton crates + leases (one file per lease).
2. `harness/factory/workflows/wave.js`: one cheap agent per lease, in its own worktree, commits only its file.
3. `python3 harness/factory/assemble.py runs/epoch-01/submissions-XX.json runs/epoch-01/assembly-XX.json`,
   then `cargo build/test -p <crate>`, then repair compile and test failures centrally, then commit.
4. Differential campaign: `uv run python -m pgarbor_harness diff ...` against the pgarbor wire server.

## Status (2026-10-15)
- Implemented and tested: common, storage, fault, types, branches, core, catalog (`cargo test --workspace`).
- Implementation waves running or pending assembly: sql, plan (epoch-02a), exec, pgarbor API (epoch-02b).
- Wire + CLI: contract being written; no runnable server yet.
- Oracle corpus: 5,837 cases; 7 nondeterministic, 7 quarantined, 1 crashes PG itself (see tasks).
- Useful: PG source `~/develop/reference/postgres-18`; oracle container `pgarbor-oracle` (podman, 127.0.0.1:54318).
