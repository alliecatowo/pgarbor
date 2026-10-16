//! User rows of `pg_operator`, `pg_aggregate`, `pg_opclass`, `pg_opfamily`, `pg_cast`,
//! `pg_collation`, `pg_extension`, and the full `pg_amop`, `pg_amproc`, `pg_conversion`
//! producers (builtin rows from `builtin::ext_rows`, generated from the PG 18.6 oracle; user rows
//! from `ExtObject`s). Amendment pgarbor-catalog-09. The producers in `pg_proc.rs` /
//! `pg_builtin.rs` append the `*_user_rows` of this file after their builtin rows (ascending OID
//! because user OIDs are >= 16384).
use super::rowbuild::{d, RowBuilder};
use super::{Row, ScanContext, ScanRequest, VirtualRelation};
use crate::builtin::{self, relid, SystemRel};
use crate::model::*;
use pgarbor_common::{Oid, Result};
use pgarbor_types::Datum;

fn text_eq<'a>(req: &'a ScanRequest, idx: usize) -> Option<&'a str> {
    match req.eq_on(idx) {
        Some(Datum::Text(t)) => Some(t.as_str()),
        _ => None,
    }
}

/// `pg_operator` rows of user operators.
pub fn operator_user_rows(ctx: &ScanContext, schema: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let oid = req.oid_eq_on(0);
    let name = text_eq(req, 1);
    let mut rows = Vec::new();
    for o in ctx.env.snapshot().user_operators() {
        if oid.is_some_and(|x| x != o.oid) || name.is_some_and(|n| n != o.name) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(o.oid))
            .set("oprname", d::name(&o.name))
            .set("oprnamespace", d::oid(o.namespace))
            .set("oprowner", d::oid(o.owner))
            .set("oprkind", d::char_(o.kind))
            .set("oprcanmerge", d::bool_(o.can_merge))
            .set("oprcanhash", d::bool_(o.can_hash))
            .set("oprleft", d::oid(o.left))
            .set("oprright", d::oid(o.right))
            .set("oprresult", d::oid(o.result))
            .set("oprcom", d::oid(o.commutator))
            .set("oprnegate", d::oid(o.negator))
            .set("oprcode", d::oid(o.proc))
            .set("oprrest", d::oid(o.restrict))
            .set("oprjoin", d::oid(o.join));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_aggregate` rows of user aggregates.
pub fn aggregate_user_rows(ctx: &ScanContext, schema: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let oid = req.oid_eq_on(0);
    let mut rows = Vec::new();
    for a in ctx.env.snapshot().user_aggregates() {
        if oid.is_some_and(|x| x != a.fnoid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("aggfnoid", d::oid(a.fnoid))
            .set("aggkind", d::char_(a.kind))
            .set("aggnumdirectargs", d::int2(a.num_direct_args))
            .set("aggtransfn", d::oid(a.transfn))
            .set("aggfinalfn", d::oid(a.finalfn))
            .set("aggcombinefn", d::oid(a.combinefn))
            .set("aggserialfn", d::oid(a.serialfn))
            .set("aggdeserialfn", d::oid(a.deserialfn))
            .set("aggmtransfn", d::oid(a.mtransfn))
            .set("aggminvtransfn", d::oid(a.minvtransfn))
            .set("aggmfinalfn", d::oid(a.mfinalfn))
            .set("aggfinalextra", d::bool_(a.finalextra))
            .set("aggmfinalextra", d::bool_(a.mfinalextra))
            .set("aggfinalmodify", d::char_(a.finalmodify))
            .set("aggmfinalmodify", d::char_(a.mfinalmodify))
            .set("aggsortop", d::oid(a.sortop))
            .set("aggtranstype", d::oid(a.transtype))
            .set("aggtransspace", d::int4(a.transspace))
            .set("aggmtranstype", d::oid(a.mtranstype))
            .set("aggmtransspace", d::int4(a.mtransspace))
            .set("agginitval", d::opt_text(a.initval.as_deref()))
            .set("aggminitval", d::opt_text(a.minitval.as_deref()));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_opclass` rows of user operator classes.
pub fn opclass_user_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let schema = builtin::system_rel(relid::PG_OPCLASS)
        .ok_or_else(|| pgarbor_common::PgError::internal("pg_opclass has no system relation entry"))?;
    let oid = req.oid_eq_on(0);
    let mut rows = Vec::new();
    for c in ctx.env.snapshot().user_opclasses() {
        if oid.is_some_and(|x| x != c.oid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(c.oid))
            .set("opcmethod", d::oid(c.method))
            .set("opcname", d::name(&c.name))
            .set("opcnamespace", d::oid(c.namespace))
            .set("opcowner", d::oid(c.owner))
            .set("opcfamily", d::oid(c.family))
            .set("opcintype", d::oid(c.input_type))
            .set("opcdefault", d::bool_(c.is_default))
            .set("opckeytype", d::oid(c.key_type));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_opfamily` rows of user operator families.
pub fn opfamily_user_rows(ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
    let schema = builtin::system_rel(relid::PG_OPFAMILY)
        .ok_or_else(|| pgarbor_common::PgError::internal("pg_opfamily has no system relation entry"))?;
    let oid = req.oid_eq_on(0);
    let mut rows = Vec::new();
    for f in ctx.env.snapshot().user_opfamilies() {
        if oid.is_some_and(|x| x != f.oid) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(f.oid))
            .set("opfmethod", d::oid(f.method))
            .set("opfname", d::name(&f.name))
            .set("opfnamespace", d::oid(f.namespace))
            .set("opfowner", d::oid(f.owner));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_cast` rows of user casts.
pub fn cast_user_rows(ctx: &ScanContext, schema: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let oid = req.oid_eq_on(0);
    let source = req.oid_eq_on(1);
    let target = req.oid_eq_on(2);
    let mut rows = Vec::new();
    for c in ctx.env.snapshot().user_casts() {
        if oid.is_some_and(|x| x != c.oid) || source.is_some_and(|x| x != c.source) || target.is_some_and(|x| x != c.target)
        {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(c.oid))
            .set("castsource", d::oid(c.source))
            .set("casttarget", d::oid(c.target))
            .set("castfunc", d::oid(c.func))
            .set("castcontext", d::char_(c.context))
            .set("castmethod", d::char_(c.method));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_collation` rows of user collations.
pub fn collation_user_rows(ctx: &ScanContext, schema: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let oid = req.oid_eq_on(0);
    let name = text_eq(req, 1);
    let mut rows = Vec::new();
    for c in ctx.env.snapshot().user_collations() {
        if oid.is_some_and(|x| x != c.oid) || name.is_some_and(|n| n != c.name) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(c.oid))
            .set("collname", d::name(&c.name))
            .set("collnamespace", d::oid(c.namespace))
            .set("collowner", d::oid(c.owner))
            .set("collprovider", d::char_(c.provider))
            .set("collisdeterministic", d::bool_(c.deterministic))
            .set("collencoding", d::int4(c.encoding))
            .set("collcollate", d::opt_text(c.collate.as_deref()))
            .set("collctype", d::opt_text(c.ctype.as_deref()))
            .set("colllocale", d::opt_text(c.locale.as_deref()))
            .set("collicurules", d::opt_text(c.icu_rules.as_deref()))
            .set("collversion", d::opt_text(c.version.as_deref()));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// `pg_extension` rows of user extensions.
pub fn extension_user_rows(ctx: &ScanContext, schema: &'static SystemRel, req: &ScanRequest) -> Result<Vec<Row>> {
    let oid = req.oid_eq_on(0);
    let name = text_eq(req, 1);
    let mut rows = Vec::new();
    for e in ctx.env.snapshot().user_extensions() {
        if oid.is_some_and(|x| x != e.oid) || name.is_some_and(|n| n != e.name) {
            continue;
        }
        let mut b = RowBuilder::new(schema);
        b.set("oid", d::oid(e.oid))
            .set("extname", d::name(&e.name))
            .set("extowner", d::oid(e.owner))
            .set("extnamespace", d::oid(e.namespace))
            .set("extrelocatable", d::bool_(e.relocatable))
            .set("extversion", d::text(&e.version));
        rows.push(b.finish()?);
    }
    Ok(rows)
}

/// Producer for `pg_amop` (builtin + user rows, ordered by OID).
#[derive(Clone, Copy, Debug)]
pub struct PgAmop;

impl VirtualRelation for PgAmop {
    fn oid(&self) -> Oid {
        relid::PG_AMOP
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let family = req.oid_eq_on(1);
        let mut rows = Vec::new();
        let mut push = |o: Oid, fam: Oid, l: Oid, r: Oid, strat: i16, purpose: u8, opr: Oid, method: Oid, sortfam: Oid| -> Result<()> {
            if oid.is_some_and(|x| x != o) || family.is_some_and(|x| x != fam) {
                return Ok(());
            }
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(o))
                .set("amopfamily", d::oid(fam))
                .set("amoplefttype", d::oid(l))
                .set("amoprighttype", d::oid(r))
                .set("amopstrategy", d::int2(strat))
                .set("amoppurpose", d::char_(purpose))
                .set("amopopr", d::oid(opr))
                .set("amopmethod", d::oid(method))
                .set("amopsortfamily", d::oid(sortfam));
            rows.push(b.finish()?);
            Ok(())
        };
        for r in builtin::ext_rows::AMOPS {
            push(r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8)?;
        }
        let mut user: Vec<(Oid, Oid, AmOpEntry)> = Vec::new();
        for f in ctx.env.snapshot().user_opfamilies() {
            for m in &f.operators {
                user.push((f.oid, f.method, m.clone()));
            }
        }
        user.sort_by_key(|u| u.2.oid);
        for (fam, method, m) in user {
            push(m.oid, fam, m.lefttype, m.righttype, m.strategy, m.purpose, m.operator, method, m.sortfamily)?;
        }
        Ok(rows)
    }
}

/// Producer for `pg_amproc` (builtin + user rows, ordered by OID).
#[derive(Clone, Copy, Debug)]
pub struct PgAmproc;

impl VirtualRelation for PgAmproc {
    fn oid(&self) -> Oid {
        relid::PG_AMPROC
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let family = req.oid_eq_on(1);
        let mut rows = Vec::new();
        let mut push = |o: Oid, fam: Oid, l: Oid, r: Oid, num: i16, proc: Oid| -> Result<()> {
            if oid.is_some_and(|x| x != o) || family.is_some_and(|x| x != fam) {
                return Ok(());
            }
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(o))
                .set("amprocfamily", d::oid(fam))
                .set("amproclefttype", d::oid(l))
                .set("amprocrighttype", d::oid(r))
                .set("amprocnum", d::int2(num))
                .set("amproc", d::oid(proc));
            rows.push(b.finish()?);
            Ok(())
        };
        for r in builtin::ext_rows::AMPROCS {
            push(r.0, r.1, r.2, r.3, r.4, r.5)?;
        }
        let mut user: Vec<(Oid, AmProcEntry)> = Vec::new();
        for f in ctx.env.snapshot().user_opfamilies() {
            for m in &f.procs {
                user.push((f.oid, m.clone()));
            }
        }
        user.sort_by_key(|u| u.1.oid);
        for (fam, m) in user {
            push(m.oid, fam, m.lefttype, m.righttype, m.procnum, m.proc)?;
        }
        Ok(rows)
    }
}

/// Producer for `pg_conversion` (builtin + user rows).
#[derive(Clone, Copy, Debug)]
pub struct PgConversion;

impl VirtualRelation for PgConversion {
    fn oid(&self) -> Oid {
        relid::PG_CONVERSION
    }
    fn scan(&self, ctx: &ScanContext, req: &ScanRequest) -> Result<Vec<Row>> {
        let schema = self.schema()?;
        let oid = req.oid_eq_on(0);
        let name = text_eq(req, 1);
        let mut rows = Vec::new();
        for c in ctx.env.all_conversions() {
            if oid.is_some_and(|x| x != c.oid) || name.is_some_and(|n| n != c.name) {
                continue;
            }
            let mut b = RowBuilder::new(schema);
            b.set("oid", d::oid(c.oid))
                .set("conname", d::name(&c.name))
                .set("connamespace", d::oid(c.namespace))
                .set("conowner", d::oid(c.owner))
                .set("conforencoding", d::int4(c.for_encoding))
                .set("contoencoding", d::int4(c.to_encoding))
                .set("conproc", d::oid(c.proc))
                .set("condefault", d::bool_(c.is_default));
            rows.push(b.finish()?);
        }
        Ok(rows)
    }
}

/// Registry slice.
pub static RELATIONS: &[&dyn VirtualRelation] = &[&PgAmop, &PgAmproc, &PgConversion];
