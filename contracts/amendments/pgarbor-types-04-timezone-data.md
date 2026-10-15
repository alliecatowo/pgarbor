# Amendment request pgarbor-types-04: IANA time zone rules (tzdata)

Status: PROPOSED (architect, pgarbor-types). v1 supports UTC, fixed offsets, POSIX `UTC+n` strings and the
PG "Default" abbreviation set. IANA region names return 0A000 (`datetime::tz`, lease TYP-25).

## Problem

Clients routinely run `SET TIME ZONE 'America/New_York'`, and JDBC sends `TimeZone` from the JVM default in
its startup packet. `AT TIME ZONE 'Europe/Paris'` and timestamptz output in DST zones need full tz rules,
which the frozen dependency set cannot supply.

## Proposed change

Choose one:
1. Embed a compact build-time snapshot of tzdata, as compiled TZif files or a generated Rust table,
   inside pgarbor-types. Regenerate it with a script from the system zoneinfo or the oracle container's
   `/usr/share/zoneinfo`, keeping the same tzdata version as the oracle.
2. Read the host's `/usr/share/zoneinfo` at runtime through an engine-supplied `TimeZoneProvider` trait
   object in `SessionContext`. This is not wasm-friendly and is non-hermetic.

Recommendation: option 1, with the version pinned to the oracle image's tzdata (`# version 2026c` in
`/usr/share/zoneinfo/tzdata.zi` of the pinned oracle image).

## Affected crates

pgarbor-types (`datetime::tz`), pgarbor (TimeZone GUC validation), pgarbor-wire (startup TimeZone parameter).
