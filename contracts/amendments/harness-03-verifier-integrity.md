# Harness verifier integrity

Status: ACCEPTED (2026-10-15, lead-engineer review remediation).

Decision: amend harness §6: default user_oids canonicalizes descriptor identities only.
OID-typed row values have no allocated-object provenance in case/1, so literal and stored
OID data must be compared exactly. Allocation-dependent row values will conservatively
mismatch until a separately reviewed provenance schema exists. Existing recorded U<n>
row values require the pinned PG18 reviewed recorder to rerecord; this patch changes no
expected files. Descriptor canonicalization still cannot prove semantic identity of user
types from names alone; that remains a limitation, not a claim of correctness.

Amend §5/§9: every public recorder write entry enforces the review gate and verifies live
oracle identity. Only private corpus workers reuse that verified identity. Any setup SQL
error, missing setup observation or missing setup results invalidates recording; expected
errors belong in explicit steps. Dry runs remain scratch-only with live verification.

Amend §7: execution evidence requires fresh result artifacts and successful psql execution;
launcher failure, connection loss and absent output cannot PASS even if an old artifact
matches. Official and differential verdicts remain independent: missing PG baseline blocks
only differential comparison. SLT needs at least one executed record to PASS; empty or
unknown isolation selections are errors.

Amend §9.5 inventory scope: constructor examples inside cfg(test) Rust modules do not
represent production unsupported features. Inventory excludes those test modules, retaining
production markers and existing forbidden-code checks. Injected Python fault tests use
RuntimeError instead of unsupported-feature placeholders.

Client campaigns discover tests/apps/*/run.sh, provision each independently, set DATABASE_URL,
retain process output and require exactly one valid FIXTURE_RESULT with positive consistent
step totals. A successful process exit without substantive checks cannot PASS. Process exit
and failed-check counters independently reject an app run.

Validation: regression tests live in harness/tests/test_*_integrity.py and
harness/tests/test_client_fixture_results.py; offline selftest includes an OID-value mutant.

Legacy references containing U<n> OID row placeholders are BLOCKED with
reference_normalization_stale before target execution. They cannot be treated as evidence
under the stricter data policy, including when a malformed target repeats the old placeholder.
