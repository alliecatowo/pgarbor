# pgarbor-core-03: reused locks still report committed changes

Status: ACCEPTED (2026-10-15, lead engineering safety review).

Decision: CORE-04's held-key cache skips lock-manager acquisition only. Every
successful grant or reuse compares the latest committed head against the
unchanged statement snapshot, with the normal branch-generation guard.
Repeated probes continue returning `Changed(latest)` until a later statement
refreshes the RC snapshot. The overlay does not affect that comparison; the
transaction KV view remains authoritative for its own writes. RR write probes
continue returning 40001 even when the lock is already held.

This corrects the module's earlier fast-path promise of unconditional Unchanged
and implements the crate invariant that outcomes use a fresh committed head.
No API change. Regression coverage includes concurrent insert/update/delete,
repeated reuse, upgrades, unchanged statement snapshots, own overlays and RR
serialization checking.
