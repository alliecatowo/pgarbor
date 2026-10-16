//! CREATE TABLE hierarchy clauses (amendment pgarbor-sql-06): `INHERITS (...)`, `PARTITION BY
//! strategy (...)`, `PARTITION OF parent {FOR VALUES ... | DEFAULT}` and `ON COMMIT`. PG references:
//! `parse_utilcmd.c` (`transformPartitionBound`, `transformPartitionBoundValue`,
//! `transformPartitionRangeBounds`, `validateInfiniteBounds`), `tablecmds.c`
//! (`transformPartitionSpec`, `DefineRelation`).
//!
//! The binder resolves parents and validates the bound against the parent's key; column merging,
//! overlap checks and constraint inheritance are the catalog's job (`ddl::inherit`,
//! `ddl::partitioned`). Child expressions are bound over `local columns ++ inherited-only columns`
//! (partitions: the parent's visible columns), see `pgarbor_catalog::ddl::inherit`.
use pgarbor_catalog::ddl::spec::{
    ColumnSpec, OnCommitAction, PartitionKeyItemSpec, PartitionKeySpec, PartitionOfSpec,
};
use pgarbor_catalog::model::{RelKind, Relation};
use pgarbor_catalog::partition::{self, BoundPositions, PartitionBound, RangeValue};
use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_types::context::IoConfig;
use pgarbor_types::{io, Datum, IoCtx, TypeEnv, TypeId};
use sqlparser::ast::{
    CreateTable, Expr, ForValues, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName, OnCommit,
    PartitionBoundValue,
};

use super::coerce::coerce_assignment;
use super::context::{Binder, BoundArg, ExprCtx};
use super::stored;
use crate::diag::{self, ExprClause};
use crate::ir::ExprKind;

/// Parents of the table being created.
pub(super) struct Hierarchy<'r> {
    /// `INHERITS` parents in written order.
    pub inherits: Vec<&'r Relation>,
    /// `PARTITION OF` parent.
    pub partition_parent: Option<&'r Relation>,
}

impl<'r> Hierarchy<'r> {
    pub fn parent_oids(&self) -> Vec<Oid> {
        self.inherits.iter().map(|r| r.oid).collect()
    }

    /// Column scope for binding the child's expressions (module docs).
    pub fn scope(&self, local: &[ColumnSpec]) -> Vec<(String, TypeId)> {
        if let Some(p) = self.partition_parent {
            return p.visible_columns().map(|c| (c.name.clone(), c.type_id)).collect();
        }
        let mut out: Vec<(String, TypeId)> = local.iter().map(|c| (c.name.clone(), c.type_id)).collect();
        let mut inherited: Vec<(String, TypeId)> = Vec::new();
        for p in &self.inherits {
            for c in p.visible_columns() {
                if !inherited.iter().any(|(n, _)| n == &c.name) {
                    inherited.push((c.name.clone(), c.type_id));
                }
            }
        }
        for (n, t) in inherited {
            if !out.iter().any(|(o, _)| o == &n) {
                out.push((n, t));
            }
        }
        out
    }
}

fn at(e: PgError, pos: Option<u32>) -> PgError {
    diag::with_position(e, pos)
}

/// Resolve `INHERITS` parents and the `PARTITION OF` parent.
pub(super) fn bind_hierarchy<'r>(b: &mut Binder<'r>, ct: &CreateTable) -> Result<Hierarchy<'r>> {
    let env = b.env;
    let mut inherits = Vec::new();
    if let Some(names) = &ct.inherits {
        for name in names {
            let qn = b.qual(name)?;
            let rel = env.resolve_relation(qn.schema.as_deref(), &qn.name).map_err(|e| at(e, qn.pos))?;
            inherits.push(rel);
        }
    }
    let partition_parent = match &ct.partition_of {
        Some(name) => {
            let qn = b.qual(name)?;
            let rel = env.resolve_relation(qn.schema.as_deref(), &qn.name).map_err(|e| at(e, qn.pos))?;
            if rel.kind != RelKind::PartitionedTable {
                let e = if rel.kind == RelKind::Table {
                    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not partitioned", rel.name))
                } else {
                    PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", rel.name))
                };
                return Err(e);
            }
            Some(rel)
        }
        None => None,
    };
    Ok(Hierarchy { inherits, partition_parent })
}

/// Type of a `PARTITION OF` column entry (no type written): the parent's column type; 42703 when
/// the parent has no such column.
pub(super) fn partition_column_type(parent: &Relation, name: &str) -> Result<TypeId> {
    parent
        .column_by_name(name)
        .map(|c| c.type_id)
        .ok_or_else(|| PgError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{name}\" does not exist")))
}

/// `ON COMMIT` (42P16 on a permanent table).
pub(super) fn bind_on_commit(ct: &CreateTable, temporary: bool) -> Result<OnCommitAction> {
    let action = match ct.on_commit {
        None => return Ok(OnCommitAction::Noop),
        Some(OnCommit::DeleteRows) => OnCommitAction::DeleteRows,
        Some(OnCommit::PreserveRows) => OnCommitAction::PreserveRows,
        Some(OnCommit::Drop) => OnCommitAction::Drop,
    };
    if !temporary {
        return Err(PgError::new(SqlState::INVALID_TABLE_DEFINITION, "ON COMMIT can only be used on temporary tables"));
    }
    Ok(action)
}

/// Expression context of partition key expressions (PG `EXPR_KIND_PARTITION_EXPRESSION`).
fn partition_expr_ctx() -> ExprCtx {
    ExprCtx {
        clause: ExprClause::Other("partition key expressions"),
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: false,
        columns_ok: true,
    }
}

/// Expression context of partition bound values (PG `EXPR_KIND_PARTITION_BOUND`).
fn partition_bound_ctx() -> ExprCtx {
    ExprCtx {
        clause: ExprClause::Other("partition bound"),
        aggs_ok: false,
        windows_ok: false,
        srf_ok: false,
        sublinks_ok: false,
        columns_ok: false,
    }
}

/// `PARTITION BY RANGE|LIST|HASH (item, ...)`, bound inside the table expression scope (`scope`
/// gives column names by binder position).
pub(super) fn bind_partition_by(b: &mut Binder, expr: &Expr, scope: &[(String, TypeId)]) -> Result<PartitionKeySpec> {
    let Expr::Function(f) = expr else {
        return Err(diag::syntax_error_near(&expr.to_string(), None));
    };
    let strategy_name = f.name.to_string().to_ascii_lowercase();
    let strategy = match strategy_name.as_str() {
        "range" => b'r',
        "list" => b'l',
        "hash" => b'h',
        other => {
            return Err(PgError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized partitioning strategy \"{other}\""),
            ))
        }
    };
    let args: Vec<&Expr> = match &f.args {
        FunctionArguments::List(list) => {
            let mut v = Vec::with_capacity(list.args.len());
            for a in &list.args {
                match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => v.push(e),
                    _ => return Err(diag::syntax_error_near(&a.to_string(), None)),
                }
            }
            v
        }
        _ => return Err(diag::syntax_error_near(&f.to_string(), None)),
    };
    if strategy == b'l' && args.len() != 1 {
        return Err(PgError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "cannot use \"list\" partition strategy with more than one column",
        ));
    }
    let names = |attnum: i16| -> Option<String> { scope.get((attnum - 1) as usize).map(|(n, _)| n.clone()) };
    let mut items = Vec::with_capacity(args.len());
    for a in args {
        // A plain column name (possibly parenthesized) is a column key.
        let mut inner = a;
        while let Expr::Nested(e) = inner {
            inner = e;
        }
        if let Expr::Identifier(id) = inner {
            items.push(PartitionKeyItemSpec { column: Some(b.ident(id)), expr: None, expr_type: None });
            continue;
        }
        let mut refs: Vec<i16> = Vec::new();
        let (bound, stored_expr) = stored::bind_ddl_expr_with_source(b, a, partition_expr_ctx(), None, None, &mut refs, &names)?;
        if let ExprKind::Column(c) = &bound.kind {
            if let Some((n, _)) = scope.get((c.attnum - 1) as usize) {
                items.push(PartitionKeyItemSpec { column: Some(n.clone()), expr: None, expr_type: None });
                continue;
            }
        }
        if matches!(bound.kind, ExprKind::Const(_)) {
            return Err(PgError::new(SqlState::INVALID_OBJECT_DEFINITION, "cannot use constant expression as partition key"));
        }
        items.push(PartitionKeyItemSpec { column: None, expr: Some(stored_expr), expr_type: Some(bound.ty) });
    }
    Ok(PartitionKeySpec { strategy, items })
}

/// Text of a datum in the bound's canonical (ISO, DateStyle-independent) form.
fn datum_text(env: &dyn TypeEnv, ty: TypeId, d: &Datum) -> Result<String> {
    let cfg = IoConfig::default();
    io::output(&IoCtx::new(env, &cfg), ty, d)
}

/// One bound value coerced to the key item type (PG `transformPartitionBoundValue`). `None` = NULL.
fn bound_value(b: &mut Binder, e: &Expr, col_name: &str, ty: TypeId) -> Result<Option<String>> {
    let pos = b.pos(e);
    let bound = super::expr::bind_expr(b, e, partition_bound_ctx())?;
    let coerced = match coerce_assignment(b, &BoundArg::new(bound.clone(), pos), col_name, ty) {
        Ok(c) => c,
        Err(err) if err.sqlstate == SqlState::DATATYPE_MISMATCH || err.sqlstate == SqlState::CANNOT_COERCE => {
            let tn = b.type_name_typmod(ty);
            return Err(at(
                PgError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("specified value cannot be cast to type {tn} for column \"{col_name}\""),
                ),
                pos,
            ));
        }
        Err(err) => return Err(err),
    };
    let env = b.env;
    if let ExprKind::Const(d) = &coerced.kind {
        if d.is_null() {
            return Ok(None);
        }
        return Ok(Some(datum_text(env, ty, d)?));
    }
    // A cast over a constant: re-read the constant's text with the key type's input function.
    let mut inner = &coerced;
    loop {
        match &inner.kind {
            ExprKind::Const(d) => {
                if d.is_null() {
                    return Ok(None);
                }
                let text = datum_text(env, inner.ty, d)?;
                let cfg = IoConfig::default();
                let v = io::input(&IoCtx::new(env, &cfg), ty, &text)?;
                return Ok(Some(datum_text(env, ty, &v)?));
            }
            ExprKind::FuncCall(f) if f.args.len() == 1 => inner = &f.args[0],
            ExprKind::Cast(c) => inner = &c.arg,
            _ => break,
        }
    }
    Err(at(PgError::unimplemented("SQL-06", "non-constant partition bound expressions"), pos))
}

/// `FOR VALUES ...` / `DEFAULT` against the parent's partition key (PG `transformPartitionBound`).
pub(super) fn bind_partition_bound(b: &mut Binder, parent: &Relation, fv: &ForValues, after: Option<usize>) -> Result<PartitionOfSpec> {
    let mut positions = match after {
        Some(byte) => bound_positions(b.src, byte),
        None => BoundPositions::default(),
    };
    let Some(key) = &parent.partition_key else {
        return Err(PgError::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{}\" is not partitioned", parent.name)));
    };
    let types = partition::key_types(parent);
    let names = partition::key_names(parent);
    let strategy = key.strategy;
    let bound = match fv {
        ForValues::Default => {
            if strategy == b'h' {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "a hash-partitioned table may not have a default partition",
                ));
            }
            PartitionBound::Default
        }
        ForValues::With { modulus, remainder } => {
            if strategy != b'h' {
                return Err(wrong_strategy(strategy));
            }
            if *modulus == 0 || *modulus > i32::MAX as u64 {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "modulus for hash partition must be an integer value greater than zero",
                ));
            }
            if remainder >= modulus {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "remainder for hash partition must be less than modulus",
                ));
            }
            PartitionBound::Hash { modulus: *modulus as u32, remainder: *remainder as u32 }
        }
        ForValues::In(values) => {
            if strategy != b'l' {
                return Err(wrong_strategy(strategy));
            }
            let ty = types.first().copied().unwrap_or(TypeId::TEXT);
            let name = names.first().cloned().unwrap_or_default();
            let mut out: Vec<Option<String>> = Vec::new();
            let mut list_pos: Vec<Option<u32>> = Vec::new();
            for (i, v) in values.iter().enumerate() {
                let val = bound_value(b, v, &name, ty)?;
                if !out.contains(&val) {
                    out.push(val);
                    list_pos.push(positions.list.get(i).copied().flatten());
                }
            }
            positions.list = list_pos;
            PartitionBound::List(out)
        }
        ForValues::From { from, to } => {
            if strategy != b'r' {
                return Err(wrong_strategy(strategy));
            }
            if from.len() != types.len() {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "FROM must specify exactly one value per partitioning column",
                ));
            }
            if to.len() != types.len() {
                return Err(PgError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "TO must specify exactly one value per partitioning column",
                ));
            }
            let lower = range_values(b, from, &types, &names)?;
            let upper = range_values(b, to, &types, &names)?;
            PartitionBound::Range { lower, upper }
        }
    };
    Ok(PartitionOfSpec { parent: parent.oid, bound, positions })
}

fn wrong_strategy(strategy: u8) -> PgError {
    PgError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("invalid bound specification for a {} partition", partition::strategy_name(strategy)),
    )
}

fn range_values(b: &mut Binder, vals: &[PartitionBoundValue], types: &[TypeId], names: &[String]) -> Result<Vec<RangeValue>> {
    let mut out = Vec::with_capacity(vals.len());
    for (i, v) in vals.iter().enumerate() {
        out.push(match v {
            PartitionBoundValue::MinValue => RangeValue::MinValue,
            PartitionBoundValue::MaxValue => RangeValue::MaxValue,
            PartitionBoundValue::Expr(e) => {
                let ty = types.get(i).copied().unwrap_or(TypeId::TEXT);
                let name = names.get(i).cloned().unwrap_or_default();
                match bound_value(b, e, &name, ty)? {
                    Some(s) => RangeValue::Value(s),
                    None => {
                        return Err(PgError::new(SqlState::INVALID_TABLE_DEFINITION, "cannot specify NULL in range bound"))
                    }
                }
            }
        });
    }
    // validateInfiniteBounds
    let mut kind = 0u8;
    for v in &out {
        let k = match v {
            RangeValue::Value(_) => 0,
            RangeValue::MinValue => 1,
            RangeValue::MaxValue => 2,
        };
        if k == kind {
            continue;
        }
        match kind {
            0 => kind = k,
            2 => return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "every bound following MAXVALUE must also be MAXVALUE")),
            _ => return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "every bound following MINVALUE must also be MINVALUE")),
        }
    }
    Ok(out)
}

/// Byte offset just after the `PARTITION OF` parent name (start of the bound scan).
pub(super) fn after_parent_name(b: &Binder, name: &ObjectName) -> Option<usize> {
    use sqlparser::ast::ObjectNamePart;
    match name.0.last() {
        Some(ObjectNamePart::Identifier(id)) => b.src.byte_offset(id.span.end),
        _ => None,
    }
}

/// Error cursor positions of the bound written after byte `start` (`FOR VALUES ...` / `DEFAULT`):
/// a light scan of the source text (quotes, parentheses, commas, words).
fn bound_positions(src: &crate::source::SourceMap, start: usize) -> BoundPositions {
    let text = src.text();
    let bytes = text.as_bytes();
    // (upper-cased word or punctuation, byte offset, paren depth)
    let mut toks: Vec<(String, usize, i32)> = Vec::new();
    let mut i = start.min(bytes.len());
    let mut depth = 0i32;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == b'\'' {
            let st = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            toks.push(("'".to_string(), st, depth));
        } else if c == b'(' {
            toks.push(("(".to_string(), i, depth));
            depth += 1;
            i += 1;
        } else if c == b')' {
            depth -= 1;
            toks.push((")".to_string(), i, depth));
            i += 1;
        } else if c == b',' || c == b';' {
            toks.push((String::from(c as char), i, depth));
            i += 1;
            if c == b';' {
                break;
            }
        } else {
            let st = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'(' | b')' | b',' | b'\'' | b';') {
                i += 1;
            }
            toks.push((text[st..i].to_ascii_uppercase(), st, depth));
        }
        if toks.len() > 4096 {
            break;
        }
    }
    let pos = |byte: usize| Some(src.pos_of_byte(byte));
    let mut out = BoundPositions::default();
    let Some(k) = toks.iter().position(|t| t.0 == "FOR" || t.0 == "DEFAULT") else { return out };
    out.spec = pos(toks[k].1);
    // Items of the parenthesized list starting at token index `open`.
    let items = |open: usize| -> Vec<Option<u32>> {
        let mut v = Vec::new();
        let mut expect = true;
        for t in toks.iter().skip(open + 1) {
            if t.2 == 0 && t.0 == ")" {
                break;
            }
            if t.2 == 1 && t.0 == "," {
                expect = true;
                continue;
            }
            if expect {
                v.push(pos(t.1));
                expect = false;
            }
        }
        v
    };
    for (j, t) in toks.iter().enumerate().skip(k) {
        let open = toks.get(j + 1).filter(|n| n.0 == "(").map(|_| j + 1);
        match (t.0.as_str(), open) {
            ("IN", Some(o)) if out.list.is_empty() => out.list = items(o),
            ("FROM", Some(o)) if out.lower.is_empty() => out.lower = items(o),
            ("TO", Some(o)) if out.upper.is_empty() => out.upper = items(o),
            _ => {}
        }
    }
    out
}
