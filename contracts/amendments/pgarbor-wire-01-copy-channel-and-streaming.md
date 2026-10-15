Status: DEFERRED (lead-engineer decision, 2026-10-15).
Crate: pgarbor-wire
Affects: pgarbor (embedded API), pgarbor-exec (copy.rs), pgarbor-wire (copy.rs, simple_query.rs)
Leases: WIR-13 (copy), WIR-11 (simple_query), API-07/API-12 (pgarbor)

# pgarbor-wire-01: COPY plumbing the wire layer cannot express with the frozen API

## Problem
1. `Session::set_copy_channel` takes `Box<dyn pgarbor_exec::copy::CopyChannel>`, but `pgarbor-wire`
   and `pgarbor-cli` do not depend on `pgarbor-exec` and `pgarbor`'s lib.rs does not re-export the
   trait. The wire layer cannot name the type it must implement.
2. `CopyChannel` has only `read_chunk` / `write_chunk`. The protocol needs CopyInResponse /
   CopyOutResponse (overall format 0/1, column count, per-column formats) BEFORE the first data
   message. The channel cannot know them.
3. `Session::simple_query_raw` returns only after the WHOLE query string ran. COPY data streams
   through the channel DURING execution, so for `SELECT 1; COPY t TO STDOUT` the CopyOutResponse and
   CopyData would be written before the rows of `SELECT 1`. Only a single-statement COPY is correct.

## Proposed change (minimum)
a. `pgarbor/src/lib.rs`: `pub use pgarbor_exec::copy::{CopyChannel, CopyDirection, MemChannel};`.
b. `pgarbor_exec::copy::CopyChannel` gains a defaulted method
   `fn start(&mut self, direction: CopyDirection, binary: bool, column_count: usize) -> Result<()> { Ok(()) }`
   (`CopyDirection::{In, Out}`), called once by `execute_copy` before the first read/write.
c. `Session::simple_query_streaming(&mut self, sql: &str, sink: &mut dyn FnMut(StreamedItem))` where
   `StreamedItem::{Statement(StatementOutput), Error{..}}` is invoked after each statement completes
   (so the wire layer can emit its messages in order, before the next statement's COPY starts), with
   the final `TxnStatus` returned. `simple_query_raw` stays as the collecting convenience.

## Design-around until this lands
`WireCopyChannel` (wire `copy.rs`) sends CopyInResponse / CopyOutResponse lazily with overall format
text (0) and zero column formats on the first `read_chunk` / `write_chunk` call. Its lease body stays
`PgError::unimplemented("WIR-13", ..)` until (a) lands because the type cannot be named. Single
statement COPY works with the collecting `simple_query_raw`; multi-statement strings containing COPY
are documented as a known gap (`contracts/crates/pgarbor-wire.md`).

## Steward decision (2026-10-15)

The complete ordered COPY streaming contract is not installed. Keep WIR-13 leased 0A000; do not install a zero-column lazy response or acknowledge COPY bytes without executing them. API reexports alone would not establish ordering. CLI COPY import/export remains unsupported.
