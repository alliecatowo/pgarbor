//! Doctor / verify / inspect (STORE-19, STORE-15 accounting). Reports serialize to JSON for
//! `pgarbor doctor --json` and `pgarbor inspect --space`.
pub mod report;
pub mod space;
pub mod verify;

pub use report::{DoctorReport, Issue, IssueSeverity, PageInspection, SpaceReport};
pub use space::{inspect_page, space_report};
pub use verify::{verify_container, verify_file, verify_snapshot, verify_tree, TreeStats};
