//! Immutable catalog snapshots: accessors ([`core`]) and construction ([`build`], CAT-02).
pub mod build;
pub mod core;

pub use self::build::parse_acl_text;
pub use self::core::{CatalogSnapshot, ClassRef, SnapshotIndexes, SnapshotInner, UserTypeRef};
