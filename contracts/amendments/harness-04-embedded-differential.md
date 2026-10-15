# Socket-free embedded differential transport

Status: ACCEPTED (lead-engineer decision, 2026-10-15).

`runner/embedded.py` runs the recorded corpus through the `embedded_case` Rust example,
one process and in-memory database per case. It uses the existing Transcript model,
normalizer, comparator and classifier unchanged. Text output is encoded by Session,
not by Rust debug strings. Missing/stale/legacy-normalized references are blocked.
Async, protocol and restart steps are blocked by transport capability; unsupported SQL
is executed and retains its real engine error. No expected files are changed.

This is embedded SQL evidence only. It cannot substitute for wire, psql, ORM, durable
file crash recovery or native-runner evidence. Output records the target binary hash,
fixed selected denominator, execution count, outcome, transcript and family totals.
A process timeout is a HANG; abort is CRASH; absent complete transcript is HARNESS_ERROR.
The existing classifier also calls unexpected XX000 results CRASH; those must be
reported separately from process aborts. No live oracle identity is claimed.

Default three/five-second campaign timeouts can classify slow valid work as HANG;
retain and rerun those cases with the ordinary campaign timeout before calling them
engine hangs. This campaign changes no frozen Target or model signatures.
