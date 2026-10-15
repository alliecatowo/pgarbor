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
use super::pg_type::regproc_oid;
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::model::{ArgMode, Function};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, Result};
use pgarbor_types::catalog_data::{
    self, AggKind, BuiltinAggregate, BuiltinCast, BuiltinOperator, BuiltinProc, CastContext, CastMethod, OprKind,
};
use pgarbor_types::Datum;

/// `proowner` / `oprowner` / `lanowner` of every builtin object (the bootstrap superuser).
const BOOTSTRAP_SUPERUSER: Oid = 10;
/// `procost` of builtin SQL-language functions (PG default for `LANGUAGE sql`).
const SQL_PROC_COST: f32 = 100.0;
/// `procost` of every other builtin function.
const DEFAULT_PROC_COST: f32 = 1.0;
/// `prorows` of set-returning builtin functions.
const SRF_PROC_ROWS: f32 = 1000.0;
/// `prolang` of SQL-language builtins.
const LANG_SQL: Oid = 14;

/// Producer for `pg_proc`.
#[derive(Clone, Copy, Debug)]
pub struct PgProc;

impl VirtualRelation for PgProc {
    fn oid(&self) -> Oid {
        relid::PG_PROC
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let name = text_filter(req, 1);
        let nsp = req.oid_eq_on(2);
        let mut rows = Vec::new();
        for p in by_oid(catalog_data::PROCS, oid, |p| p.oid) {
            if name.map_or(false, |n| p.name != n) || nsp.map_or(false, |n| p.namespace != n) {
                continue;
            }
            rows.push(builtin_proc_row(schema, p)?);
        }
        let snap = ctx.env.snapshot();
        let users: Vec<&Function> = match oid {
            Some(o) => snap.function(o).into_iter().collect(),
            None => snap.data().functions.values().collect(),
        };
        for f in users {
            if name.map_or(false, |n| f.name != n) || nsp.map_or(false, |n| f.namespace != n) {
                continue;
            }
            rows.push(user_proc_row(schema, f, snap)?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_aggregate`.
#[derive(Clone, Copy, Debug)]
pub struct PgAggregate;

impl VirtualRelation for PgAggregate {
    fn oid(&self) -> Oid {
        relid::PG_AGGREGATE
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let mut rows = Vec::new();
        for a in by_oid(catalog_data::AGGREGATES, oid, |a| a.oid) {
            rows.push(aggregate_row(schema, a)?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_operator`.
#[derive(Clone, Copy, Debug)]
pub struct PgOperator;

impl VirtualRelation for PgOperator {
    fn oid(&self) -> Oid {
        relid::PG_OPERATOR
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let name = text_filter(req, 1);
        let mut rows = Vec::new();
        for o in by_oid(catalog_data::OPERATORS, oid, |o| o.oid) {
            if name.map_or(false, |n| o.name != n) {
                continue;
            }
            rows.push(operator_row(schema, o)?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_cast`.
#[derive(Clone, Copy, Debug)]
pub struct PgCast;

impl VirtualRelation for PgCast {
    fn oid(&self) -> Oid {
        relid::PG_CAST
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let source = req.oid_eq_on(1);
        let target = req.oid_eq_on(2);
        let mut rows = Vec::new();
        for c in by_oid(catalog_data::CASTS, oid, |c| c.oid) {
            if source.map_or(false, |s| c.source != s) || target.map_or(false, |t| c.target != t) {
                continue;
            }
            rows.push(cast_row(schema, c)?);
        }
        Ok(rows)
    }
}

/// Producer for `pg_language`.
#[derive(Clone, Copy, Debug)]
pub struct PgLanguage;

impl VirtualRelation for PgLanguage {
    fn oid(&self) -> Oid {
        relid::PG_LANGUAGE
    }
    fn scan(&self, _ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let mut rows = Vec::new();
        for l in builtin::LANGUAGES.iter().filter(|l| oid.map_or(true, |o| l.oid == o)) {
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(l.oid))
                .set("lanname", d::name(l.name))
                .set("lanowner", d::oid(l.owner))
                .set("lanispl", d::bool_(l.is_pl))
                .set("lanpltrusted", d::bool_(l.trusted))
                .set("lanplcallfoid", d::oid(l.call_handler))
                .set("laninline", d::oid(l.inline_handler))
                .set("lanvalidator", d::oid(l.validator))
                .set("lanacl", Datum::Null);
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgProc, &PgAggregate, &PgOperator, &PgCast, &PgLanguage];

// ---------------------------------------------------------------- helpers

/// Equality filter on a `name`/`text` column, only when the request carries a text datum. Ignoring
/// any other filter shape is always correct because the executor re-applies the WHERE clause.
fn text_filter(req: &ScanRequest, idx: usize) -> Option<&str> {
    match req.eq_on(idx) {
        Some(Datum::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// The candidate slice of a builtin table sorted ascending by OID: a single row for an OID
/// equality filter (binary search), the whole table otherwise.
fn by_oid<T: 'static>(all: &'static [T], oid: Option<Oid>, key: impl Fn(&T) -> Oid) -> &'static [T] {
    match oid {
        None => all,
        Some(o) => match all.binary_search_by_key(&o, |t| key(t)) {
            Ok(i) => &all[i..=i],
            Err(_) => &all[..0],
        },
    }
}

/// `NULL` for an empty list, else the `oid[]` array.
fn opt_oid_array(v: &[Oid]) -> Datum {
    if v.is_empty() {
        Datum::Null
    } else {
        d::oid_array(v)
    }
}

/// `NULL` for an empty list, else the `"char"[]` array.
fn opt_char_array(v: &[u8]) -> Datum {
    if v.is_empty() {
        Datum::Null
    } else {
        d::char_array(v)
    }
}

/// `NULL` for an empty list, else the `text[]` array.
fn opt_names(v: &[&str]) -> Datum {
    if v.is_empty() {
        Datum::Null
    } else {
        let owned: Vec<String> = v.iter().map(|s| (*s).to_string()).collect();
        d::text_array(&owned)
    }
}

/// `pg_operator.oprkind` byte for an operator kind.
fn opr_kind_char(kind: OprKind) -> u8 {
    match kind {
        OprKind::Infix => b'b',
        OprKind::Prefix => b'l',
    }
}

fn agg_kind_char(kind: AggKind) -> u8 {
    match kind {
        AggKind::Normal => b'n',
        AggKind::OrderedSet => b'o',
        AggKind::Hypothetical => b'h',
    }
}

fn cast_context_char(ctx: CastContext) -> u8 {
    match ctx {
        CastContext::Implicit => b'i',
        CastContext::Assignment => b'a',
        CastContext::Explicit => b'e',
    }
}

fn cast_method_char(method: CastMethod) -> u8 {
    match method {
        CastMethod::Function => b'f',
        CastMethod::Binary => b'b',
        CastMethod::InOut => b'i',
    }
}

/// `provariadic` of a user function: the element type of its VARIADIC parameter. Only builtin
/// array types expose their element type here; a user array type reports 0 (documented gap).
fn user_variadic_elem(f: &Function) -> Oid {
    f.args
        .iter()
        .find(|a| a.mode == ArgMode::Variadic)
        .and_then(|a| catalog_data::builtin_type(a.type_oid))
        .map(|t| t.elem)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- row builders

/// One `pg_proc` row for a builtin procedure (module docs, `pg_proc`).
fn builtin_proc_row(schema: &'static SystemRel, p: &BuiltinProc) -> Result<Row> {
    let is_sql = p.lang == LANG_SQL;
    let procost = if is_sql { SQL_PROC_COST } else { DEFAULT_PROC_COST };
    let prorows = if p.retset { SRF_PROC_ROWS } else { 0.0 };
    let prosrc = if p.prosrc.is_empty() && is_sql { p.name } else { p.prosrc };
    let mut b = RowBuilder::new(schema);
    b.set("oid", d::oid(p.oid))
        .set("proname", d::name(p.name))
        .set("pronamespace", d::oid(p.namespace))
        .set("proowner", d::oid(BOOTSTRAP_SUPERUSER))
        .set("prolang", d::oid(p.lang))
        .set("procost", d::float4(procost))
        .set("prorows", d::float4(prorows))
        .set("provariadic", d::oid(p.variadic))
        .set("prosupport", d::oid(0))
        .set("prokind", d::char_(p.kind.as_char() as u8))
        .set("prosecdef", d::bool_(false))
        .set("proleakproof", d::bool_(false))
        .set("proisstrict", d::bool_(p.strict))
        .set("proretset", d::bool_(p.retset))
        .set("provolatile", d::char_(p.volatility.as_char() as u8))
        .set("proparallel", d::char_(b's'))
        .set("pronargs", d::int2(p.nargs as i16))
        .set("pronargdefaults", d::int2(p.ndefaults as i16))
        .set("prorettype", d::oid(p.ret))
        .set("proargtypes", d::oidvector(p.arg_types))
        .set("proallargtypes", opt_oid_array(p.all_arg_types))
        .set("proargmodes", opt_char_array(p.arg_modes))
        .set("proargnames", opt_names(p.arg_names))
        .set(
            "proargdefaults",
            if p.arg_defaults.is_empty() { Datum::Null } else { d::text(p.arg_defaults) },
        )
        .set("protrftypes", Datum::Null)
        .set("prosrc", d::text(prosrc))
        .set("probin", Datum::Null)
        .set("prosqlbody", Datum::Null)
        .set("proconfig", Datum::Null)
        .set("proacl", Datum::Null);
    b.finish()
}

/// One `pg_proc` row for a user-defined function (module docs, `pg_proc`).
fn user_proc_row(schema: &'static SystemRel, f: &Function, snap: &CatalogSnapshot) -> Result<Row> {
    let in_types = f.in_arg_types();
    let all_types: Vec<Oid> = f.args.iter().map(|a| a.type_oid).collect();
    let modes: Vec<u8> = f.args.iter().map(|a| a.mode.as_char()).collect();
    let names: Vec<String> = f.args.iter().map(|a| a.name.clone()).collect();
    let defaults: Vec<&str> = f.args.iter().filter_map(|a| a.default.as_ref().map(|e| e.sql.as_str())).collect();
    let joined_defaults = defaults.join(", ");
    // PG fills proallargtypes/proargmodes only when some argument is not IN; proargnames whenever
    // any argument is named.
    let (allargtypes, argmodes) = if f.all_in() {
        (Datum::Null, Datum::Null)
    } else {
        (d::oid_array(&all_types), d::char_array(&modes))
    };
    let argnames = if names.iter().all(|n| n.is_empty()) { Datum::Null } else { d::text_array(&names) };
    let config = if f.config.is_empty() { Datum::Null } else { d::text_array(&f.config) };
    let sqlbody = match &f.sql_body {
        Some(e) => d::text(&e.sql),
        None => Datum::Null,
    };
    let mut b = RowBuilder::new(schema);
    b.set("oid", d::oid(f.oid))
        .set("proname", d::name(&f.name))
        .set("pronamespace", d::oid(f.namespace))
        .set("proowner", d::oid(f.owner))
        .set("prolang", d::oid(f.language))
        .set("procost", d::float4(f.cost))
        .set("prorows", d::float4(f.rows))
        .set("provariadic", d::oid(user_variadic_elem(f)))
        .set("prosupport", d::oid(0))
        .set("prokind", d::char_(f.kind.to_types().as_char() as u8))
        .set("prosecdef", d::bool_(f.security_definer))
        .set("proleakproof", d::bool_(f.leakproof))
        .set("proisstrict", d::bool_(f.strict))
        .set("proretset", d::bool_(f.retset))
        .set("provolatile", d::char_(f.volatility.to_types().as_char() as u8))
        .set("proparallel", d::char_(f.parallel))
        .set("pronargs", d::int2(in_types.len() as i16))
        .set("pronargdefaults", d::int2(f.n_defaults() as i16))
        .set("prorettype", d::oid(f.ret))
        .set("proargtypes", d::oidvector(&in_types))
        .set("proallargtypes", allargtypes)
        .set("proargmodes", argmodes)
        .set("proargnames", argnames)
        .set(
            "proargdefaults",
            if defaults.is_empty() { Datum::Null } else { d::text(&joined_defaults) },
        )
        .set("protrftypes", Datum::Null)
        .set("prosrc", d::text(&f.body))
        .set("probin", Datum::Null)
        .set("prosqlbody", sqlbody)
        .set("proconfig", config)
        .set("proacl", d::acl_array(f.acl.as_ref(), snap));
    b.finish()
}

/// One `pg_aggregate` row (module docs, `pg_aggregate`).
fn aggregate_row(schema: &'static SystemRel, a: &BuiltinAggregate) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("aggfnoid", d::oid(a.oid))
        .set("aggkind", d::char_(agg_kind_char(a.kind)))
        .set("aggnumdirectargs", d::int2(0))
        .set("aggtransfn", d::oid(regproc_oid(a.transfn)))
        .set("aggfinalfn", d::oid(regproc_oid(a.finalfn)))
        .set("aggcombinefn", d::oid(0))
        .set("aggserialfn", d::oid(0))
        .set("aggdeserialfn", d::oid(0))
        .set("aggmtransfn", d::oid(0))
        .set("aggminvtransfn", d::oid(0))
        .set("aggmfinalfn", d::oid(0))
        .set("aggfinalextra", d::bool_(false))
        .set("aggmfinalextra", d::bool_(false))
        .set("aggfinalmodify", d::char_(b'r'))
        .set("aggmfinalmodify", d::char_(b'r'))
        .set("aggsortop", d::oid(0))
        .set("aggtranstype", d::oid(a.transtype))
        .set("aggtransspace", d::int4(0))
        .set("aggmtranstype", d::oid(0))
        .set("aggmtransspace", d::int4(0))
        .set("agginitval", d::opt_text(a.initval))
        .set("aggminitval", Datum::Null);
    b.finish()
}

/// One `pg_operator` row (module docs, `pg_operator`).
fn operator_row(schema: &'static SystemRel, o: &BuiltinOperator) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("oid", d::oid(o.oid))
        .set("oprname", d::name(o.name))
        .set("oprnamespace", d::oid(builtin::PG_CATALOG_NAMESPACE))
        .set("oprowner", d::oid(BOOTSTRAP_SUPERUSER))
        .set("oprkind", d::char_(opr_kind_char(o.kind)))
        .set("oprcanmerge", d::bool_(o.can_merge))
        .set("oprcanhash", d::bool_(o.can_hash))
        .set("oprleft", d::oid(o.left))
        .set("oprright", d::oid(o.right))
        .set("oprresult", d::oid(o.result))
        .set("oprcom", d::oid(o.commutator))
        .set("oprnegate", d::oid(o.negator))
        .set("oprcode", d::oid(o.proc))
        .set("oprrest", d::oid(0))
        .set("oprjoin", d::oid(0));
    b.finish()
}

/// One `pg_cast` row (module docs, `pg_cast`).
fn cast_row(schema: &'static SystemRel, c: &BuiltinCast) -> Result<Row> {
    let mut b = RowBuilder::new(schema);
    b.set("oid", d::oid(c.oid))
        .set("castsource", d::oid(c.source))
        .set("casttarget", d::oid(c.target))
        .set("castfunc", d::oid(c.func))
        .set("castcontext", d::char_(cast_context_char(c.context)))
        .set("castmethod", d::char_(cast_method_char(c.method)));
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::CatalogData;
    use crate::env::CatalogEnv;
    use crate::model::common::StoredExpr;
    use crate::model::{FuncArg, FuncKind, FuncVolatility, Namespace};
    use crate::session::NullSessionInfo;
    use crate::CatalogVersion;
    use pgarbor_types::IoConfig;

    fn col(name: &str) -> usize {
        let rel = builtin::system_rel(relid::PG_PROC).expect("pg_proc schema");
        rel.columns.iter().position(|c| c.name == name).expect("pg_proc column")
    }

    fn env_with(functions: Vec<Function>) -> CatalogEnv {
        let mut data = CatalogData::default();
        // Snapshot build rejects objects whose namespace is missing (XX001), so persist the
        // bootstrap `public` schema (oid 2200, owner pg_database_owner 6171) as persist::bootstrap does.
        data.namespaces.insert(
            2200,
            Namespace { oid: 2200, name: "public".to_string(), owner: 6171, acl: None },
        );
        for f in functions {
            data.functions.insert(f.oid, f);
        }
        let snap = CatalogSnapshot::build(CatalogVersion::ZERO, data).expect("snapshot");
        CatalogEnv::with_default_path(snap)
    }

    fn scan(rel: &dyn VirtualRelation, env: &CatalogEnv, eq: Vec<(usize, Datum)>) -> Vec<Row> {
        let io = IoConfig::default();
        let ctx = ScanContext { env, session: &NullSessionInfo, io_config: &io };
        rel.scan(&ctx, &ScanRequest { eq }).expect("scan")
    }

    fn arg(name: &str, type_oid: Oid, mode: ArgMode, default: Option<&str>) -> FuncArg {
        FuncArg {
            name: name.to_string(),
            type_oid,
            mode,
            default: default.map(|s| StoredExpr { sql: s.to_string(), bound: None, depends_on: Vec::new() }),
        }
    }

    fn user_fn(oid: Oid, name: &str, args: Vec<FuncArg>) -> Function {
        Function {
            oid,
            name: name.to_string(),
            namespace: 2200,
            owner: 10,
            kind: FuncKind::Function,
            language: LANG_SQL,
            args,
            ret: 23,
            retset: false,
            volatility: FuncVolatility::Volatile,
            strict: false,
            security_definer: false,
            leakproof: false,
            parallel: b'u',
            cost: 100.0,
            rows: 0.0,
            body: "select 1".to_string(),
            sql_body: None,
            config: Vec::new(),
            acl: None,
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn builtin_abs_int4_row() {
        let env = env_with(Vec::new());
        let abs = catalog_data::procs_named("abs")
            .find(|p| p.arg_types.len() == 1 && p.arg_types[0] == 23)
            .expect("abs(int4)");
        let rows = scan(&PgProc, &env, vec![(col("oid"), Datum::Oid(abs.oid))]);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(matches!(r[col("proisstrict")], Datum::Bool(true)));
        assert!(matches!(r[col("provolatile")], Datum::Char(b'i')));
        assert!(matches!(r[col("prorettype")], Datum::Oid(23)));
        assert!(matches!(r[col("pronargs")], Datum::Int2(1)));
        assert!(matches!(r[col("proallargtypes")], Datum::Null));
        assert!(matches!(r[col("proargmodes")], Datum::Null));
    }

    #[test]
    fn builtin_row_counts_match_tables() {
        let env = env_with(Vec::new());
        assert_eq!(scan(&PgProc, &env, Vec::new()).len(), catalog_data::PROCS.len());
        assert_eq!(scan(&PgAggregate, &env, Vec::new()).len(), catalog_data::AGGREGATES.len());
        assert_eq!(scan(&PgOperator, &env, Vec::new()).len(), catalog_data::OPERATORS.len());
        assert_eq!(scan(&PgCast, &env, Vec::new()).len(), catalog_data::CASTS.len());
        assert_eq!(scan(&PgLanguage, &env, Vec::new()).len(), builtin::LANGUAGES.len());
    }

    #[test]
    fn pg_cast_source_filter_is_applied() {
        let env = env_with(Vec::new());
        let rows = scan(&PgCast, &env, vec![(1, Datum::Oid(23))]);
        assert!(rows.iter().all(|r| matches!(r[1], Datum::Oid(23))));
        assert_eq!(rows.len(), catalog_data::CASTS.iter().filter(|c| c.source == 23).count());
    }

    #[test]
    fn user_function_defaults() {
        let f = user_fn(
            16384,
            "f",
            vec![arg("a", 23, ArgMode::In, None), arg("b", 25, ArgMode::In, Some("'x'::text"))],
        );
        let env = env_with(vec![f]);
        let rows = scan(&PgProc, &env, vec![(col("oid"), Datum::Oid(16384))]);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(matches!(r[col("pronargs")], Datum::Int2(2)));
        assert!(matches!(r[col("pronargdefaults")], Datum::Int2(1)));
        assert!(matches!(&r[col("proargdefaults")], Datum::Text(s) if s == "'x'::text"));
        assert!(matches!(r[col("proallargtypes")], Datum::Null));
        assert!(matches!(r[col("proargmodes")], Datum::Null));
        assert!(matches!(r[col("prokind")], Datum::Char(b'f')));
        assert!(matches!(r[col("provolatile")], Datum::Char(b'v')));
    }

    #[test]
    fn user_function_with_out_param_fills_all_arg_columns() {
        let f = user_fn(
            16385,
            "g",
            vec![arg("a", 23, ArgMode::In, None), arg("b", 23, ArgMode::Out, None)],
        );
        let env = env_with(vec![f]);
        let rows = scan(&PgProc, &env, vec![(col("oid"), Datum::Oid(16385))]);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(!matches!(r[col("proallargtypes")], Datum::Null));
        assert!(!matches!(r[col("proargmodes")], Datum::Null));
        assert!(!matches!(r[col("proargnames")], Datum::Null));
        assert!(matches!(r[col("pronargs")], Datum::Int2(1)));
        assert!(matches!(r[col("proargdefaults")], Datum::Null));
    }

    #[test]
    fn user_function_name_filter_excludes_others() {
        let env = env_with(vec![user_fn(16384, "f", Vec::new()), user_fn(16385, "h", Vec::new())]);
        let rows = scan(&PgProc, &env, vec![(col("proname"), Datum::Text("h".to_string()))]);
        let user_rows: Vec<&Row> = rows.iter().filter(|r| matches!(r[col("oid")], Datum::Oid(o) if o >= 16384)).collect();
        assert_eq!(user_rows.len(), 1);
        assert!(matches!(user_rows[0][col("oid")], Datum::Oid(16385)));
    }

    #[test]
    fn pg_language_has_four_rows() {
        let env = env_with(Vec::new());
        assert_eq!(scan(&PgLanguage, &env, Vec::new()).len(), 4);
    }
}
