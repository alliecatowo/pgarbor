//! Core identifiers and handles (ARCH-04).
use serde::{Deserialize, Serialize};
use std::fmt;

/// Page number inside a `.pga` file. `PageId(0)` means "no page" (null pointer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PageId(pub u64);

impl PageId {
    pub const NONE: PageId = PageId(0);
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// Monotonic durable commit counter of a container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommitSeq(pub u64);

/// In-process transaction id. Never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TxnId(pub u64);

/// PostgreSQL object identifier.
pub type Oid = u32;
/// Relation OID.
pub type RelId = Oid;
/// First OID assigned to user-created objects (PostgreSQL's FirstNormalObjectId).
pub const FIRST_NORMAL_OID: Oid = 16384;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
        pub struct $name(pub [u8; 16]);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
            /// Canonical 8-4-4-4-12 lowercase hex form.
            pub fn to_hyphenated(&self) -> String {
                let h: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
                format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
            }
            /// Parse the hyphenated (or bare 32-hex) form.
            pub fn parse(s: &str) -> Option<Self> {
                let hex: String = s.chars().filter(|c| *c != '-').collect();
                if hex.len() != 32 {
                    return None;
                }
                let mut out = [0u8; 16];
                for (i, chunk) in out.iter_mut().enumerate() {
                    *chunk = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
                }
                Some($name(out))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hyphenated())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hyphenated())
            }
        }
    };
}

uuid_id!(BranchId);
uuid_id!(DatabaseUuid);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uuid_roundtrip() {
        let id = BranchId([0xab; 16]);
        let s = id.to_hyphenated();
        assert_eq!(s, "abababab-abab-abab-abab-abababababab");
        assert_eq!(BranchId::parse(&s), Some(id));
        assert_eq!(BranchId::parse("zz"), None);
    }
}
