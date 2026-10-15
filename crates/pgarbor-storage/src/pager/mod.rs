//! Page access: the [`PageSource`]/[`PageSink`] traits the B+tree is written against, the shared
//! [`PageCache`], and the file-backed [`Pager`].
pub mod cache;
#[allow(clippy::module_inception)]
pub mod pager;
pub mod source;

pub use crate::format::page::{Page, PageRef};
pub use cache::{CacheStats, PageCache};
pub use pager::{Pager, PagerStats};
pub use source::{PageSink, PageSource};
