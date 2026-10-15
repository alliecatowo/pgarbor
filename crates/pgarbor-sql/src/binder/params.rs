//! Parameter (`$n`) typing: declared types, inference from context, finalization (SQL-06).
//! PG reference: `src/backend/parser/parse_param.c` (`variable_paramref_hook`,
//! `variable_coerce_param_hook`, `check_variable_parameters`).
//!
//! ## Model
//! `ParamTable` holds, for each `$n` seen or declared, `Option<TypeId>`:
//! * declared by the client (Parse message / `PREPARE p(int, text)`): fixed, never changes;
//! * otherwise unresolved until some coercion site pins it ("inference").
//!
//! Binding `$n` yields `BoundExpr::param(n, ty)` where `ty` is the known type, or `unknown` while
//! unresolved (an unresolved `Param` is a [`BoundArg::is_unknown`] argument exactly like an untyped
//! string literal). The coercion layer (`coerce::try_coerce`) calls [`resolve_param`] when it is
//! asked to coerce an unknown `Param` to a concrete target: the table records the target type and
//! the node is rebuilt with that type. The tree is never patched afterwards: every site that needs a
//! type coerces the parameter itself (`WHERE a = $1` coerces `$1` to a's type; `SELECT $1` is
//! coerced to `text` by the target-list unknown resolution; `values($1)` and `coalesce($1,$1)` too).
//!
//! ## Oracle-verified behavior (PG 18.6)
//! * `prepare p as select $1` -> OK, `$1` is `text`.
//! * `prepare p as select $1 is null` -> 42P18 `could not determine data type of parameter $1`
//!   (no position).
//! * `prepare p as select $1+$2` -> 42725 `operator is not unique: unknown + unknown` (operator
//!   resolution error, position at the operator).
//! * `select * from (select 1 a) s where a = $1 and a < $1::bigint` -> OK: `$1` is int4 from the first
//!   site; `$1::bigint` casts the int4 parameter.
//! * `$1` used with two different inferred types (`where a = $1 and b = $1`, `a int`, `b text`) ->
//!   42P08 `inconsistent types deduced for parameter $1` DETAIL `integer versus text`, position at the
//!   second `$1`.
//! * `$0` or a number above 65535 -> 42P02 `there is no parameter $0`.
//! * A declared type conflicting with usage is NOT checked here (the coercion error is normal:
//!   22P02/42804/42883 at the usage site).
use pgarbor_common::{PgError, Result};
use pgarbor_types::TypeId;

use super::context::Binder;
use crate::ir::BoundExpr;

/// PG `MaxAllocSize/sizeof(Oid)` limit is huge; the wire protocol caps parameters at 65535.
pub const MAX_PARAMS: u32 = 65535;

/// Types of the statement parameters. Index `i` is `$i+1`.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamTable {
    /// Declared by the client; `None` = to infer.
    pub declared: Vec<Option<TypeId>>,
    /// Current type of each parameter (declared or inferred so far).
    pub types: Vec<Option<TypeId>>,
    /// Highest `$n` referenced by the statement (0 = none).
    pub max_referenced: u32,
}

impl ParamTable {
    /// From the client's declared types (OID 0 / `unknown` = infer).
    pub fn new(declared: &[TypeId]) -> ParamTable {
        let decl: Vec<Option<TypeId>> = declared
            .iter()
            .map(|t| if t.oid == 0 || t.oid == pgarbor_types::oids::UNKNOWN { None } else { Some(*t) })
            .collect();
        ParamTable { types: decl.clone(), declared: decl, max_referenced: 0 }
    }
    /// Current type of `$n` (1-based); `None` = unresolved or never mentioned.
    pub fn type_of(&self, n: u32) -> Option<TypeId> {
        if n == 0 {
            return None;
        }
        self.types.get(n as usize - 1).copied().flatten()
    }
    /// Number of parameters the statement has: max(declared count, highest referenced).
    pub fn count(&self) -> u32 {
        (self.declared.len() as u32).max(self.max_referenced)
    }
}

/// Bind a `$n` reference (`Value::Placeholder("$3")`): validates `1 <= n <= 65535` (42P02
/// `there is no parameter $N`), grows the table, updates `max_referenced`, returns
/// `Param(n)` typed with the known type or `unknown`. `text` is the placeholder text (`$3`);
/// anonymous `?` / named `:x` placeholders are not PostgreSQL syntax -> 42601 `syntax error at or
/// near "?"`.
#[allow(unused_variables)]
pub fn bind_param(b: &mut Binder, text: &str, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-06", "parameter reference binding (params::bind_param)"))
}

/// Pin the type of unresolved parameter `n` to `target` (called by the coercion layer) and return
/// the retyped node. If `n` already has a different type: 42P08 `inconsistent types deduced for
/// parameter $n` DETAIL `<old> versus <new>`. If `target` is itself `unknown`/polymorphic
/// pseudo-type, leave unresolved and return the node unchanged. Domain targets pin the domain's
/// BASE type? No: PG pins the domain type itself (verified: `prepare p as select * from t where d=$1`
/// reports the domain OID in `pg_prepared_statements.parameter_types`).
#[allow(unused_variables)]
pub fn resolve_param(b: &mut Binder, n: u32, target: TypeId, pos: Option<u32>) -> Result<BoundExpr> {
    Err(PgError::unimplemented("SQL-06", "parameter type inference (params::resolve_param)"))
}

/// End of binding: every parameter `1..=count` must have a type. First unresolved one raises
/// 42P18 `could not determine data type of parameter $n`. Returns the final types.
/// (`BindOutput::param_types`). Parameters in the gaps between referenced numbers that are neither
/// declared nor referenced (`$1`, `$3` used, `$2` never) are also 42P18 (PG behaves the same).
#[allow(unused_variables)]
pub fn finalize(b: &mut Binder) -> Result<Vec<TypeId>> {
    Err(PgError::unimplemented("SQL-06", "parameter finalization (params::finalize)"))
}
