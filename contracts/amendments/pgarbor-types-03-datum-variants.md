# Amendment request pgarbor-types-03: additional `Datum` variants for the remaining PG18 base types

Status: DEFERRED (2026-10-15, lead engineering review).

## Problem

ARCHITECTURE §8 freezes the `Datum` enum. The following PG18 builtin types have no faithful representation:
- `inet`/`cidr` (pg_stat_activity.client_addr, ORMs that map IP columns), `macaddr`/`macaddr8`
- `bit`/`varbit` (bit-length semantics), `money`, which can live in `Int8` without an amendment
- geometric types (`point`, `line`, `lseg`, `box`, `path`, `polygon`, `circle`)
- range and multirange types (`int4range`, `tstzrange`, ...), which Prisma and Drizzle use for exclusion
  constraints
- `tsvector`/`tsquery`, `xml`, `jsonpath`, `pg_snapshot`/`txid_snapshot`, `tid`

## Proposed change (v2 contract epoch)

Add these variants:

```rust
Inet { addr: std::net::IpAddr, bits: u8, is_cidr: bool },
MacAddr([u8; 8], bool /*is_8*/),
Bits { len: u32, bytes: Vec<u8> },
Range(Box<Range>),   // Range { elem: Oid, lower: Bound<Datum>, upper: Bound<Datum>, empty: bool }
MultiRange(Vec<Range>),
Geometric(Box<Geo>),
Tid(u32 /*block*/, u16 /*offset*/),
```

`jsonpath`, `xml` and `tsvector` would get dedicated payload types in later epochs. Every match on `Datum`
(tuple, keycodec, compare, io) must add arms, so this is a contract epoch bump, not a patch.

## Affected crates

pgarbor-types (all value modules), pgarbor-exec and pgarbor-wire (exhaustive matches), pgarbor-catalog
(virtual tables exposing inet/tid columns).


## Steward decision (2026-10-15)

Defer new Datum payloads to a coordinated encoding/type/executor/wire epoch. Unsupported families continue to return their existing leased 0A000; no textual or numeric placeholder is approved as faithful support. Money/aclitem handling that already fits current variants is unaffected.
