//! PGArbor executor: runs physical plans and DDL/utility statements against the transactional core
//! (ARCHITECTURE §10, seed tasks EXEC-01..17).
//!
//! CONTRACT: everything marked `// CONTRACT` is frozen at `contracts-v1`; see
//! `contracts/crates/pgarbor-exec.md` for the design, formats and downstream usage and
//! `contracts/leases/pgarbor-exec.json` for the implementation leases (EXE-01..). Amendment:
//! `contracts/amendments/pgarbor-exec-01-unique-null-entries.md`, `pgarbor-exec-02-upstream-requirements.md`.
//!
//! ## Flow
//! ```text
//! BoundStatement --pgarbor_plan::plan--> Plan --execute::execute / build::open_plan--> ExecResult / PlanStream
//!                                   DDL / utility statements are executed directly from the BoundStatement
//! ```
//! ## Module map
//! | area | modules |
//! |---|---|
//! | entry points | [`execute`] (`execute`, `execute_plan`), [`build`] (`open_plan`, `BuildCtx`), [`result`] ([`result::ExecResult`]) |
//! | environment | [`context`] (`ExecContext`, `SessionEnv`, `GucAccess`, `EngineServices`), [`txn`] (`ExecTxn`, adapter over core `TxnKv`), [`memtxn`] (test double), [`error`] (PG-exact error constructors) |
//! | privileges | [`perm`] |
//! | operator runtime | [`runtime`] (`RowSource`, `Runtime`, `Executor`, `PlanStream`) |
//! | operators | [`node_scan`], [`node_basic`], [`node_func`], [`node_cte`], [`node_join`], [`node_hashjoin`], [`node_mergejoin`], [`node_agg`], [`node_window`] + [`window_funcs`], [`node_sort`] + [`sortutil`], [`node_setops`], [`node_lock`], [`modify`] |
//! | expressions | [`eval`], [`eval_call`], [`eval_cast`], [`eval_srf`], [`subplan`], [`session_fns`], [`exec_session`] |
//! | storage access | [`tabledesc`], [`rowstore`], [`index_key`], [`index_maint`] |
//! | DML | [`dml_insert`], [`dml_update`], [`dml_delete`], [`dml_conflict`], [`constraints`], [`fk`] |
//! | DDL / utility | [`ddl_driver`], [`ddl_table`], [`ddl_alter`], [`ddl_objects`], [`ddl_drop`], [`ddl_followup`], [`ctas`], [`utility`], [`explain_exec`], [`copy`] |
//!
//! ## North-star slice (create .pga -> CREATE TABLE -> INSERT -> SELECT -> COMMIT -> reopen)
//! `ddl_driver` + `ddl_table` + `tabledesc` + `rowstore` + `index_key/index_maint` + `dml_insert` + `modify` +
//! `node_scan` + `node_basic` + `eval*` + `constraints`.
#![allow(clippy::too_many_arguments)]

pub mod build;
pub mod constraints;
pub mod context;
pub mod copy;
pub mod ctas;
pub mod ddl_alter;
pub mod ddl_driver;
pub mod ddl_drop;
pub mod ddl_followup;
pub mod ddl_objects;
pub mod ddl_table;
pub mod dml_conflict;
pub mod dml_delete;
pub mod dml_insert;
pub mod dml_update;
pub mod error;
pub mod eval;
pub mod eval_call;
pub mod eval_cast;
pub mod eval_srf;
pub mod exec_session;
pub mod execute;
pub mod explain_exec;
pub mod fk;
pub mod index_key;
pub mod index_maint;
pub mod memtxn;
pub mod modify;
pub mod node_agg;
pub mod node_basic;
pub mod node_cte;
pub mod node_func;
pub mod node_hashjoin;
pub mod node_join;
pub mod node_lock;
pub mod node_mergejoin;
pub mod node_scan;
pub mod node_setops;
pub mod node_sort;
pub mod node_window;
pub mod perm;
pub mod result;
pub mod rowstore;
pub mod runtime;
pub mod session_fns;
pub mod sortutil;
pub mod subplan;
pub mod tabledesc;
pub mod txn;
pub mod utility;
pub mod window_funcs;

pub use build::{open_plan, BuildCtx};
pub use context::{EngineServices, ExecContext, ExecOptions, GucAccess, NoGuc, NoServices, SessionEnv, StmtState};
pub use execute::{execute, execute_plan, execute_with_channel};
pub use result::ExecResult;
pub use runtime::{Executor, PlanStream, Row, RowSource, Runtime};
pub use txn::{ExecTxn, LatestRead};
