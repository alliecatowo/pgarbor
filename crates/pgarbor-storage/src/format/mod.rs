//! On-disk format: constants, superblock codec, page codec. Byte layouts are CONTRACT
//! (see `contracts/crates/pgarbor-storage.md` §Formats). All integers on disk are little-endian.
pub mod consts;
pub mod page;
pub mod superblock;

pub use consts::*;
pub use page::{decode_page, encode_page, page_checksum, Page, PageHeader, PageRef};
pub use superblock::{choose_slot, random_db_uuid, SlotChoice, SlotStatus, Superblock};
