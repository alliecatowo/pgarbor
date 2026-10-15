//! Copy-on-write B+tree over byte keys/values ordered bytewise (STORE-05..08).
//!
//! A tree is identified by its root `PageId`; `PageId::NONE` is the empty tree. All mutating
//! functions take a [`crate::PageSink`] and return the NEW root; the old root and every page
//! reachable from it stay valid and unchanged (copy-on-write), so snapshots holding old roots keep
//! reading consistent data. No sibling pointers exist (they are incompatible with path copying);
//! iteration keeps a root-to-leaf stack.
pub mod bulk;
pub mod delete;
pub mod insert;
pub mod node;
pub mod overflow;
pub mod read;

pub use bulk::{apply_batch, build_from_sorted};
pub use delete::{delete, delete_range, DeleteResult, RangeDeleteResult};
pub use insert::{put, PutResult};
pub use node::{max_inline_value, max_key_len};
pub use read::{get, RangeIter};
