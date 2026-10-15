# pgarbor-fault — crate contract v1

Status: RATIFIED skeleton. Leases: `contracts/leases/pgarbor-fault.json` (FLT-01..FLT-10).
Depends on: pgarbor-common, pgarbor-storage (`FileIo`, `Vfs`, `Container`, ...).

## Purpose

Deterministic, seed-reproducible verification of the storage durability contract (RFC 001 §5.2, §11.2, §14
Phase 1 gate): "fuzzed crash/restart never violates acknowledged-commit or root-reachability invariants".

## Public API

| Item | Use |
|---|---|
| `DetRng` | SplitMix64; `new(seed)`, `next_u64`, `below`, `chance(permille)`, `bytes`, `fork` (no `rand` in the simulation path) |
| `FaultPlan`, `FaultKind`, `ScheduledFault`, `CrashMode`, `OpKind` | what fails when; crash points are *mutating-op indexes* |
| `FaultController` | shared per disk; `decide(kind, path, offset, len) -> Decision`; `crashed()`, `trigger_crash()`, `log()` |
| `FileState`, `Pending` | per-file durable/current/pending model; `crash_image`, `fsync_fail` (fsyncgate) |
| `FaultFile` | `FileIo` over a `FileState` |
| `FaultVfs`, `DiskImage` | `Vfs` with non-durable directory ops; `crash_image()`, `live_image()`, `from_image()` |
| `RefModel`, `Verdict`, `RootsState`, `read_state` | oracle of committed state per named root |
| `WorkloadSpec`, `WorkOp`, `generate`, `Executor`, `StepOutcome`, `RunSummary` | deterministic workloads executed through the real storage API |
| `scheduler::{run_scenario, crash_matrix, explore, count_mutating_ops, repro_line, env_repro}`, `Scenario`, `ScenarioReport`, `MatrixReport` | crash scheduler / property driver |

Storage safety amendment `contracts/amendments/pgarbor-storage-03-safe-publication-and-recovery.md`
also applies: FaultFile supplies live object identity, and FaultVfs no-replace publication uses
the existing Rename fault/durability operation with the destination check under the namespace mutex.

## Disk model (CONTRACT)

- Each file: `durable` bytes, `current` bytes (reads), ordered `pending` ops (writes, set_len).
  `sync_data`/`sync_all` → durable = current.
- Power loss image: durable + subset of pending per `CrashMode` — `DropUnsynced`, `KeepUnsynced`, or
  `Random{keep, tear}` (independent per op → reordering; tearing at 512-byte absolute sector boundaries).
- Failed fsync (fsyncgate): pending ops randomly persisted/torn/lost and **forgotten** (current unchanged).
- Directory ops (create/rename/remove) are pending until `sync_dir(parent)`; a crash keeps a random *prefix* of
  them (ordered metadata journal model). Handles stay bound to file objects across rename/remove (Unix).
- ENOSPC = `io::Error::from_raw_os_error(28)` (maps to SQLSTATE 53100 in storage); EIO = 5.
- After a `Crash` decision, every op fails with `io::Error::other("pgarbor-fault: simulated crash")`.

## Oracle invariants (what a scenario checks)

1. Recovery of any crash image succeeds (never XX001) — except a crash inside `Container::create` (it had not
   returned Ok): then NotFound, any XX001 open error, or an empty database are all acceptable. Once `create`
   returned Ok, commit 1 is promised durable and open must succeed.
   Torn superblocks/page headers are only reachable with `sector_size` <= 64 (the 92 meaningful superblock
   bytes and 32-byte page headers fit inside one 512-byte sector), so tests targeting CRC fallback use
   `sector_size: 16`.
2. Recovered state == last acknowledged commit, or == the single in-flight commit. Never a mix, never older.
3. `Container::verify(VerifyLevel::Full)` reports no Error issues (reachability, checksums, ordering).
4. The recovered container accepts a new commit and reopens again.
5. No `commit()` returns `Ok` when any of its page writes, syncs or superblock write failed.

Failures are data (`ScenarioReport { passed() == false, repro }`), never panics; the repro line is
`PGARBOR_FAULT_SEED=<s> PGARBOR_FAULT_CRASH_AT=<k>`.

## Error conventions

Harness code returns `pgarbor_common::Result`; `Err` means the harness itself is broken (incl. lease stubs).
`io::Result` trait methods stub as `Err(io::Error::other(PgError::unimplemented("FLT-xx", ..)))`.

## Non-goals / known gaps

- Single crash per scenario (multi-crash chains: run `run_scenario` on a recovered image — future FLT lease).
- No modelling of misdirected writes or bit rot on *synced* data (doctor tests flip bytes directly).
- No real-filesystem power-cut testing (dm-flakey / VM) — that is a later campaign tool, not this crate.
- Process-level locking between real OS processes is tested in pgarbor-storage (tests/sto-21.rs), not here.
