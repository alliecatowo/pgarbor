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
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::registry::is_polymorphic;
use pgarbor_types::{oids, TypeEnv, TypeId};

use super::context::Binder;
use super::types::is_pseudo_type;
use crate::diag;
use crate::ir::BoundExpr;

/// Result of reading the text of a `$n` placeholder.
enum ParamRef {
    /// `1 <= n <= MAX_PARAMS`.
    Number(u32),
    /// Digits that name no valid parameter (`0`, or above `MAX_PARAMS`); holds the digits as PG
    /// prints them (leading zeros stripped).
    OutOfRange(String),
}

/// Reads `$digits`. `None` when the text is not `$` followed by one or more ASCII digits.
fn parse_placeholder(text: &str) -> Option<ParamRef> {
    let digits = text.strip_prefix('$')?;
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let trimmed = digits.trim_start_matches('0');
    let shown = if trimmed.is_empty() { "0" } else { trimmed };
    match shown.parse::<u32>() {
        Ok(n) if n >= 1 && n <= MAX_PARAMS => Some(ParamRef::Number(n)),
        _ => Some(ParamRef::OutOfRange(shown.to_string())),
    }
}

/// 42P02 `there is no parameter $N` for a number that names no valid parameter.
fn no_such_param(shown: &str, pos: Option<u32>) -> PgError {
    let mut e = PgError::new(SqlState::UNDEFINED_PARAMETER, format!("there is no parameter ${shown}"));
    e.position = pos;
    e
}

/// Makes `types` and `declared` at least `n` slots long (new slots are `None`). Does not touch
/// `max_referenced`; callers that reference a parameter raise it themselves.
fn ensure_slots(table: &mut ParamTable, n: u32) {
    let len = n as usize;
    if table.types.len() < len {
        table.types.resize(len, None);
    }
    if table.declared.len() < len {
        table.declared.resize(len, None);
    }
}

/// The type an inferred parameter is pinned to. PostgreSQL keeps only the type OID for a
/// parameter, so the typmod is always -1.
fn pinned_type(oid: Oid) -> TypeId {
    TypeId { oid, typmod: -1 }
}

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
pub fn bind_param(b: &mut Binder, text: &str, pos: Option<u32>) -> Result<BoundExpr> {
    let n = match parse_placeholder(text) {
        // `?`, `:x`, `$x`: not PostgreSQL parameter syntax (PG scanner: `syntax error at or near`).
        None => return Err(diag::syntax_error_near(text, pos)),
        Some(ParamRef::OutOfRange(shown)) => return Err(no_such_param(&shown, pos)),
        Some(ParamRef::Number(n)) => n,
    };
    ensure_slots(&mut b.params, n);
    if n > b.params.max_referenced {
        b.params.max_referenced = n;
    }
    let ty = b.params.type_of(n).unwrap_or(TypeId::UNKNOWN);
    Ok(BoundExpr::param(n, ty))
}

/// Pin the type of unresolved parameter `n` to `target` (called by the coercion layer) and return
/// the retyped node. If `n` already has a different type: 42P08 `inconsistent types deduced for
/// parameter $n` DETAIL `<old> versus <new>`. If `target` is itself `unknown`/polymorphic
/// pseudo-type, leave unresolved and return the node unchanged. Domain targets pin the domain's
/// BASE type? No: PG pins the domain type itself (verified: `prepare p as select * from t where d=$1`
/// reports the domain OID in `pg_prepared_statements.parameter_types`).
pub fn resolve_param(b: &mut Binder, n: u32, target: TypeId, pos: Option<u32>) -> Result<BoundExpr> {
    if n == 0 || n > MAX_PARAMS {
        return Err(no_such_param(&n.to_string(), pos));
    }
    ensure_slots(&mut b.params, n);
    let idx = n as usize - 1;
    let current = b.params.types[idx];

    // Client-declared types are fixed (PG `variable_coerce_param_hook` only pins `unknown`
    // parameters). A conflicting use is reported by the coercion of the usage site, not here.
    if b.params.declared[idx].is_some() {
        return Ok(BoundExpr::param(n, current.unwrap_or(TypeId::UNKNOWN)));
    }

    // Targets that cannot be a parameter type leave the parameter unresolved (PG `parse_param.c`:
    // only concrete types are pinned; pseudo-types such as `unknown` or `anyelement` are not).
    let unpinnable = target.oid == oids::UNKNOWN
        || is_pseudo_type(target.oid)
        || is_polymorphic(target.oid)
        || b.env.type_info(target.oid).map_or(false, |t| t.is_pseudo());
    if unpinnable {
        return Ok(BoundExpr::param(n, current.unwrap_or(TypeId::UNKNOWN)));
    }

    match current {
        None => {
            // Domains are pinned as the domain type itself (no base-type stripping).
            let pinned = pinned_type(target.oid);
            b.params.types[idx] = Some(pinned);
            Ok(BoundExpr::param(n, pinned))
        }
        Some(cur) if cur.oid == target.oid => Ok(BoundExpr::param(n, cur)),
        Some(cur) => {
            let old = b.type_name(cur);
            let new = b.type_name(target);
            Err(diag::inconsistent_param_types(n, &old, &new, pos))
        }
    }
}

/// End of binding: every parameter `1..=count` must have a type. First unresolved one raises
/// 42P18 `could not determine data type of parameter $n`. Returns the final types.
/// (`BindOutput::param_types`). Parameters in the gaps between referenced numbers that are neither
/// declared nor referenced (`$1`, `$3` used, `$2` never) are also 42P18 (PG behaves the same).
pub fn finalize(b: &mut Binder) -> Result<Vec<TypeId>> {
    let count = b.params.count();
    let mut out = Vec::with_capacity(count as usize);
    for n in 1..=count {
        match b.params.type_of(n) {
            Some(ty) => out.push(ty),
            // PG `check_variable_parameters` / `parse_analyze_varparams`: no position.
            None => return Err(diag::param_type_unknown(n, None)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::context::{BindContext, BindSession};
    use super::*;
    use crate::source::SourceMap;
    use pgarbor_catalog::{testing, CatalogEnv};
    use pgarbor_types::context::IoConfig;

    fn ctx_for(env: &CatalogEnv, declared: Vec<TypeId>) -> BindContext<'_> {
        BindContext {
            env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: declared,
        }
    }

    #[test]
    fn declared_type_is_fixed() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![TypeId::INT4]);
        let src = SourceMap::new("select $1");
        let mut b = Binder::new(&ctx, &src);
        let e = bind_param(&mut b, "$1", Some(8))?;
        assert_eq!(e.ty, TypeId::INT4);
        let again = resolve_param(&mut b, 1, TypeId::TEXT, Some(9))?;
        assert_eq!(again.ty, TypeId::INT4);
        assert_eq!(finalize(&mut b)?, vec![TypeId::INT4]);
        Ok(())
    }

    #[test]
    fn later_slot_first_then_earlier() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![]);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        let e2 = bind_param(&mut b, "$2", None)?;
        assert!(e2.ty.oid == oids::UNKNOWN);
        bind_param(&mut b, "$1", None)?;
        resolve_param(&mut b, 2, TypeId::TEXT, None)?;
        assert_eq!(b.params.count(), 2);
        let err = finalize(&mut b).err().expect("$1 is unresolved");
        assert!(err.sqlstate == SqlState::INDETERMINATE_DATATYPE);
        assert_eq!(err.message, "could not determine data type of parameter $1");
        resolve_param(&mut b, 1, TypeId::INT4, None)?;
        assert_eq!(finalize(&mut b)?, vec![TypeId::INT4, TypeId::TEXT]);
        Ok(())
    }

    #[test]
    fn gap_slot_is_unresolved() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![]);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        bind_param(&mut b, "$3", None)?;
        bind_param(&mut b, "$1", None)?;
        resolve_param(&mut b, 1, TypeId::INT4, None)?;
        resolve_param(&mut b, 3, TypeId::TEXT, None)?;
        let err = finalize(&mut b).err().expect("$2 is never referenced");
        assert!(err.sqlstate == SqlState::INDETERMINATE_DATATYPE);
        assert_eq!(err.message, "could not determine data type of parameter $2");
        Ok(())
    }

    #[test]
    fn conflicting_inferred_types() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![]);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        bind_param(&mut b, "$1", Some(20))?;
        resolve_param(&mut b, 1, TypeId::INT4, Some(10))?;
        let again = bind_param(&mut b, "$1", Some(30))?;
        assert_eq!(again.ty, TypeId::INT4);
        let err = resolve_param(&mut b, 1, TypeId::TEXT, Some(30)).err().expect("42P08");
        assert!(err.sqlstate == SqlState::AMBIGUOUS_PARAMETER);
        assert_eq!(err.message, "inconsistent types deduced for parameter $1");
        assert_eq!(err.detail.as_deref(), Some("integer versus text"));
        assert_eq!(err.position, Some(30));
        Ok(())
    }

    #[test]
    fn placeholder_text_validation() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![]);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        for bad in ["$0", "$65536", "$99999999999999999999"] {
            let err = bind_param(&mut b, bad, Some(8)).err().expect("42P02");
            assert!(err.sqlstate == SqlState::UNDEFINED_PARAMETER);
            assert_eq!(err.position, Some(8));
        }
        let err = bind_param(&mut b, "$0", None).err().expect("42P02");
        assert_eq!(err.message, "there is no parameter $0");
        let err = bind_param(&mut b, "$65536", None).err().expect("42P02");
        assert_eq!(err.message, "there is no parameter $65536");
        let err = bind_param(&mut b, "?", Some(3)).err().expect("42601");
        assert!(err.sqlstate == SqlState::SYNTAX_ERROR);
        assert_eq!(err.message, "syntax error at or near \"?\"");
        assert!(bind_param(&mut b, "$65535", None).is_ok());
        Ok(())
    }

    #[test]
    fn unknown_target_leaves_param_open() -> Result<()> {
        let (_kv, env) = testing::bootstrapped()?;
        let ctx = ctx_for(&env, vec![]);
        let src = SourceMap::new("");
        let mut b = Binder::new(&ctx, &src);
        assert!(finalize(&mut b)?.is_empty());
        bind_param(&mut b, "$1", None)?;
        let kept = resolve_param(&mut b, 1, TypeId::UNKNOWN, None)?;
        assert!(kept.ty.oid == oids::UNKNOWN);
        assert!(b.params.type_of(1).is_none());
        let err = finalize(&mut b).err().expect("$1 is unresolved");
        assert!(err.sqlstate == SqlState::INDETERMINATE_DATATYPE);
        assert!(err.position.is_none());
        Ok(())
    }
}
