# Amendment request pgarbor-catalog-05: catalog space-1 key layout (refines ARCHITECTURE §9)

Status: PROPOSED (architect, pgarbor-catalog). Implemented in `persist::keys` / `persist::codec` (frozen on-disk v1).

## Problem

ARCHITECTURE §9 sketches space 1 as `[0,0,0,1][kind u8][oid u32 BE]` with kinds "namespace, class (table/
index/view/sequence), type (enum/domain/composite), constraint, attrdef, database, role, description".
The implemented model is finer-grained for good reasons and the layout of description rows needs more key
bytes, so the byte layout differs from the sketch.

## Proposed change (as implemented)

```
object:       [00 00 00 01] [kind u8] [oid u32 BE]                                                   (9 bytes)
description:  [00 00 00 01] [b'D']    [classoid u32 BE] [objoid u32 BE] [objsubid u32 BE ^ 0x8000_0000]  (17 bytes)
```
Kinds: `n` namespace, `r` relation (table, partitioned table, view, matview, composite type), `i` index,
`S` sequence, `t` type (enum/domain only), `c` constraint, `f` function, `a` role, `m` role membership,
`d` database, `D` description. Differences from the sketch:
- class is split into `r`/`i`/`S` so scans by kind are prefix scans; composite types are relations (a
  composite is a pg_class row in PG) whose rowtype/array type are derived;
- there is no `attrdef` kind: a column default is embedded in `Column::default` (its `oid` is the pg_attrdef
  OID, kept in the same OID space);
- added `f` (user functions), `m` (role memberships); roles/databases/descriptions are cluster-wide in PG
  but stored per branch here;
- description keys carry `(classoid, objoid, objsubid)` with a sign-flipped sub-id so keys sort numerically.
Values are `[0x01]` + `serde_json` (versioned, fail-closed on unknown version bytes; evolution rule in
`persist::codec`).

## Affected crates

ARCHITECTURE §9 wording only (no other crate reads space 1).
