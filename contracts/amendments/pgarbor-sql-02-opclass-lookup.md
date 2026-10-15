# Amendment request pgarbor-sql-02: operator class lookup by name

Status: DEFERRED (lead-engineer decision, 2026-10-15).

## Problem

`CREATE INDEX ... (b text_pattern_ops)` and `CREATE INDEX ... (a int4_ops)` name an operator class. The binder
(`binder/ddl_index.rs`, lease SQL-32) must produce `IndexKeySpec::opclass: Option<Oid>`. The catalog exposes
`ddl::index::default_btree_opclass(type)` (writer side) and the generated `builtin::OPCLASSES` text rows, but no
typed read API by name.

## Proposed change

```rust
impl CatalogEnv {
    /// `[schema.]name` operator class for access method `btree`; Ok(None) when absent.
    pub fn lookup_opclass(&self, schema: Option<&str>, method: &str, name: &str) -> Result<Option<OpClassRef>>;
}
pub struct OpClassRef { pub oid: Oid, pub family: Oid, pub input_type: Oid, pub is_default: bool }
```
Errors for the binder to raise: 42704 `operator class "x" does not exist for access method "btree"`, 42804
`operator class "int4_ops" does not accept data type text`.

## Affected crates

pgarbor-catalog, pgarbor-sql (`ddl_index.rs`).

## Steward decision (2026-10-15)

The assembled binder resolves compiled-in btree opclasses directly in ddl_index.rs. A new catalog public API is unnecessary for this slice and is not promised. Custom/non-btree lookup remains unsupported.
