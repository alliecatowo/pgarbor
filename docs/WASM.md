# WebAssembly build

`crates/pgarbor-wasm` builds the full engine for `wasm32-unknown-unknown` with an in-memory database
(`MemVfs`, `SyncMode::Off`) behind `exec(sql) -> JSON`. Verified: Node loads it and runs SELECT, CREATE,
INSERT, a duplicate-key error (23505) (`node demo/test-node.mjs`), and headless Chrome renders the demo page
(`demo/web/index.html?run`).

## Changes that made it compile and run

- `pgarbor-storage`: `fs2` is a non-wasm dependency; the `os` module (`OsFile`/`OsVfs`) and `Container::create/open`
  (real-FS convenience wrappers) are `cfg(not(wasm32))`. `Container::create_with/open_with` take any `Vfs`.
- `pgarbor-core` `Engine::vfs_of`: default VFS is `MemVfs` on wasm32 (`OsVfs` elsewhere).
- `pgarbor-branches`: `uuid` gets the `js` feature on wasm32 (randomness for v4 ids).
- `pgarbor-types` `scalar/float_ops.rs`: `erf/erfc/tgamma/lgamma` use the pure-Rust `libm` crate on wasm32
  (native builds still link the platform libm). Results may differ from glibc in the last ulp on wasm.
- `pgarbor-common::time`: `Instant`, `SystemTime`, `UNIX_EPOCH` and `process_id()` are re-exports of `std` natively
  and shims over `Date.now()` / `performance.now()` on wasm32 (std's clocks and `process::id` panic there). All
  non-test engine code uses this module.

## Limits of the wasm build

- Memory only: no `.pga` file persistence in the browser (no IndexedDB/OPFS VFS yet).
- Single thread, no blocking: any code path that would wait on a `Condvar` (lock contention between sessions)
  would panic; one session cannot contend with itself, so the demo does not hit it. `statement_timeout`/lock
  timeouts rely on the shimmed clock only.
- `reflink`/branch file operations (`pgarbor-branches` `reflink.rs`, `libc`) are native-only paths and are not
  used by the in-memory database; branch file export from wasm is untested.
- The `.wasm` is built unoptimised-for-size (about 13 MB); `wasm-opt` was not available and was not run.
- The oracle test corpus is native-only; wasm numerics have not been run against it.
