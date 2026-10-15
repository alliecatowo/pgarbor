//! Roots embedded in directory values. CONTRACT. Fully implemented.
//!
//! The superblock points at ONE tree, the *directory* (owned by pgarbor-branches: keys `B..`,
//! `N..`, `M..` per ARCHITECTURE §6). Directory values (e.g. `BranchRecord`) embed other tree roots
//! (`data_root`, named snapshots). Storage cannot parse those values, but reachability tracing,
//! compaction and export must find and rewrite them. The owner of the directory format supplies a
//! [`RootResolver`]; storage calls it for every directory entry. See amendment
//! `contracts/amendments/pgarbor-storage-01-root-resolver.md`.
use pgarbor_common::{PageId, Result};

// CONTRACT
/// A tree root with a human-readable label (for space accounting and doctor output), e.g.
/// `"branch main"`, `"branch main snapshot before-migration"`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NamedRoot {
    pub label: String,
    pub root: PageId,
}

// CONTRACT
/// Interprets directory values. Implementations must be pure functions of `(key, value)`.
pub trait RootResolver: Send + Sync {
    /// Append every tree root referenced by this directory entry to `out` (skip `PageId::NONE`).
    /// A value it cannot parse → return a corruption error (XX001), never guess.
    fn roots(&self, key: &[u8], value: &[u8], out: &mut Vec<NamedRoot>) -> Result<()>;

    /// Return the value with every embedded root `r` replaced by `map(r)` (compaction/export
    /// renumber pages), or `None` if the entry embeds no roots (value is copied unchanged).
    /// `map(PageId::NONE)` is never called; NONE stays NONE.
    fn remap(
        &self,
        key: &[u8],
        value: &[u8],
        map: &mut dyn FnMut(PageId) -> Result<PageId>,
    ) -> Result<Option<Vec<u8>>>;
}

// CONTRACT
/// Directory values embed no roots (only the directory tree itself is live).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoNestedRoots;

impl RootResolver for NoNestedRoots {
    fn roots(&self, _key: &[u8], _value: &[u8], _out: &mut Vec<NamedRoot>) -> Result<()> {
        Ok(())
    }
    fn remap(
        &self,
        _key: &[u8],
        _value: &[u8],
        _map: &mut dyn FnMut(PageId) -> Result<PageId>,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

// CONTRACT
/// Simple convention used by storage/fault tests: directory keys starting with `prefix` have an
/// 8-byte little-endian PageId as their value; label = `String::from_utf8_lossy(key[prefix..])`.
/// Other keys embed nothing.
#[derive(Clone, Debug, Default)]
pub struct PrefixU64Roots {
    pub prefix: Vec<u8>,
}

impl PrefixU64Roots {
    pub fn new(prefix: &[u8]) -> PrefixU64Roots {
        PrefixU64Roots { prefix: prefix.to_vec() }
    }
    fn decode(value: &[u8]) -> Result<PageId> {
        let b: [u8; 8] = value
            .try_into()
            .map_err(|_| crate::error::corrupt(format!("root reference has {} bytes, expected 8", value.len())))?;
        Ok(PageId(u64::from_le_bytes(b)))
    }
}

impl RootResolver for PrefixU64Roots {
    fn roots(&self, key: &[u8], value: &[u8], out: &mut Vec<NamedRoot>) -> Result<()> {
        if let Some(rest) = key.strip_prefix(self.prefix.as_slice()) {
            let root = Self::decode(value)?;
            if !root.is_none() {
                out.push(NamedRoot { label: String::from_utf8_lossy(rest).into_owned(), root });
            }
        }
        Ok(())
    }
    fn remap(
        &self,
        key: &[u8],
        value: &[u8],
        map: &mut dyn FnMut(PageId) -> Result<PageId>,
    ) -> Result<Option<Vec<u8>>> {
        if !key.starts_with(&self.prefix) {
            return Ok(None);
        }
        let root = Self::decode(value)?;
        let new = if root.is_none() { root } else { map(root)? };
        Ok(Some(new.0.to_le_bytes().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_roots() {
        let r = PrefixU64Roots::new(b"R");
        let mut out = vec![];
        r.roots(b"Rmain", &7u64.to_le_bytes(), &mut out).unwrap();
        r.roots(b"Xother", b"junk", &mut out).unwrap();
        assert_eq!(out, vec![NamedRoot { label: "main".into(), root: PageId(7) }]);
        let v = r.remap(b"Rmain", &7u64.to_le_bytes(), &mut |p| Ok(PageId(p.0 + 1))).unwrap();
        assert_eq!(v, Some(8u64.to_le_bytes().to_vec()));
        assert!(r.roots(b"Rbad", b"123", &mut out).is_err());
    }
}
