//! Reachability tracing, structural tree copying and compaction (STORE-14..16).
pub mod compact;
pub mod copy;
pub mod trace;

pub use copy::TreeCopier;
pub use trace::{collect_roots, trace, visit_tree, KindCounts, ReachSet, RootUsage, TraceResult};
