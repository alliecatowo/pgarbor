//! The container: one `.pga` file with dual superblocks, a single writer and concurrent snapshot
//! readers (STORE-09..13, STORE-18).
pub mod handle;
pub mod options;
pub mod pins;
pub mod recovery;
pub mod snapshot;
pub mod tree_writer;
pub mod write;

pub use handle::Container;
pub use options::{
    CompactOptions, CompactReport, CreateOptions, OpenOptions, RecoveryPolicy, RecoveryReport, SyncMode,
    VerifyLevel,
};
pub use pins::{PinGuard, PinTable};
pub use recovery::{read_slots, COMPACT_TEMP_SUFFIX, EXPORT_TEMP_SUFFIX};
pub use snapshot::{Snapshot, TreeSnapshot};
pub use tree_writer::TreeWriter;
pub use write::WriteTxn;
