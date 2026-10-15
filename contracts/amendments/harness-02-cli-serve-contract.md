Status: ACCEPTED (2026-10-15, lead-engineer assembly review)
Crate: harness
Affects: pgarbor-cli
Leases: HAR-11 (PgarborTarget), FAC-09 (campaign); pgarbor-cli leases

# harness-02: `pgarbor` CLI process contract the harness depends on

## Problem
The differential runner, regress, isolation and wire families start PGArbor as a server
process per case: one fresh `.pga` per case, on a loopback port. The harness needs a stable,
non-interactive command line. `crates/pgarbor-cli` is owned by another architect, so this
request is filed instead of guessing silently. The defaults are encoded in
`harness/pgarbor_harness/runner/targets.py::PgarborCommand`, which can be overridden.

## Proposed change (minimum surface)
1. `pgarbor create <file>`
   - Creates a new container with branch `main` and the built-in `postgres` database/role.
   - Exits 0 on success.
   - If the file already exists, exits non-zero with a message on stderr.
2. `pgarbor serve <file> --host <ip> --port <n> [--branch <name>]`
   - Serves PG wire protocol 3.0 on host:port with trust auth on loopback.
   - The branch defaults to `main`.
   - Startup and shutdown:
     - Stays in the foreground.
     - After it starts accepting, prints exactly one line to stdout:
       `pgarbor: listening on <host>:<port>`.
     - `--port 0` binds an ephemeral port and prints the real one.
     - On SIGTERM: stops accepting, lets in-flight statements end or terminates them, closes
       the container cleanly, and exits 0 within 10 s.
   - Failures:
     - If the port is already in use, exits 2 with stderr containing `address in use`.
     - On corruption detected at open, exits 3 with stderr containing `corrupt`. The harness
       classifies this as CORRUPTION.
   - Rust panics must print the standard `panicked at` line to stderr (the default hook). The
     harness classifies such cases as CRASH.
3. Session behavior on the wire:
   - Honors the startup `options` parameter (`-c name=value`) for at least `TimeZone`,
     `DateStyle`, `IntervalStyle`, `extra_float_digits`, `client_encoding` and `lc_messages`.
   - Reports `server_version` in ParameterStatus as a PG18-compatible string (exact value to
     be decided by the wire contract; the harness compares it).
4. The function `pg_catalog.pg_isolation_test_session_is_blocked(int4, int4[])` would let the
   official isolation suite use real wait detection instead of the heuristic fallback, which
   is never scored PASS (see isolation.tester). This is optional and can come later.

## Affected crates
pgarbor-cli (argv, signals, exit codes), pgarbor-wire (options/ParameterStatus),
pgarbor (isolation helper function, optional).

## Steward decision

Accepted. The frozen CLI contract adopts create/serve argv, readiness line and signal semantics. PgarborCommand already matches that grammar and checks TCP plus SELECT 1. Real implementation coverage requires the independent opt-in lifecycle gate in harness/tests/test_real_pgarbor_target.py; adapter wiring alone proves no SQL compatibility.
