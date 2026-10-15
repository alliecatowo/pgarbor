//! Byte I/O layer: [`FileIo`] is the ONLY path to file bytes (ARCHITECTURE §5); [`Vfs`] is the only
//! path to directory operations (create/rename/remove/dir-fsync). Real files use [`OsFile`]/[`OsVfs`],
//! tests and wasm use [`MemFile`]/[`MemVfs`], fault injection uses `pgarbor_fault::FaultVfs`.
pub mod file_io;
pub mod mem;
#[cfg(not(target_arch = "wasm32"))]
pub mod os;
pub mod vfs;

pub use file_io::{FileIo, LockMode};
pub use mem::{MemFile, MemVfs};
#[cfg(not(target_arch = "wasm32"))]
pub use os::{OsFile, OsVfs};
pub use vfs::{parent_dir, sibling_path, OpenMode, Vfs};
