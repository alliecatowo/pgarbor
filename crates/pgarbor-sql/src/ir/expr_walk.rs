//! Generic traversal and rewriting of [`BoundExpr`] trees. Fully implemented, frozen (v1).
//!
//! These helpers are the ONLY place that enumerates the children of every `ExprKind` variant, so
//! adding a variant breaks the build here (no `_ =>` arms) instead of silently skipping children.
//!
//! Child enumeration order is deterministic: arguments left to right; `Case` visits `arg`, then each
//! arm's `cond` then `result`, then `default`; `Aggref` visits `args`, `direct_args`, `filter`, then
//! each `order_by[i].expr`.
//!
//! **Sub-queries are opaque**: a `SubLink`'s `subquery` is NOT entered by any helper in this file
//! (its `test` expression is). Planner code that needs correlation analysis walks the subquery
//! explicitly and adjusts `levels_up` itself.
use super::expr::*;
use pgarbor_common::Result;
use pgarbor_types::catalog_data::Volatility;

impl BoundExpr {
    /// Call `f` on every direct child expression.
    pub fn for_each_child<'a>(&'a self, f: &mut dyn FnMut(&'a BoundExpr)) {
        match &self.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::Column(_)
            | ExprKind::InputRef(_)
            | ExprKind::OuterRef(_)
            | ExprKind::CaseTestValue
            | ExprKind::SqlValue(_)
            | ExprKind::Grouping(_)
            | ExprKind::SetToDefault { .. } => {}
            ExprKind::FuncCall(e) => e.args.iter().for_each(f),
            ExprKind::Op(e) => e.args.iter().for_each(f),
            ExprKind::Bool(e) => e.args.iter().for_each(f),
            ExprKind::NullTest(e) => f(&e.arg),
            ExprKind::BooleanTest(e) => f(&e.arg),
            ExprKind::DistinctFrom(e) => {
                f(&e.left);
                f(&e.right);
            }
            ExprKind::Case(e) => {
                if let Some(a) = &e.arg {
                    f(a);
                }
                for arm in &e.arms {
                    f(&arm.cond);
                    f(&arm.result);
                }
                if let Some(d) = &e.default {
                    f(d);
                }
            }
            ExprKind::Coalesce(args) => args.iter().for_each(f),
            ExprKind::NullIf(e) => {
                f(&e.left);
                f(&e.right);
            }
            ExprKind::MinMax(e) => e.args.iter().for_each(f),
            ExprKind::Cast(e) => f(&e.arg),
            ExprKind::CoerceToDomain(e) => f(&e.arg),
            ExprKind::Collate(e) => f(&e.arg),
            ExprKind::Array(e) => e.elements.iter().for_each(f),
            ExprKind::Row(e) => e.args.iter().for_each(f),
            ExprKind::FieldSelect(e) => f(&e.arg),
            ExprKind::Subscript(e) => {
                f(&e.array);
                for s in &e.subscripts {
                    if let Some(l) = &s.lower {
                        f(l);
                    }
                    if let Some(u) = &s.upper {
                        f(u);
                    }
                }
            }
            ExprKind::ScalarArrayOp(e) => {
                f(&e.left);
                f(&e.right);
            }
            ExprKind::RowCompare(e) => {
                e.left.iter().for_each(&mut *f);
                e.right.iter().for_each(&mut *f);
            }
            ExprKind::SubLink(e) => {
                if let Some(t) = &e.test {
                    f(t);
                }
            }
            ExprKind::SubPlan(e) => {
                if let Some(t) = &e.test {
                    f(t);
                }
                e.args.iter().for_each(f);
            }
            ExprKind::Aggref(e) => {
                e.args.iter().for_each(&mut *f);
                e.direct_args.iter().for_each(&mut *f);
                if let Some(fl) = &e.filter {
                    f(fl);
                }
                for o in &e.order_by {
                    f(&o.expr);
                }
            }
            ExprKind::WindowFunc(e) => {
                e.args.iter().for_each(&mut *f);
                if let Some(fl) = &e.filter {
                    f(fl);
                }
            }
        }
    }

    /// Call `f` on every direct child expression, mutably.
    pub fn for_each_child_mut(&mut self, f: &mut dyn FnMut(&mut BoundExpr)) {
        match &mut self.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::Column(_)
            | ExprKind::InputRef(_)
            | ExprKind::OuterRef(_)
            | ExprKind::CaseTestValue
            | ExprKind::SqlValue(_)
            | ExprKind::Grouping(_)
            | ExprKind::SetToDefault { .. } => {}
            ExprKind::FuncCall(e) => e.args.iter_mut().for_each(f),
            ExprKind::Op(e) => e.args.iter_mut().for_each(f),
            ExprKind::Bool(e) => e.args.iter_mut().for_each(f),
            ExprKind::NullTest(e) => f(&mut e.arg),
            ExprKind::BooleanTest(e) => f(&mut e.arg),
            ExprKind::DistinctFrom(e) => {
                f(&mut e.left);
                f(&mut e.right);
            }
            ExprKind::Case(e) => {
                if let Some(a) = &mut e.arg {
                    f(a);
                }
                for arm in &mut e.arms {
                    f(&mut arm.cond);
                    f(&mut arm.result);
                }
                if let Some(d) = &mut e.default {
                    f(d);
                }
            }
            ExprKind::Coalesce(args) => args.iter_mut().for_each(f),
            ExprKind::NullIf(e) => {
                f(&mut e.left);
                f(&mut e.right);
            }
            ExprKind::MinMax(e) => e.args.iter_mut().for_each(f),
            ExprKind::Cast(e) => f(&mut e.arg),
            ExprKind::CoerceToDomain(e) => f(&mut e.arg),
            ExprKind::Collate(e) => f(&mut e.arg),
            ExprKind::Array(e) => e.elements.iter_mut().for_each(f),
            ExprKind::Row(e) => e.args.iter_mut().for_each(f),
            ExprKind::FieldSelect(e) => f(&mut e.arg),
            ExprKind::Subscript(e) => {
                f(&mut e.array);
                for s in &mut e.subscripts {
                    if let Some(l) = &mut s.lower {
                        f(l);
                    }
                    if let Some(u) = &mut s.upper {
                        f(u);
                    }
                }
            }
            ExprKind::ScalarArrayOp(e) => {
                f(&mut e.left);
                f(&mut e.right);
            }
            ExprKind::RowCompare(e) => {
                e.left.iter_mut().for_each(&mut *f);
                e.right.iter_mut().for_each(&mut *f);
            }
            ExprKind::SubLink(e) => {
                if let Some(t) = &mut e.test {
                    f(t);
                }
            }
            ExprKind::SubPlan(e) => {
                if let Some(t) = &mut e.test {
                    f(t);
                }
                e.args.iter_mut().for_each(f);
            }
            ExprKind::Aggref(e) => {
                e.args.iter_mut().for_each(&mut *f);
                e.direct_args.iter_mut().for_each(&mut *f);
                if let Some(fl) = &mut e.filter {
                    f(fl);
                }
                for o in &mut e.order_by {
                    f(&mut o.expr);
                }
            }
            ExprKind::WindowFunc(e) => {
                e.args.iter_mut().for_each(&mut *f);
                if let Some(fl) = &mut e.filter {
                    f(fl);
                }
            }
        }
    }

    /// Pre-order walk of the whole tree (sub-queries opaque). `f` returns `false` to skip the
    /// children of the visited node.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a BoundExpr) -> bool) {
        if f(self) {
            self.for_each_child(&mut |c| c.walk(&mut *f));
        }
    }

    /// True when `pred` holds for this node or any descendant.
    pub fn any(&self, pred: &mut dyn FnMut(&BoundExpr) -> bool) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if found {
                return false;
            }
            if pred(e) {
                found = true;
                return false;
            }
            true
        });
        found
    }

    /// Rebuild the node with every direct child replaced by `f(child)` (children are NOT recursed
    /// into; see [`BoundExpr::transform`]). Sub-queries are untouched.
    pub fn map_children(self, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<BoundExpr> {
        let BoundExpr { kind, ty } = self;
        let kind = match kind {
            k @ (ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::Column(_)
            | ExprKind::InputRef(_)
            | ExprKind::OuterRef(_)
            | ExprKind::CaseTestValue
            | ExprKind::SqlValue(_)
            | ExprKind::Grouping(_)
            | ExprKind::SetToDefault { .. }) => k,
            ExprKind::FuncCall(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::FuncCall(e)
            }
            ExprKind::Op(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::Op(e)
            }
            ExprKind::Bool(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::Bool(e)
            }
            ExprKind::NullTest(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::NullTest(e)
            }
            ExprKind::BooleanTest(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::BooleanTest(e)
            }
            ExprKind::DistinctFrom(mut e) => {
                e.left = map_one(std::mem::replace(&mut e.left, placeholder()), f)?;
                e.right = map_one(std::mem::replace(&mut e.right, placeholder()), f)?;
                ExprKind::DistinctFrom(e)
            }
            ExprKind::Case(mut e) => {
                if let Some(a) = e.arg.take() {
                    e.arg = Some(Box::new(f(*a)?));
                }
                let arms = std::mem::take(&mut e.arms);
                let mut out = Vec::with_capacity(arms.len());
                for arm in arms {
                    out.push(CaseArm { cond: f(arm.cond)?, result: f(arm.result)? });
                }
                e.arms = out;
                if let Some(d) = e.default.take() {
                    e.default = Some(Box::new(f(*d)?));
                }
                ExprKind::Case(e)
            }
            ExprKind::Coalesce(args) => ExprKind::Coalesce(map_vec(args, f)?),
            ExprKind::NullIf(mut e) => {
                e.left = map_one(std::mem::replace(&mut e.left, placeholder()), f)?;
                e.right = map_one(std::mem::replace(&mut e.right, placeholder()), f)?;
                ExprKind::NullIf(e)
            }
            ExprKind::MinMax(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::MinMax(e)
            }
            ExprKind::Cast(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::Cast(e)
            }
            ExprKind::CoerceToDomain(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::CoerceToDomain(e)
            }
            ExprKind::Collate(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::Collate(e)
            }
            ExprKind::Array(mut e) => {
                e.elements = map_vec(std::mem::take(&mut e.elements), f)?;
                ExprKind::Array(e)
            }
            ExprKind::Row(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::Row(e)
            }
            ExprKind::FieldSelect(mut e) => {
                e.arg = map_one(std::mem::replace(&mut e.arg, placeholder()), f)?;
                ExprKind::FieldSelect(e)
            }
            ExprKind::Subscript(mut e) => {
                e.array = map_one(std::mem::replace(&mut e.array, placeholder()), f)?;
                let subs = std::mem::take(&mut e.subscripts);
                let mut out = Vec::with_capacity(subs.len());
                for s in subs {
                    let lower = match s.lower {
                        Some(l) => Some(f(l)?),
                        None => None,
                    };
                    let upper = match s.upper {
                        Some(u) => Some(f(u)?),
                        None => None,
                    };
                    out.push(SubscriptItem { lower, upper });
                }
                e.subscripts = out;
                ExprKind::Subscript(e)
            }
            ExprKind::ScalarArrayOp(mut e) => {
                e.left = map_one(std::mem::replace(&mut e.left, placeholder()), f)?;
                e.right = map_one(std::mem::replace(&mut e.right, placeholder()), f)?;
                ExprKind::ScalarArrayOp(e)
            }
            ExprKind::RowCompare(mut e) => {
                e.left = map_vec(std::mem::take(&mut e.left), f)?;
                e.right = map_vec(std::mem::take(&mut e.right), f)?;
                ExprKind::RowCompare(e)
            }
            ExprKind::SubLink(mut e) => {
                if let Some(t) = e.test.take() {
                    e.test = Some(Box::new(f(*t)?));
                }
                ExprKind::SubLink(e)
            }
            ExprKind::SubPlan(mut e) => {
                if let Some(t) = e.test.take() {
                    e.test = Some(Box::new(f(*t)?));
                }
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                ExprKind::SubPlan(e)
            }
            ExprKind::Aggref(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                e.direct_args = map_vec(std::mem::take(&mut e.direct_args), f)?;
                if let Some(fl) = e.filter.take() {
                    e.filter = Some(Box::new(f(*fl)?));
                }
                let ob = std::mem::take(&mut e.order_by);
                let mut out = Vec::with_capacity(ob.len());
                for mut o in ob {
                    o.expr = f(o.expr)?;
                    out.push(o);
                }
                e.order_by = out;
                ExprKind::Aggref(e)
            }
            ExprKind::WindowFunc(mut e) => {
                e.args = map_vec(std::mem::take(&mut e.args), f)?;
                if let Some(fl) = e.filter.take() {
                    e.filter = Some(Box::new(f(*fl)?));
                }
                ExprKind::WindowFunc(e)
            }
        };
        Ok(BoundExpr { kind, ty })
    }

    /// Bottom-up rewrite: children are transformed first, then `f` is applied to the rebuilt node.
    pub fn transform(self, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<BoundExpr> {
        let rebuilt = self.map_children(&mut |c| c.transform(&mut *f))?;
        f(rebuilt)
    }

    /// Top-down rewrite: `f` is applied first; when it returns `Ok((expr, true))` the children of
    /// the result are rewritten recursively, `(expr, false)` stops descending.
    pub fn transform_down(
        self,
        f: &mut dyn FnMut(BoundExpr) -> Result<(BoundExpr, bool)>,
    ) -> Result<BoundExpr> {
        let (e, descend) = f(self)?;
        if descend {
            e.map_children(&mut |c| c.transform_down(&mut *f))
        } else {
            Ok(e)
        }
    }

    /// Contains an `Aggref` that is evaluated at this query level (`levels_up == 0`).
    pub fn contains_local_agg(&self) -> bool {
        self.any(&mut |e| matches!(&e.kind, ExprKind::Aggref(a) if a.levels_up == 0))
    }
    /// Contains any `WindowFunc`.
    pub fn contains_window(&self) -> bool {
        self.any(&mut |e| matches!(e.kind, ExprKind::WindowFunc(_)))
    }
    /// Contains a `SubLink` or `SubPlan`.
    pub fn contains_sublink(&self) -> bool {
        self.any(&mut |e| matches!(e.kind, ExprKind::SubLink(_) | ExprKind::SubPlan(_)))
    }
    /// Contains a set-returning function call.
    pub fn contains_srf(&self) -> bool {
        self.any(&mut |e| matches!(&e.kind, ExprKind::FuncCall(f) if f.retset))
    }
    /// Contains a `Column` with `levels_up == 0` (a variable of the current query level).
    pub fn contains_local_column(&self) -> bool {
        self.any(&mut |e| matches!(&e.kind, ExprKind::Column(c) if c.levels_up == 0))
    }
    /// Contains a `Param`.
    pub fn contains_param(&self) -> bool {
        self.any(&mut |e| matches!(e.kind, ExprKind::Param(_)))
    }
    /// Worst volatility of any function/operator in the tree (`Immutable` for pure trees).
    /// SQL value functions are STABLE; sub-links are treated as VOLATILE (sub-queries are opaque).
    pub fn volatility(&self) -> Volatility {
        let mut worst = Volatility::Immutable;
        self.walk(&mut |e| {
            let v = match &e.kind {
                ExprKind::FuncCall(f) => f.volatility,
                ExprKind::Op(o) => o.op.volatility,
                ExprKind::DistinctFrom(d) => d.eq.volatility,
                ExprKind::NullIf(n) => n.eq.volatility,
                ExprKind::ScalarArrayOp(s) => s.op.volatility,
                ExprKind::SqlValue(_) => Volatility::Stable,
                ExprKind::SubLink(_) | ExprKind::SubPlan(_) => Volatility::Volatile,
                _ => Volatility::Immutable,
            };
            if v > worst {
                worst = v;
            }
            true
        });
        worst
    }
    /// All `Column` references at `levels_up == level` found in the tree (sub-queries opaque).
    pub fn collect_columns(&self, level: u32, out: &mut Vec<ColumnRef>) {
        self.walk(&mut |e| {
            if let ExprKind::Column(c) = &e.kind {
                if c.levels_up == level {
                    out.push(*c);
                }
            }
            true
        });
    }
}

fn placeholder() -> BoundExpr {
    BoundExpr::bool_const(false)
}

fn map_one(e: BoundExpr, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<BoundExpr> {
    f(e)
}

fn map_vec(v: Vec<BoundExpr>, f: &mut dyn FnMut(BoundExpr) -> Result<BoundExpr>) -> Result<Vec<BoundExpr>> {
    let mut out = Vec::with_capacity(v.len());
    for e in v {
        out.push(f(e)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_types::{Datum, TypeId};

    fn int(v: i32) -> BoundExpr {
        BoundExpr::constant(Datum::Int4(v), TypeId::INT4)
    }

    #[test]
    fn walk_and_transform() {
        let e = BoundExpr::and_all(vec![BoundExpr::bool_const(true), BoundExpr::not(BoundExpr::bool_const(false))]);
        let mut n = 0;
        e.walk(&mut |_| {
            n += 1;
            true
        });
        assert_eq!(n, 4);
        let t = e
            .transform(&mut |x| Ok(if x.is_true_const() { int(1) } else { x }))
            .unwrap();
        assert!(t.any(&mut |x| x.as_const() == Some(&Datum::Int4(1))));
    }

    #[test]
    fn volatility_pure() {
        assert_eq!(int(1).volatility(), Volatility::Immutable);
    }
}
