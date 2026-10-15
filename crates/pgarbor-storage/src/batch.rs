//! `WriteBatch`: an ordered set of pending puts/deletes (ARCHITECTURE §7 overlay shape).
//! CONTRACT. Fully implemented.
use std::collections::BTreeMap;
use std::ops::Bound;

// CONTRACT
/// Ordered pending mutations; later operations on a key replace earlier ones. `None` = delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBatch {
    ops: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl WriteBatch {
    pub fn new() -> WriteBatch {
        WriteBatch::default()
    }
    pub fn from_map(ops: BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> WriteBatch {
        WriteBatch { ops }
    }
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.ops.insert(key.to_vec(), Some(value.to_vec()));
    }
    pub fn delete(&mut self, key: &[u8]) {
        self.ops.insert(key.to_vec(), None);
    }
    /// `None` = key not in batch; `Some(None)` = deleted in batch; `Some(Some(v))` = put.
    pub fn get(&self, key: &[u8]) -> Option<Option<&[u8]>> {
        self.ops.get(key).map(|v| v.as_deref())
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
    pub fn has_deletes(&self) -> bool {
        self.ops.values().any(|v| v.is_none())
    }
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], Option<&[u8]>)> {
        self.ops.iter().map(|(k, v)| (k.as_slice(), v.as_deref()))
    }
    pub fn range<'a>(
        &'a self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + 'a {
        let lower = lower.map(|b| b.to_vec());
        let upper = upper.map(|b| b.to_vec());
        let empty = match (&lower, &upper) {
            (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) => {
                a > b || (a == b && (matches!(lower, Bound::Excluded(_)) || matches!(upper, Bound::Excluded(_))))
            }
            _ => false,
        };
        let it = if empty { None } else { Some(self.ops.range::<Vec<u8>, _>((lower, upper))) };
        it.into_iter().flatten().map(|(k, v)| (k.as_slice(), v.as_deref()))
    }
    pub fn clear(&mut self) {
        self.ops.clear()
    }
    pub fn as_map(&self) -> &BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        &self.ops
    }
    pub fn into_map(self) -> BTreeMap<Vec<u8>, Option<Vec<u8>>> {
        self.ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_write_wins_and_empty_range_is_safe() {
        let mut b = WriteBatch::new();
        b.put(b"a", b"1");
        b.delete(b"a");
        b.put(b"b", b"2");
        assert_eq!(b.get(b"a"), Some(None));
        assert_eq!(b.range(Bound::Included(b"z"), Bound::Included(b"a")).count(), 0);
        assert_eq!(b.range(Bound::Included(b"a"), Bound::Excluded(b"a")).count(), 0);
        assert_eq!(b.range(Bound::Unbounded, Bound::Unbounded).count(), 2);
    }
}
