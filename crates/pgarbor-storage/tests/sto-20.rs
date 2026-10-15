//! STO-20: model-based property tests of the CoW B+tree against `std::collections::BTreeMap`.
//!
//! Lease STO-20 writes this file. Required tests (proptest 1.12, `ProptestConfig::with_cases(64)`
//! default, deterministic seeds via `PROPTEST_RNG_SEED` honoured by proptest):
//! 1. `random_ops_match_btreemap`: over a `MemVfs` container (page_size 4096 to force splits),
//!    apply 1..2000 random put/delete ops (keys 0..=40 bytes from a small alphabet so collisions
//!    happen; values 0..=6000 bytes so overflow chains occur) inside ONE WriteTxn, then commit;
//!    after every 100 ops and at the end compare full `scan(Unbounded, Unbounded)` and random
//!    `get`s with the model. Then reopen and compare again.
//! 2. `snapshots_are_immutable`: take `TreeSnapshot`s after each of several commits; later
//!    commits (including deletes of everything) never change what old snapshots return.
//! 3. `range_bounds`: every combination of Included/Excluded/Unbounded bounds equals
//!    `BTreeMap::range` (and empty/inverted ranges yield nothing without panicking).
//! 4. `sequential_append_fill`: inserting 100k ascending 16-byte keys yields leaves that are >= 90%
//!    full on average (append split heuristic), checked through `space_report`.
//! 5. `delete_all_returns_null_root`: deleting every key returns `PageId::NONE`.
//! 6. `doctor_clean`: `Container::verify(.., VerifyLevel::Full)` reports no Error issues after
//!    each scenario above.
