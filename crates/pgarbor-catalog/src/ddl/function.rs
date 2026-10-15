//! CREATE [OR REPLACE] FUNCTION / PROCEDURE and ALTER FUNCTION (catalog side). Lease CAT-19. PG
//! sources: `commands/functioncmds.c` (`CreateFunction`, `interpret_function_parameter_list`,
//! `compute_function_attributes`, `AlterFunction`), `catalog/pg_proc.c` (`ProcedureCreate`),
//! `catalog/namespace.c` (`FuncnameGetCandidates`), `parser/parse_func.c`
//! (`LookupFuncWithArgs`, `LookupFuncName`).
//!
//! The catalog stores the definition ([`Function`]); EXECUTING SQL / PL/pgSQL bodies is
//! pgarbor-exec's business (and later work). The binder resolves parameter types, defaults
//! (`FuncArg::default` as `StoredExpr`), the language and the body text; it validates SQL-language
//! bodies by analysis before calling `create_function` (PG `check_function_bodies`).
//!
//! ## `create_function(f, or_replace)` - `f.oid` is ignored (0)
//! Checks, oracle-verified (PG 18.6):
//! - `check_creatable_namespace`, `require_schema_create`; language must be SQL (14) or PL/pgSQL
//!   (13647) - `internal` (12) / `c` (13) -> 42501 `permission denied for language c` for
//!   non-superusers and 0A000 for superusers (no loadable code in PGArbor).
//! - parameters: IN/OUT/INOUT/VARIADIC/TABLE. Duplicate names among parameters of compatible
//!   modes -> 42P13 `parameter name "a" used more than once`; VARIADIC must be the last IN
//!   parameter (42P13 `VARIADIC parameter must be the last input parameter`) and an array type
//!   (42P13 `VARIADIC parameter must be an array`); a parameter with a default followed by one
//!   without -> 42P13 `input parameters after one with a default value must also have defaults`;
//!   OUT parameters cannot have defaults (42P13 `only input parameters can have default values`).
//! - return type derivation (`ProcedureCreate`): no OUT/TABLE params -> `f.ret` as given; exactly one
//!   OUT/INOUT param -> its type; two or more (or TABLE) -> `record` (2249) with TABLE implying
//!   `retset`; procedures -> `ret = 2278` (void) unless INOUT params make it `record`. `f.ret`
//!   given with OUT params must match (42P13 `function result type must be integer because of OUT
//!   parameters`).
//! - identity: (namespace, name, IN-argument types incl. INOUT/VARIADIC) - an existing function
//!   with the same identity -> 42723 `errors::function_already_exists(name)` (oracle text:
//!   `function "f" already exists with same argument types`; same for procedure/function clashes
//!   because they share the name space) unless `or_replace`.
//! - **OR REPLACE** keeps OID, owner, ACL, comment, dependents; requires ownership
//!   (`must be owner of function f`); rejects: kind change (42809 `"p" is a procedure` DETAIL
//!   `Use DROP PROCEDURE...`: `cannot change routine kind` 42809 + DETAIL `"p" is a procedure.`),
//!   return type change (42P13 `errors::cannot_change_return_type(signature, detail)`; detail
//!   `Row type defined by OUT parameters is different.` for OUT changes), input parameter
//!   rename (42P13 `errors::cannot_change_param_name(param, signature)`), OUT parameter rename
//!   (`cannot change name of OUT parameter`... same family), removing defaults (42P13 `cannot remove
//!   parameter defaults from existing function` HINT `Use DROP FUNCTION f(integer) first.`),
//!   changing default values' types (42P13 `cannot change data type of existing parameter default value`).
//!   Everything else (volatility, strictness, body, cost, config, security) is replaced.
//! - Stores `Function` with `cost` default 100 (sql/plpgsql), `rows` 1000 for SRFs else 0,
//!   `parallel` 'u', `depends_on` for BEGIN ATOMIC bodies. Dependencies on argument/return/default
//!   types are derived by `depend.rs`.
//!
//! ## ALTER FUNCTION
//! `alter_function_attrs`: volatility, strictness, SECURITY DEFINER/INVOKER, LEAKPROOF, PARALLEL,
//! COST, ROWS (22023 `ROWS is not applicable when function does not return a set`), SET/RESET
//! config (`Function::config`). `rename_function`: new identity must be free (42723
//! `function g(integer) already exists in schema "zz_cat"` - oracle wording for rename clash:
//! `function ff(integer) already exists in schema "zz_cat"`). `set_function_schema`, `alter_function_owner`
//! (+ ACL rewrite). All need ownership.
//!
//! ## Signature formatting
//! [`format_signature`] renders `name(argtype,argtype)` exactly like PG `format_procedure` /
//! `pg_get_function_identity_arguments` + name: types via `pgarbor_types::names::format_type`,
//! separated by `,` WITHOUT space (`g(integer,integer[])`), qualified with the schema only when the
//! function is not visible in the search path (`qualified = true` forces it).
use crate::env::CatalogEnv;
use crate::model::*;
use crate::txn::CatalogTxn;
use pgarbor_common::{Oid, PgError, Result};

/// New attribute values for ALTER FUNCTION (`None` = unchanged).
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FunctionChanges {
    pub volatility: Option<FuncVolatility>,
    pub strict: Option<bool>,
    pub security_definer: Option<bool>,
    pub leakproof: Option<bool>,
    pub parallel: Option<u8>,
    pub cost: Option<f32>,
    pub rows: Option<f32>,
    /// `SET name = value` entries (`name=value`) to add/replace.
    pub set_config: Vec<String>,
    /// `RESET name` names to remove (`RESET ALL` = every entry: pass `reset_all`).
    pub reset_config: Vec<String>,
    pub reset_all: bool,
}

/// `name(argtype,argtype)` (module docs).
// CONTRACT
#[allow(unused_variables)]
pub fn format_signature(env: &CatalogEnv, namespace: Oid, name: &str, arg_types: &[Oid], qualified: bool) -> Result<String> {
    Err(PgError::unimplemented("CAT-19", "function signature formatting"))
}

impl<'a> CatalogTxn<'a> {
    /// CREATE [OR REPLACE] FUNCTION/PROCEDURE (module docs). Returns the function OID.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn create_function(&mut self, f: &Function, or_replace: bool) -> Result<Oid> {
        Err(PgError::unimplemented("CAT-19", "CREATE FUNCTION"))
    }

    /// ALTER FUNCTION/PROCEDURE ... attribute changes.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_function_attrs(&mut self, func: Oid, changes: &FunctionChanges) -> Result<()> {
        Err(PgError::unimplemented("CAT-19", "ALTER FUNCTION"))
    }

    /// ALTER FUNCTION ... RENAME TO.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn rename_function(&mut self, func: Oid, new_name: &str, rewriter: &dyn super::spec::ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-19", "ALTER FUNCTION RENAME"))
    }

    /// ALTER FUNCTION ... SET SCHEMA.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn set_function_schema(&mut self, func: Oid, new_schema: Oid, rewriter: &dyn super::spec::ExprRewriter) -> Result<()> {
        Err(PgError::unimplemented("CAT-19", "ALTER FUNCTION SET SCHEMA"))
    }

    /// ALTER FUNCTION ... OWNER TO.
    // CONTRACT
    #[allow(unused_variables)]
    pub fn alter_function_owner(&mut self, func: Oid, new_owner: Oid) -> Result<()> {
        Err(PgError::unimplemented("CAT-19", "ALTER FUNCTION OWNER"))
    }
}
