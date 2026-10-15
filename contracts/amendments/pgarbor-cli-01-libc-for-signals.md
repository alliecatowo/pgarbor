Status: PROPOSED
Crate: pgarbor-cli
Affects: pgarbor-cli (Cargo.toml)
Leases: CLI-07 (serve), CLI-22 (branch exec)

# pgarbor-cli-01: SIGTERM/SIGINT handling without the `libc` crate

## Problem
harness-02 requires `pgarbor serve` to exit 0 within 10 s of SIGTERM after closing the container
cleanly. Rust std has no signal API and `pgarbor-cli/Cargo.toml` has no `libc`.

## Design-around (implemented in the skeleton)
`signals.rs` declares `unsafe extern "C" { fn signal(signum: i32, handler: usize) -> usize; }`
(std links libc already) and installs a handler that only stores to a static `AtomicBool`. glibc
`signal()` has BSD semantics (SA_RESTART), so a blocking `accept()` never returns EINTR: the wire
listener uses a non-blocking accept with a ~50 ms poll of the stop flag, and the CLI's main thread
polls `signals::shutdown_requested()` every 50 ms.

## Proposed change
Optional hardening: add `libc = { workspace = true }` to `crates/pgarbor-cli/Cargo.toml` and replace
the hand-written declaration with `libc::sigaction` (SA_RESTART off, SA_RESETHAND off). Not
required for correctness of the v1 design.
