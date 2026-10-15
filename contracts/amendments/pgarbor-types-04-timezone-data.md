# Amendment request pgarbor-types-04: IANA time zone rules (tzdata)

Status: DEFERRED (2026-10-15, lead engineering review).

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


## Steward decision (2026-10-15)

Defer IANA rule embedding/provider APIs and dependency changes. UTC, fixed offsets and the implemented abbreviation/POSIX subset remain supported; IANA conversions must retain leased TYP-25 0A000. The proposed oracle tzdata version is not verified by this review and is not newly ratified as an available dependency.
