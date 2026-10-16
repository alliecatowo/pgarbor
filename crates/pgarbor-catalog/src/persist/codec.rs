//! Catalog row values and whole-catalog load/store. Fully implemented, FROZEN (on-disk v1).
//!
//! Value format of every space-1 row: `[FORMAT_V1 = 0x01]` followed by the `serde_json`
//! serialization of the model struct (`Namespace`, `Relation`, ...; a description value is a JSON
//! string). Unknown leading byte → XX001 `unsupported catalog row format version N` (fail closed).
//! Undecodable JSON → XX001 `corrupt catalog row <key hex>: <serde error>`.
//! Evolution rule: new struct fields must be `#[serde(default)]`; renames/removals need a new
//! format version byte plus a migration (amendment).
use super::keys::{self, CatalogKey};
use crate::data::CatalogData;
use crate::model::*;
use pgarbor_common::{KvRead, KvWrite, PgError, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Current row format version byte.
pub const FORMAT_V1: u8 = 1;

/// Encode a value as `[FORMAT_V1] + json`.
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut out = vec![FORMAT_V1];
    serde_json::to_writer(&mut out, v).map_err(|e| PgError::internal(format!("cannot encode catalog row: {e}")))?;
    Ok(out)
}

/// Decode a value written by [`encode`]. `key` is used only for error messages.
pub fn decode<T: DeserializeOwned>(key: &[u8], bytes: &[u8]) -> Result<T> {
    match bytes.first() {
        Some(&FORMAT_V1) => serde_json::from_slice(&bytes[1..]).map_err(|e| {
            PgError::corruption(format!(
                "corrupt catalog row {}: {e}",
                key.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ))
        }),
        Some(v) => Err(PgError::corruption(format!("unsupported catalog row format version {v}"))),
        None => Err(PgError::corruption("empty catalog row")),
    }
}

/// Encode an object's value bytes.
pub fn encode_object(obj: &CatalogObject) -> Result<Vec<u8>> {
    match obj {
        CatalogObject::Namespace(o) => encode(o),
        CatalogObject::Relation(o) => encode(o),
        CatalogObject::Index(o) => encode(o),
        CatalogObject::Sequence(o) => encode(o),
        CatalogObject::Type(o) => encode(o),
        CatalogObject::Constraint(o) => encode(o),
        CatalogObject::Function(o) => encode(o),
        CatalogObject::Role(o) => encode(o),
        CatalogObject::RoleMembership(o) => encode(o),
        CatalogObject::Database(o) => encode(o),
        CatalogObject::Ext(o) => encode(o),
    }
}

/// Decode an object of `kind` from its value bytes.
pub fn decode_object(kind: ObjKind, key: &[u8], bytes: &[u8]) -> Result<CatalogObject> {
    Ok(match kind {
        ObjKind::Namespace => CatalogObject::Namespace(decode(key, bytes)?),
        ObjKind::Relation => CatalogObject::Relation(decode(key, bytes)?),
        ObjKind::Index => CatalogObject::Index(decode(key, bytes)?),
        ObjKind::Sequence => CatalogObject::Sequence(decode(key, bytes)?),
        ObjKind::Type => CatalogObject::Type(decode(key, bytes)?),
        ObjKind::Constraint => CatalogObject::Constraint(decode(key, bytes)?),
        ObjKind::Function => CatalogObject::Function(decode(key, bytes)?),
        ObjKind::Role => CatalogObject::Role(decode(key, bytes)?),
        ObjKind::RoleMembership => CatalogObject::RoleMembership(decode(key, bytes)?),
        ObjKind::Database => CatalogObject::Database(decode(key, bytes)?),
        ObjKind::Ext => CatalogObject::Ext(decode(key, bytes)?),
        ObjKind::Description => return Err(PgError::internal("description rows are not objects")),
    })
}

/// Write (insert or replace) one object row.
pub fn put_object(kv: &mut dyn KvWrite, obj: &CatalogObject) -> Result<()> {
    kv.put(&keys::object_key(obj.kind(), obj.oid()), &encode_object(obj)?)
}

/// Delete one object row; returns whether it existed.
pub fn delete_object(kv: &mut dyn KvWrite, kind: ObjKind, oid: pgarbor_common::Oid) -> Result<bool> {
    kv.delete(&keys::object_key(kind, oid))
}

/// Write a description (COMMENT ON). Callers delete instead of writing an empty comment
/// (PG: `COMMENT ON ... IS NULL` or `IS ''` removes it).
pub fn put_description(kv: &mut dyn KvWrite, key: &DescriptionKey, text: &str) -> Result<()> {
    kv.put(&keys::description_key(key), &encode(&text)?)
}

/// Delete a description row.
pub fn delete_description(kv: &mut dyn KvWrite, key: &DescriptionKey) -> Result<bool> {
    kv.delete(&keys::description_key(key))
}

/// Load every persisted catalog row of the branch state visible through `kv` (one prefix scan
/// of space 1). Rows are validated: key/value kind mismatch (value OID ≠ key OID) → XX001
/// `catalog row key/oid mismatch for <kind> <oid>`.
pub fn load_data(kv: &dyn KvRead) -> Result<CatalogData> {
    let mut data = CatalogData::default();
    let prefix = keys::catalog_prefix();
    for item in kv.scan_prefix(&prefix)? {
        let (k, v) = item?;
        match keys::decode_key(&k)? {
            CatalogKey::Object(kind, oid) => {
                let obj = decode_object(kind, &k, &v)?;
                if obj.oid() != oid {
                    return Err(PgError::corruption(format!("catalog row key/oid mismatch for {kind:?} {oid}")));
                }
                data.insert(obj);
            }
            CatalogKey::Description(d) => {
                let text: String = decode(&k, &v)?;
                data.descriptions.insert(d, text);
            }
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::MemKv;
    #[test]
    fn store_and_load() {
        let mut kv = MemKv::default();
        let ns = Namespace { oid: 16384, name: "s".into(), owner: 10, acl: None };
        put_object(&mut kv, &CatalogObject::Namespace(ns.clone())).unwrap();
        put_description(&mut kv, &DescriptionKey { objoid: 16384, classoid: 2615, objsubid: 0 }, "hi").unwrap();
        let d = load_data(&kv).unwrap();
        assert_eq!(d.namespaces.get(&16384), Some(&ns));
        assert_eq!(d.descriptions.len(), 1);
        assert!(decode::<String>(b"k", &[9, b'"', b'"']).is_err());
    }
}
