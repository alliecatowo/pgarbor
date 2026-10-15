//! `pg_proc`, `pg_aggregate`, `pg_operator`, `pg_cast`, `pg_language`. Lease CAT-33.
//! Builtin rows come from `pgarbor_types::catalog_data` (`PROCS` 3413 rows, `AGGREGATES` 162,
//! `OPERATORS` 800, `CASTS` 236) and `builtin::LANGUAGES`; user rows from `Function`.
//! Building every builtin row on each scan is acceptable but WASTEFUL (3413 x 30 datums): use the
//! fast paths (`pg_proc`: eq `oid` col 0, `proname` col 1, `pronamespace` col 2; `pg_operator`: eq
//! `oid`, `oprname`; `pg_cast`: eq `castsource`/`casttarget`) and cache nothing across scans
//! (snapshots are immutable but the builtin tables are `'static`, so rows are cheap to rebuild).
//!
//! ## pg_proc
//! Builtin `BuiltinProc` -> `oid`, `proname`, `pronamespace`, `proowner` 10, `prolang` = `lang`,
//! `procost` (100.0 for SQL language, 1.0 otherwise: `BuiltinProc` carries no cost; documented gap),
//! `prorows` (1000.0 when `retset`, else 0.0), `provariadic` = `variadic`, `prosupport` 0, `prokind`
//! = `ProcKind::as_char`, `prosecdef` false, `proleakproof` false (gap), `proisstrict` = `strict`,
//! `proretset`, `provolatile` = `Volatility::as_char` ('i','s','v'), `proparallel` 's' (gap),
//! `pronargs` = `nargs`, `pronargdefaults` = `ndefaults`, `prorettype` = `ret`, `proargtypes` =
//! `d::oidvector(arg_types)`, `proallargtypes` = `oid[]` of `all_arg_types` (NULL when empty),
//! `proargmodes` = `"char"[]` of `arg_modes` (NULL when empty), `proargnames` = `text[]` of
//! `arg_names` (NULL when empty), `proargdefaults` = `arg_defaults` text or NULL when empty,
//! `protrftypes` NULL, `prosrc` = `prosrc` (for SQL-language builtins with empty `prosrc`: the
//! function name), `probin` NULL, `prosqlbody` NULL, `proconfig` NULL, `proacl` NULL.
//! User `Function` -> same columns from the model (`Function::in_arg_types`, `args` for
//! allargtypes/modes/names when not `all_in()`, `n_defaults`, `body` as `prosrc`, `sql_body.sql`
//! as `prosqlbody`, `config` as `proconfig`, `acl`, `security_definer`, `leakproof`, `parallel`,
//! `cost`, `rows`, `strict`, `volatility`, `ret`, `retset`, `language`, `args[*].default` joined with
//! `, ` as `proargdefaults`). Ordered by `oid`.
//!
//! ## pg_aggregate
//! `BuiltinAggregate`: `aggfnoid` (regproc = proc OID `oid`), `aggkind` ('n','o','h'),
//! `aggnumdirectargs` 0 (1 for hypothetical/ordered-set with direct args: 0 gap), `aggtransfn` =
//! `regproc_oid(transfn)`, `aggfinalfn` = `regproc_oid(finalfn)` (0 for "-"), `aggcombinefn`,
//! `aggserialfn`, `aggdeserialfn`, `aggmtransfn`, `aggminvtransfn`, `aggmfinalfn` = 0 (not recorded
//! in `BuiltinAggregate`; gap), `aggfinalextra`/`aggmfinalextra` false, `aggfinalmodify`/
//! `aggmfinalmodify` 'r', `aggsortop` 0, `aggtranstype` = `transtype`, `aggtransspace` 0,
//! `aggmtranstype` 0, `aggmtransspace` 0, `agginitval` = `initval`, `aggminitval` NULL. Ordered by
//! `aggfnoid`. User-defined aggregates do not exist (CREATE AGGREGATE is unsupported).
//!
//! ## pg_operator
//! `BuiltinOperator`: `oid`, `oprname`, `oprnamespace` 11, `oprowner` 10, `oprkind` ('b' infix, 'l'
//! prefix), `oprcanmerge`, `oprcanhash`, `oprleft`, `oprright`, `oprresult`, `oprcom`, `oprnegate`,
//! `oprcode` = `proc`, `oprrest` 0, `oprjoin` 0 (selectivity functions not recorded; gap).
//!
//! ## pg_cast
//! `BuiltinCast`: `oid`, `castsource`, `casttarget`, `castfunc`, `castcontext` ('i' implicit, 'a'
//! assignment, 'e' explicit), `castmethod` ('f' function, 'b' binary, 'i' in/out). Ordered by `oid`.
//!
//! ## pg_language
//! `builtin::LANGUAGES` (internal 12, c 13, sql 14, plpgsql 13647): `oid, lanname, lanowner,
//! lanispl, lanpltrusted, lanplcallfoid, laninline, lanvalidator, lanacl` (NULL).
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::relid;
use pgarbor_common::{Oid, PgError, Result};

/// Producer for `pg_proc`.
#[derive(Clone, Copy, Debug)]
pub struct PgProc;

impl VirtualRelation for PgProc {
    fn oid(&self) -> Oid {
        relid::PG_PROC
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-33", "pg_proc rows"))
    }
}

/// Producer for `pg_aggregate`.
#[derive(Clone, Copy, Debug)]
pub struct PgAggregate;

impl VirtualRelation for PgAggregate {
    fn oid(&self) -> Oid {
        relid::PG_AGGREGATE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-33", "pg_aggregate rows"))
    }
}

/// Producer for `pg_operator`.
#[derive(Clone, Copy, Debug)]
pub struct PgOperator;

impl VirtualRelation for PgOperator {
    fn oid(&self) -> Oid {
        relid::PG_OPERATOR
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-33", "pg_operator rows"))
    }
}

/// Producer for `pg_cast`.
#[derive(Clone, Copy, Debug)]
pub struct PgCast;

impl VirtualRelation for PgCast {
    fn oid(&self) -> Oid {
        relid::PG_CAST
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-33", "pg_cast rows"))
    }
}

/// Producer for `pg_language`.
#[derive(Clone, Copy, Debug)]
pub struct PgLanguage;

impl VirtualRelation for PgLanguage {
    fn oid(&self) -> Oid {
        relid::PG_LANGUAGE
    }
    #[allow(unused_variables)]
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        Err(PgError::unimplemented("CAT-33", "pg_language rows"))
    }
}


/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgProc, &PgAggregate, &PgOperator, &PgCast, &PgLanguage];
