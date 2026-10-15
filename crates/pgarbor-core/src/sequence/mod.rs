//! Sequences (TXN-13): per-branch, non-transactional `nextval`/`setval` state in keyspace 2
//! (ARCHITECTURE §4), with PostgreSQL's SEQ_LOG_VALS=32 prefetch-logging emulated so that
//! `SELECT last_value, log_cnt, is_called FROM seq` matches PG and a crash skips at most 32 values.

pub mod state;
pub mod store;

pub use state::{seq_key, SequenceParams, SequenceState, SessionSequences, SEQ_LOG_VALS};
pub use store::SequenceStore;
