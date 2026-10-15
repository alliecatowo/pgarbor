//! Session-level statements (lease API-13): SET / RESET / SHOW, SET ROLE, SET SESSION AUTHORIZATION,
//! DISCARD, LISTEN / UNLISTEN / NOTIFY. They never reach the executor (`pgarbor_exec::execute` returns
//! XX000 for them). All return `StatementResult`s with PG's tags.
//!
//! * `SET [SESSION|LOCAL] name = value` (`SetStmt`): `guc.set(name, values, scope)` /
//!   `SetValue::Default` -> `guc.reset`, `SetValue::FromCurrent` -> current value. Tag `SET`.
//!   `SET LOCAL` outside a transaction block (state `Started`, not `ImplicitBlock`/`InBlock`): after
//!   applying (it is reverted at once when the autocommit transaction ends) raise WARNING 25P01
//!   `SET LOCAL can only be used in transaction blocks`. Special names: `transaction_isolation` (like
//!   `SET TRANSACTION ISOLATION LEVEL` for the CURRENT transaction via `Txn::set_isolation`, error from
//!   core when late), `transaction_read_only`, `transaction_deferrable`, `session_authorization`/`role`
//!   (routed to the dedicated functions), `default_transaction_*` plain GUCs. `statement_timeout`,
//!   `lock_timeout` take effect at the next statement start.
//! * `RESET name` / `RESET ALL`: `guc.reset(..)` / `guc.reset_all()`; tag `RESET`.
//! * `SHOW name`: one row, one `text` column named like the parameter's canonical name
//!   (`SHOW datestyle` -> column `DateStyle`, value `ISO, MDY`); `SHOW ALL`: rows `(name, setting,
//!   description)` for every visible parameter; `SHOW TRANSACTION ISOLATION LEVEL` reports the LIVE
//!   transaction's level (`Txn::isolation().as_pg_str()`), `SHOW transaction_read_only` the live flag,
//!   `SHOW server_version` `18.6`, `SHOW is_superuser` the current role's `rolsuper`, `SHOW
//!   session_authorization` the session user, `SHOW role` `none` or the role name. Tag `SHOW`.
//!   Unknown parameter: 42704 `unrecognized configuration parameter "x"`.
//! * `SET ROLE name | NONE` (`local` scope variant too): role must exist (42704 `role "x" does not exist`
//!   - oracle text `role "nobody" does not exist`, SQLSTATE 22023) and the session user must be a member
//!   (42501 `permission denied to set role "x"`); updates `user_oid`, the `role` GUC and `env`
//!   (search_path `$user`). `SET SESSION AUTHORIZATION name | DEFAULT`: superuser only; changes
//!   `session_user_oid` and `user_oid`, reports `session_authorization` (and `is_superuser`) via
//!   ParameterStatus.
//! * `DISCARD ALL`: `prevent_in_transaction_block("DISCARD ALL")` (25001 `DISCARD ALL cannot run inside a transaction
//!   block` in `ImplicitBlock`/`InBlock`/`Failed`); otherwise close all portals, deallocate all prepared statements,
//!   `RESET ALL`, `session_authorization` default, `UNLISTEN *`, release advisory locks
//!   (`ExecTxn::advisory_unlock_all`), forget sequence state (`DISCARD SEQUENCES`: core
//!   `SessionSequences::default`). Tags `DISCARD ALL`, `DISCARD PLANS` (drop cached plans),
//!   `DISCARD SEQUENCES`, `DISCARD TEMP`.
//! * `LISTEN ch` / `UNLISTEN ch|*` / `NOTIFY ch [, 'payload']`: queue in `ListenState` (applied at
//!   commit, `notify.rs`); tags `LISTEN`, `UNLISTEN`, `NOTIFY`. `LISTEN` in a failed block: 25P02 (generic).
use pgarbor_common::{PgError, Result};
use pgarbor_sql::ir::{BoundStatement, DiscardKind, SetStmt};

use crate::session::Session;
use crate::types::StatementResult;

impl Session {
    #[allow(unused_variables)]
    pub(crate) fn exec_set(&mut self, s: &SetStmt) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "SET (Session::exec_set)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_reset(&mut self, name: Option<&str>) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "RESET (Session::exec_reset)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_show(&mut self, name: Option<&str>) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "SHOW (Session::exec_show)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_set_role(&mut self, role: Option<&str>, local: bool) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "SET ROLE (Session::exec_set_role)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_set_session_authorization(&mut self, user: Option<&str>, local: bool) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "SET SESSION AUTHORIZATION (Session::exec_set_session_authorization)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_discard(&mut self, kind: DiscardKind) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "DISCARD (Session::exec_discard)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_listen(&mut self, channel: &str) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "LISTEN (Session::exec_listen)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_unlisten(&mut self, channel: Option<&str>) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "UNLISTEN (Session::exec_unlisten)"))
    }

    #[allow(unused_variables)]
    pub(crate) fn exec_notify(&mut self, channel: &str, payload: Option<&str>) -> Result<StatementResult> {
        Err(PgError::unimplemented("API-13", "NOTIFY (Session::exec_notify)"))
    }

    /// Dispatch a bound session-level statement; `Ok(None)` if `stmt` is not a session statement
    /// (it belongs to the executor).
    pub(crate) fn exec_session_statement(&mut self, stmt: &BoundStatement) -> Result<Option<StatementResult>> {
        use BoundStatement as S;
        let r = match stmt {
            S::Set(s) => self.exec_set(s)?,
            S::Reset(n) => self.exec_reset(n.as_deref())?,
            S::Show(n) => self.exec_show(n.as_deref())?,
            S::SetRole { role, local } => self.exec_set_role(role.as_deref(), *local)?,
            S::SetSessionAuthorization { user, local } => self.exec_set_session_authorization(user.as_deref(), *local)?,
            S::Discard(k) => self.exec_discard(*k)?,
            S::Listen(c) => self.exec_listen(c)?,
            S::Unlisten(c) => self.exec_unlisten(c.as_deref())?,
            S::Notify { channel, payload } => self.exec_notify(channel, payload.as_deref())?,
            S::Prepare(p) => self.exec_sql_prepare(p)?,
            S::Execute(e) => self.exec_sql_execute(e)?,
            S::Deallocate(n) => self.exec_sql_deallocate(n.as_deref())?,
            S::Transaction(t) => self.exec_transaction_stmt(t)?,
            _ => return Ok(None),
        };
        Ok(Some(r))
    }
}
