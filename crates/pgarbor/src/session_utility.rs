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
use pgarbor_catalog::acl::RoleGraph;
use pgarbor_catalog::{CatalogEnv, SearchPath};
use pgarbor_common::{Oid, PgError, Result, Severity, SqlState};
use pgarbor_core::sequence::SessionSequences;
use pgarbor_core::types::IsolationLevel;
use pgarbor_exec::ExecTxn;
use pgarbor_sql::ir::{BoundStatement, DiscardKind, SetScope, SetStmt, SetValue};
use pgarbor_types::{Datum, TypeId};

use crate::guc::GucScope;
use crate::guc_def;
use crate::notify::ListenOp;
use crate::session::Session;
use crate::txn_block::BlockState;
use crate::types::{ColumnInfo, StatementResult};

/// 42704 `unrecognized configuration parameter "x"`.
fn unrecognized(name: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\""))
}

/// 25P02 for session statements rejected in a failed transaction block.
fn failed_block_error() -> PgError {
    PgError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

/// 22023 for a boolean GUC given a non-boolean value (PG `parse_bool` failure text).
fn bool_error(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_PARAMETER_VALUE, format!("parameter \"{name}\" requires a Boolean value"))
}

fn command(tag: &str) -> StatementResult {
    StatementResult::Command { tag: tag.to_string() }
}

fn rows_result(columns: Vec<ColumnInfo>, rows: Vec<Vec<Datum>>, tag: &str) -> StatementResult {
    StatementResult::Rows { columns, rows, tag: tag.to_string() }
}

/// A `text` result column (varlena, no table origin).
fn text_column(name: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.to_string(),
        type_oid: TypeId::TEXT.oid,
        type_mod: -1,
        type_len: -1,
        table_oid: 0,
        column_attnum: 0,
    }
}

/// `on` / `off` display form of a boolean parameter.
fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn scope_of(local: bool) -> GucScope {
    if local {
        GucScope::Local
    } else {
        GucScope::Session
    }
}

/// The single value of a non-list parameter (PG `SET %s takes only one argument`, `guc.c`).
fn single_value(name: &str, values: &[String]) -> Result<String> {
    match values {
        [one] => Ok(one.clone()),
        _ => Err(PgError::new(SqlState::SYNTAX_ERROR, format!("SET {name} takes only one argument"))),
    }
}

/// `SET ROLE` argument: `none` (exact, as PG `check_role` compares with strcmp) means no role.
fn role_arg(values: &[String]) -> Result<Option<String>> {
    let role = single_value("role", values)?;
    if role == "none" {
        Ok(None)
    } else {
        Ok(Some(role))
    }
}

/// PG `parse_bool` (`utils/misc/guc.c`): the words true/false/yes/no/on/off, any unambiguous prefix,
/// and `1`/`0`. `o` alone is ambiguous (on/off). Surrounding whitespace is ignored.
fn parse_bool_literal(text: &str) -> Option<bool> {
    let v = text.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    if v == "1" {
        return Some(true);
    }
    if v == "0" {
        return Some(false);
    }
    const WORDS: [(&str, bool); 6] =
        [("true", true), ("false", false), ("yes", true), ("no", false), ("on", true), ("off", false)];
    let mut found: Option<bool> = None;
    for &(word, val) in WORDS.iter() {
        if word.starts_with(v.as_str()) {
            match found {
                None => found = Some(val),
                Some(prev) if prev == val => {}
                Some(_) => return None,
            }
        }
    }
    found
}

/// Parameters whose SHOW value is the live transaction / session state rather than the GUC slot.
fn is_live_setting(lower: &str) -> bool {
    matches!(lower, "transaction_isolation" | "transaction_read_only" | "transaction_deferrable" | "is_superuser")
}

fn check_known(name: &str) -> Result<()> {
    // Custom placeholder parameters (`app.uid`) are validated by `GucTable::set` (`check_custom_name`).
    if guc_def::find_def(name).is_some() || name.contains('.') {
        Ok(())
    } else {
        Err(unrecognized(name))
    }
}

impl Session {
    /// 25P02 when the block is failed (LISTEN / UNLISTEN / NOTIFY are not accepted there).
    fn reject_if_failed(&self) -> Result<()> {
        if self.block == BlockState::Failed {
            Err(failed_block_error())
        } else {
            Ok(())
        }
    }

    /// WARNING 25P01 `SET LOCAL can only be used in transaction blocks` when a local assignment ran in
    /// the autocommit transaction (state `Started`); the change is reverted when that transaction ends.
    fn warn_local_outside_block(&mut self, scope: GucScope) {
        if scope == GucScope::Local && self.block == BlockState::Started {
            self.pending_notices.push(
                PgError::new(SqlState::NO_ACTIVE_SQL_TRANSACTION, "SET LOCAL can only be used in transaction blocks")
                    .with_severity(Severity::Warning),
            );
        }
    }

    /// OID of the connection's authenticated role (`SessionOptions::user`).
    fn connection_user_oid(&self, env: &CatalogEnv) -> Result<Oid> {
        match env.snapshot().role_by_name(&self.opts.user) {
            Some(role) => Ok(role.oid),
            None => Err(PgError::new(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("role \"{}\" does not exist", self.opts.user),
            )),
        }
    }

    /// Rebuild the cached naming context for a new current/session user (`$user` in search_path depends
    /// on the current user). `base` supplies the snapshot.
    fn rebuild_env(&mut self, base: &CatalogEnv, current: Oid, session: Oid) -> Result<()> {
        let path = SearchPath::parse(&self.guc.show("search_path")?)?;
        let env = CatalogEnv::new(base.snapshot().clone(), &path, current)?
            .with_session(session, self.database_oid)
            .with_row_security(crate::guc_derive::row_security(&self.guc)?);
        self.env = Some(env);
        Ok(())
    }

    /// Current value of a parameter in SHOW form. Transaction characteristics come from the live
    /// transaction when one exists; `is_superuser` is the current role's `rolsuper`.
    fn current_setting(&mut self, name: &str) -> Result<String> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "transaction_isolation" => match self.txn.as_ref() {
                Some(t) => Ok(t.isolation().as_pg_str().to_string()),
                None => self.guc.show(&lower),
            },
            "transaction_read_only" => match self.txn.as_ref() {
                Some(t) => Ok(on_off(t.is_read_only()).to_string()),
                None => self.guc.show(&lower),
            },
            "transaction_deferrable" => match self.txn.as_ref() {
                Some(t) => Ok(on_off(t.is_deferrable()).to_string()),
                None => self.guc.show(&lower),
            },
            "is_superuser" => {
                let env = self.refresh_env()?;
                let graph = RoleGraph::build(env.snapshot(), env.database())?;
                Ok(on_off(graph.is_superuser(self.user_oid)).to_string())
            }
            _ => self.guc.show(&lower),
        }
    }

    /// Re-derive the identity fields from the transactional `session_authorization` and `role` GUC slots.
    /// The GUC stack reverts those slots when a transaction or savepoint ends, but `user_oid`,
    /// `session_user_oid`, `is_superuser` and the cached env are plain fields that do not revert. Statement
    /// paths in this module call this first, so a rolled-back `SET ROLE` / `SET SESSION AUTHORIZATION` does not
    /// leave the session acting as the discarded role.
    pub(crate) fn sync_identity(&mut self) -> Result<()> {
        let session_name = self.guc.show("session_authorization")?;
        let role_name = self.guc.show("role")?;
        let env = self.refresh_env()?;
        let session_oid = match env.snapshot().role_by_name(&session_name) {
            Some(r) => r.oid,
            None => self.session_user_oid,
        };
        let current = if role_name == "none" {
            session_oid
        } else {
            match env.snapshot().role_by_name(&role_name) {
                Some(r) => r.oid,
                None => self.user_oid,
            }
        };
        if session_oid == self.session_user_oid && current == self.user_oid {
            return Ok(());
        }
        let graph = RoleGraph::build(env.snapshot(), env.database())?;
        let is_super = graph.is_superuser(current);
        self.session_user_oid = session_oid;
        self.user_oid = current;
        if self.guc.show("is_superuser")? != on_off(is_super) {
            self.guc.force_value("is_superuser", on_off(is_super))?;
        }
        self.rebuild_env(&env, current, session_oid)
    }

    /// `SET ROLE` / `SET ROLE NONE` core (the caller ran `ensure_txn`). Membership: PG `check_role`
    /// requires `member_can_set_role(session user, role)`.
    fn set_role_impl(&mut self, role: Option<&str>, scope: GucScope) -> Result<()> {
        self.sync_identity()?;
        let env = self.refresh_env()?;
        let graph = RoleGraph::build(env.snapshot(), env.database())?;
        let session_user = self.session_user_oid;
        let (target, text) = match role {
            None => (session_user, "none".to_string()),
            Some(name) => {
                let oid = match env.snapshot().role_by_name(name) {
                    Some(r) => r.oid,
                    None => {
                        return Err(PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("role \"{name}\" does not exist"),
                        ))
                    }
                };
                if !graph.can_set_role(session_user, oid)? {
                    return Err(PgError::new(
                        SqlState::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied to set role \"{name}\""),
                    ));
                }
                (oid, name.to_string())
            }
        };
        let is_super = graph.is_superuser(target);
        self.guc.set("role", &[text], scope)?;
        self.guc.force_value("is_superuser", on_off(is_super))?;
        self.user_oid = target;
        self.rebuild_env(&env, target, session_user)
    }

    /// `SET SESSION AUTHORIZATION name | DEFAULT` core (the caller ran `ensure_txn`). PG
    /// `check_session_authorization`: the target must be the authenticated user, or the authenticated
    /// user must be a superuser. Also resets the current role to none.
    fn set_session_auth_impl(&mut self, user: Option<&str>, scope: GucScope) -> Result<()> {
        self.sync_identity()?;
        let env = self.refresh_env()?;
        let graph = RoleGraph::build(env.snapshot(), env.database())?;
        let authenticated = self.connection_user_oid(&env)?;
        let (target, name) = match user {
            None => (authenticated, self.opts.user.clone()),
            Some(name) => {
                let oid = match env.snapshot().role_by_name(name) {
                    Some(r) => r.oid,
                    None => {
                        return Err(PgError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!("role \"{name}\" does not exist"),
                        ))
                    }
                };
                if oid != authenticated && !graph.is_superuser(authenticated) {
                    return Err(PgError::new(
                        SqlState::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied to set session authorization \"{name}\""),
                    ));
                }
                (oid, name.to_string())
            }
        };
        let is_super = graph.is_superuser(target);
        self.guc.set("session_authorization", &[name], scope)?;
        self.guc.set("role", &["none".to_string()], scope)?;
        self.guc.force_value("is_superuser", on_off(is_super))?;
        self.session_user_oid = target;
        self.user_oid = target;
        self.rebuild_env(&env, target, target)
    }

    /// Apply an isolation level to the live transaction (core errors when it is already too late).
    fn set_txn_isolation(&mut self, level: IsolationLevel) -> Result<()> {
        match self.txn.as_mut() {
            Some(t) => t.set_isolation(level),
            None => Ok(()),
        }
    }

    /// `transaction_isolation`: validated, applied to the live transaction, then stored in the GUC table
    /// so it follows the transaction's commit/abort.
    fn apply_txn_isolation(&mut self, values: Option<Vec<String>>, scope: GucScope) -> Result<()> {
        match values {
            Some(v) => {
                let text = single_value("transaction_isolation", &v)?;
                let level = IsolationLevel::parse(&text)?;
                self.set_txn_isolation(level)?;
                self.guc.set("transaction_isolation", &[text], scope)?;
            }
            None => {
                let stored = self.guc.reset("transaction_isolation", scope)?;
                let level = IsolationLevel::parse(&stored)?;
                self.set_txn_isolation(level)?;
            }
        }
        Ok(())
    }

    /// `transaction_read_only` / `transaction_deferrable`: boolean, applied to the live transaction.
    fn apply_txn_bool(&mut self, name: &str, values: Option<Vec<String>>, scope: GucScope) -> Result<()> {
        match values {
            Some(v) => {
                let text = single_value(name, &v)?;
                let flag = parse_bool_literal(&text).ok_or_else(|| bool_error(name))?;
                self.set_txn_flag(name, flag)?;
                self.guc.set(name, &[on_off(flag).to_string()], scope)?;
            }
            None => {
                let stored = self.guc.reset(name, scope)?;
                let flag = parse_bool_literal(&stored).ok_or_else(|| bool_error(name))?;
                self.set_txn_flag(name, flag)?;
            }
        }
        Ok(())
    }

    fn set_txn_flag(&mut self, name: &str, flag: bool) -> Result<()> {
        match self.txn.as_mut() {
            Some(t) => {
                if name == "transaction_read_only" {
                    t.set_read_only(flag)
                } else {
                    t.set_deferrable(flag)
                }
            }
            None => Ok(()),
        }
    }

    /// One assignment (`SET name = values`, `SET name TO DEFAULT`, `RESET name`) with the special names
    /// routed to their dedicated paths. `name` is lower-cased and known; `values` is `None` for DEFAULT.
    fn apply_assignment(&mut self, name: &str, values: Option<Vec<String>>, scope: GucScope) -> Result<()> {
        self.ensure_txn()?;
        // PG `GUC_NO_RESET` parameters (`guc.c` set_config_option, `guc_tables.c`): RESET and SET TO DEFAULT fail.
        if values.is_none() && matches!(name, "transaction_isolation" | "transaction_read_only" | "transaction_deferrable" | "seed") {
            return Err(PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("parameter \"{name}\" cannot be reset")));
        }
        match name {
            "role" => {
                let role = match values {
                    None => None,
                    Some(v) => role_arg(&v)?,
                };
                self.set_role_impl(role.as_deref(), scope)?;
            }
            "session_authorization" => {
                let user = match values {
                    None => None,
                    Some(v) => Some(single_value(name, &v)?),
                };
                self.set_session_auth_impl(user.as_deref(), scope)?;
            }
            "transaction_isolation" => self.apply_txn_isolation(values, scope)?,
            "transaction_read_only" | "transaction_deferrable" => self.apply_txn_bool(name, values, scope)?,
            _ => match values {
                None => {
                    self.guc.reset(name, scope)?;
                }
                Some(v) => {
                    self.guc.set(name, &v, scope)?;
                }
            },
        }
        self.warn_local_outside_block(scope);
        Ok(())
    }

    /// Clear the per-session sequence memory (PG `DISCARD SEQUENCES`, `ResetSequenceCaches`).
    fn reset_sequence_state(&self) {
        let mut seqs = self.handle.shared().sequences.lock().unwrap_or_else(|e| e.into_inner());
        *seqs = SessionSequences::default();
    }

    /// `DISCARD ALL` (PG `DiscardAll`): refused in a block; drops portals, prepared statements, GUC
    /// settings (except session authorization and role), LISTEN registrations, advisory locks and
    /// sequence memory, and returns the session to the connection user.
    fn discard_all(&mut self) -> Result<()> {
        self.prevent_in_transaction_block("DISCARD ALL")?;
        self.ensure_txn()?;
        self.portals.clear();
        self.prepared.clear();
        self.guc.reset_all()?;
        self.listen.borrow_mut().queue_op(ListenOp::UnlistenAll);
        {
            let txn = self.txn.as_mut().ok_or_else(|| PgError::internal("DISCARD ALL without a transaction"))?;
            let mut kv = txn.kv("DISCARD ALL");
            kv.advisory_unlock_all()?;
        }
        self.reset_sequence_state();
        self.set_session_auth_impl(None, GucScope::Session)
    }

    /// `SET [SESSION|LOCAL] name = value` (`SetStmt`). Tag `SET`.
    pub(crate) fn exec_set(&mut self, s: &SetStmt) -> Result<StatementResult> {
        let name = s.name.to_ascii_lowercase();
        check_known(&name)?;
        let scope = match s.scope {
            SetScope::Session => GucScope::Session,
            SetScope::Local => GucScope::Local,
        };
        let values = match &s.value {
            SetValue::Default => None,
            SetValue::FromCurrent => Some(vec![self.current_setting(&name)?]),
            SetValue::Values(v) => Some(v.clone()),
        };
        self.apply_assignment(&name, values, scope)?;
        Ok(command("SET"))
    }

    /// `RESET name` / `RESET ALL`. Tag `RESET`.
    pub(crate) fn exec_reset(&mut self, name: Option<&str>) -> Result<StatementResult> {
        match name {
            None => {
                self.ensure_txn()?;
                self.guc.reset_all()?;
            }
            Some(n) => {
                let lower = n.to_ascii_lowercase();
                check_known(&lower)?;
                self.apply_assignment(&lower, None, GucScope::Session)?;
            }
        }
        Ok(command("RESET"))
    }

    /// `SHOW name` (one text column named like the canonical parameter) or `SHOW ALL`
    /// (`name`, `setting`, `description`). Tag `SHOW`.
    pub(crate) fn exec_show(&mut self, name: Option<&str>) -> Result<StatementResult> {
        self.sync_identity()?;
        match name {
            None => {
                let all = self.guc.show_all()?;
                let mut rows: Vec<Vec<Datum>> = Vec::with_capacity(all.len());
                for (pname, setting, description) in all {
                    let lower = pname.to_ascii_lowercase();
                    let value = if is_live_setting(&lower) { self.current_setting(&lower)? } else { setting };
                    rows.push(vec![Datum::Text(pname), Datum::Text(value), Datum::Text(description)]);
                }
                let columns = vec![text_column("name"), text_column("setting"), text_column("description")];
                Ok(rows_result(columns, rows, "SHOW"))
            }
            Some(n) => {
                let def = match guc_def::find_def(&n.to_ascii_lowercase()) {
                    Some(def) => def,
                    None => {
                        // Custom (placeholder) parameters become known after their first SET.
                        let value = self.guc.show(&n.to_ascii_lowercase()).map_err(|_| unrecognized(n))?;
                        let col = n.to_ascii_lowercase();
                        return Ok(rows_result(vec![text_column(&col)], vec![vec![Datum::Text(value)]], "SHOW"));
                    }
                };
                let value = self.current_setting(def.name)?;
                Ok(rows_result(vec![text_column(def.name)], vec![vec![Datum::Text(value)]], "SHOW"))
            }
        }
    }

    /// `SET [LOCAL] ROLE name | NONE`. Tag `SET`.
    pub(crate) fn exec_set_role(&mut self, role: Option<&str>, local: bool) -> Result<StatementResult> {
        self.ensure_txn()?;
        let scope = scope_of(local);
        self.set_role_impl(role, scope)?;
        self.warn_local_outside_block(scope);
        Ok(command("SET"))
    }

    /// `SET [LOCAL] SESSION AUTHORIZATION name | DEFAULT`. Tag `SET`.
    pub(crate) fn exec_set_session_authorization(&mut self, user: Option<&str>, local: bool) -> Result<StatementResult> {
        self.ensure_txn()?;
        let scope = scope_of(local);
        self.set_session_auth_impl(user, scope)?;
        self.warn_local_outside_block(scope);
        Ok(command("SET"))
    }

    /// `DISCARD ALL | PLANS | SEQUENCES | TEMP`. `TEMP` has nothing to discard because PGArbor has no
    /// temp schemas (contract non-goal), so it only reports its tag.
    pub(crate) fn exec_discard(&mut self, kind: DiscardKind) -> Result<StatementResult> {
        let tag = match kind {
            DiscardKind::All => {
                self.discard_all()?;
                "DISCARD ALL"
            }
            DiscardKind::Plans => {
                self.prepared.drop_plans();
                "DISCARD PLANS"
            }
            DiscardKind::Sequences => {
                self.reset_sequence_state();
                "DISCARD SEQUENCES"
            }
            DiscardKind::Temp => "DISCARD TEMP",
        };
        Ok(command(tag))
    }

    /// `LISTEN ch`: takes effect at commit (`ListenState`). Tag `LISTEN`.
    pub(crate) fn exec_listen(&mut self, channel: &str) -> Result<StatementResult> {
        self.reject_if_failed()?;
        self.ensure_txn()?;
        self.listen.borrow_mut().queue_op(ListenOp::Listen(channel.to_string()));
        Ok(command("LISTEN"))
    }

    /// `UNLISTEN ch` / `UNLISTEN *`: takes effect at commit. Tag `UNLISTEN`.
    pub(crate) fn exec_unlisten(&mut self, channel: Option<&str>) -> Result<StatementResult> {
        self.reject_if_failed()?;
        self.ensure_txn()?;
        let op = match channel {
            Some(c) => ListenOp::Unlisten(c.to_string()),
            None => ListenOp::UnlistenAll,
        };
        self.listen.borrow_mut().queue_op(op);
        Ok(command("UNLISTEN"))
    }

    /// `NOTIFY ch [, payload]`: queued until commit and de-duplicated (`ListenState::queue_notify`). Tag `NOTIFY`.
    pub(crate) fn exec_notify(&mut self, channel: &str, payload: Option<&str>) -> Result<StatementResult> {
        self.reject_if_failed()?;
        self.ensure_txn()?;
        self.listen.borrow_mut().queue_notify(channel, payload.unwrap_or(""))?;
        Ok(command("NOTIFY"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::types::SessionOptions;

    #[test]
    fn bool_literals_follow_pg_parse_bool() {
        assert_eq!(parse_bool_literal("on"), Some(true));
        assert_eq!(parse_bool_literal(" OFF "), Some(false));
        assert_eq!(parse_bool_literal("t"), Some(true));
        assert_eq!(parse_bool_literal("fals"), Some(false));
        assert_eq!(parse_bool_literal("yes"), Some(true));
        assert_eq!(parse_bool_literal("n"), Some(false));
        assert_eq!(parse_bool_literal("1"), Some(true));
        assert_eq!(parse_bool_literal("0"), Some(false));
        assert_eq!(parse_bool_literal("o"), None);
        assert_eq!(parse_bool_literal(""), None);
        assert_eq!(parse_bool_literal("maybe"), None);
    }

    #[test]
    fn unrecognized_parameter_message() {
        let e = unrecognized("foo");
        assert!(e.sqlstate == SqlState::UNDEFINED_OBJECT);
        assert_eq!(e.message, "unrecognized configuration parameter \"foo\"");
    }

    #[test]
    fn single_value_rules() {
        assert_eq!(single_value("x", &["a".to_string()]).ok(), Some("a".to_string()));
        assert!(single_value("x", &[]).is_err());
        assert!(single_value("x", &["a".to_string(), "b".to_string()]).is_err());
    }

    #[test]
    fn role_none_means_no_role() {
        assert_eq!(role_arg(&["none".to_string()]).ok(), Some(None));
        assert_eq!(role_arg(&["app".to_string()]).ok(), Some(Some("app".to_string())));
    }

    #[test]
    fn text_column_shape() {
        let c = text_column("DateStyle");
        assert_eq!(c.name, "DateStyle");
        assert_eq!(c.type_oid, TypeId::TEXT.oid);
        assert_eq!(c.type_len, -1);
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn show_datestyle_returns_canonical_column() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        let out = s.simple_query("SHOW datestyle")?;
        match out.first().map(|o| &o.result) {
            Some(StatementResult::Rows { columns, rows, .. }) => {
                assert_eq!(columns[0].name, "DateStyle");
                assert!(matches!(rows.first().and_then(|r| r.first()), Some(Datum::Text(v)) if v == "ISO, MDY"));
            }
            _ => panic!("SHOW datestyle must return rows"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn discard_all_inside_block_is_rejected() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        match s.simple_query("BEGIN; DISCARD ALL") {
            Err(e) => assert!(e.sqlstate == SqlState::ACTIVE_SQL_TRANSACTION),
            Ok(_) => panic!("DISCARD ALL must fail inside a transaction block"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn set_unknown_role_is_22023() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        match s.simple_query("SET ROLE nobody") {
            Err(e) => {
                assert!(e.sqlstate == SqlState::INVALID_PARAMETER_VALUE);
                assert_eq!(e.message, "role \"nobody\" does not exist");
            }
            Ok(_) => panic!("SET ROLE nobody must fail"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn listen_notify_delivers_one_notification_at_commit() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        s.simple_query("LISTEN ch; NOTIFY ch, 'x'")?;
        let got = s.take_notifications();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, "x");
        Ok(())
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn set_time_zone_utc_then_show() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        s.simple_query("SET TIME ZONE 'UTC'")?;
        let out = s.simple_query("SHOW TimeZone")?;
        match out.first().map(|o| &o.result) {
            Some(StatementResult::Rows { columns, rows, .. }) => {
                assert_eq!(columns[0].name, "TimeZone");
                assert!(matches!(rows.first().and_then(|r| r.first()), Some(Datum::Text(v)) if v == "UTC"));
            }
            _ => panic!("SHOW TimeZone must return rows"),
        }
        Ok(())
    }

    #[test]
    #[ignore = "needs API-02, API-04, API-05, API-06, API-07, API-08, API-09, API-11"]
    fn show_all_has_three_columns() -> Result<()> {
        let db = Database::in_memory()?;
        let mut s = db.session("main", SessionOptions::default())?;
        let out = s.simple_query("SHOW ALL")?;
        match out.first().map(|o| &o.result) {
            Some(StatementResult::Rows { columns, .. }) => {
                let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["name", "setting", "description"]);
            }
            _ => panic!("SHOW ALL must return rows"),
        }
        Ok(())
    }
}
