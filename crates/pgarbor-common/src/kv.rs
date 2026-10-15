//! Ordered key/value contract between the transactional core and SQL layers (ARCH-06).
use crate::error::Result;
use std::collections::BTreeMap;
use std::ops::Bound;

/// Boxed ascending iterator of `(key, value)` pairs.
pub type KvIter<'a> = Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>;

/// Read access to an ordered byte keyspace (bytewise key order).
pub trait KvRead {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// Ascending iteration over keys within the bounds.
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>>;

    /// Iterate all keys starting with `prefix`.
    fn scan_prefix<'a>(&'a self, prefix: &[u8]) -> Result<KvIter<'a>> {
        match prefix_upper_bound(prefix) {
            Some(end) => {
                let it = self.scan(Bound::Included(prefix), Bound::Excluded(&end))?;
                // `end` is dropped after scan returns; implementations must not borrow bounds.
                Ok(it)
            }
            None => self.scan(Bound::Included(prefix), Bound::Unbounded),
        }
    }
}

/// Write access. Implementations MUST make writes visible to subsequent reads through the same handle.
pub trait KvWrite: KvRead {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    /// Returns true when a key was present.
    fn delete(&mut self, key: &[u8]) -> Result<bool>;
}

/// Smallest key strictly greater than every key having `prefix` as prefix, or None if unbounded.
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// Keyspace prefixes of a branch data tree (see contracts/ARCHITECTURE.md §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceId(pub u32);

impl SpaceId {
    pub const CATALOG: SpaceId = SpaceId(1);
    pub const SEQUENCES: SpaceId = SpaceId(2);
    pub const BRANCH_META: SpaceId = SpaceId(3);

    pub fn prefix(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
    /// Key prefix for a relation's data (`relid >= FIRST_NORMAL_OID`).
    pub fn relation(relid: u32) -> SpaceId {
        SpaceId(relid)
    }
}

/// Simple in-memory KV used by tests and by layers that need a scratch keyspace.
#[derive(Clone, Debug, Default)]
pub struct MemKv {
    pub map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl KvRead for MemKv {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }
    fn scan<'a>(&'a self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<KvIter<'a>> {
        let lower = lower.map(|b| b.to_vec());
        let upper = upper.map(|b| b.to_vec());
        Ok(Box::new(
            self.map
                .range::<Vec<u8>, _>((lower, upper))
                .map(|(k, v)| Ok((k.clone(), v.clone()))),
        ))
    }
}

impl KvWrite for MemKv {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn delete(&mut self, key: &[u8]) -> Result<bool> {
        Ok(self.map.remove(key).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_scan() {
        let mut kv = MemKv::default();
        kv.put(b"a1", b"x").unwrap();
        kv.put(b"a2", b"y").unwrap();
        kv.put(b"b1", b"z").unwrap();
        let keys: Vec<_> = kv.scan_prefix(b"a").unwrap().map(|r| r.unwrap().0).collect();
        assert_eq!(keys, vec![b"a1".to_vec(), b"a2".to_vec()]);
        assert_eq!(prefix_upper_bound(&[0x01, 0xff]), Some(vec![0x02]));
        assert_eq!(prefix_upper_bound(&[0xff]), None);
    }
}
