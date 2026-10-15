//! `VALUES (...), (...)` lists (SQL-24). PG references: `analyze.c` (`transformValuesClause`,
//! `transformInsertStmt` for the INSERT variant), `parse_coerce.c`.
//!
//! * Every row must have the same number of expressions: 42601 `VALUES lists must all be the same
//!   length` (`diag::values_length_mismatch`, position = first surplus expression of the offending row).
//! * Cells are bound with `ExprCtx::values()` (no aggregates/windows/SRFs/columns of this query:
//!   a column reference is 42703; outer references allowed in sub-selects).
//! * Per column: `coerce::coerce_to_common(cells, "VALUES")` ; all-unknown columns are text;
//!   `types[i]` = the common type. A standalone `VALUES (1),(2.5)` has columns `column1`, ... of type
//!   numeric.
//! * `DEFAULT` in a VALUES list is legal ONLY as an INSERT source (`InsertTarget` given): produces
//!   `ExprKind::SetToDefault{attnum}` for the matching target column and the column's own type; elsewhere
//!   42601 `DEFAULT is not allowed in this context`.
//! * INSERT source mode: cells are NOT unified into a common type per column; each cell is
//!   assignment-coerced directly to its target column type (`coerce::coerce_assignment`) — PG's
//!   `transformInsertStmt` does the same for a single-row/multi-row VALUES. The caller passes
//!   `insert_targets`.
use pgarbor_common::Result;
use pgarbor_types::TypeId;
use sqlparser::ast::{Expr, Values};

use super::coerce;
use super::context::{Binder, BoundArg, ExprCtx};
use super::expr;
use crate::diag;
use crate::ir::{BoundExpr, BoundValues, ExprKind, OutputColumn};

/// Target column description for INSERT ... VALUES.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub struct InsertTargetCol {
    pub attnum: i16,
    pub name: String,
    pub ty: TypeId,
}

/// Bind a VALUES list. `insert_targets`: `Some` when this VALUES is an INSERT source (then rows must
/// have exactly that many cells: 42601 `INSERT has more expressions than target columns` /
/// `INSERT has more target columns than expressions`) and cells are assignment-coerced.
pub fn bind_values_body(b: &mut Binder, values: &Values, insert_targets: Option<&[InsertTargetCol]>) -> Result<BoundValues> {
    match insert_targets {
        Some(targets) => bind_insert_source(b, values, targets),
        None => bind_standalone(b, values),
    }
}

/// Output columns of a standalone VALUES: `column1`, `column2`, ... (or the given aliases).
pub fn values_output_columns(values: &BoundValues, aliases: &[String]) -> Vec<OutputColumn> {
    values
        .types
        .iter()
        .enumerate()
        .map(|(i, ty)| OutputColumn {
            name: aliases.get(i).cloned().unwrap_or_else(|| format!("column{}", i + 1)),
            ty: *ty,
            origin: None,
        })
        .collect()
}

/// `DEFAULT` as sqlparser produces it in expression position: an unquoted identifier spelled
/// `DEFAULT` in any case. A quoted `"DEFAULT"` is an ordinary column reference.
fn is_default_keyword(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(id) if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("DEFAULT"))
}

/// Bind one cell where `DEFAULT` is not allowed (standalone VALUES): PG `transformExpressionList`
/// with `allow_default = false` raises 42601 at the keyword (`parse_expr.c`, `T_SetToDefault`).
fn bind_plain_cell(b: &mut Binder, e: &Expr) -> Result<BoundArg> {
    if is_default_keyword(e) {
        return Err(diag::syntax("DEFAULT is not allowed in this context", b.pos(e)));
    }
    expr::bind_arg(b, e, ExprCtx::values())
}

/// Standalone VALUES: bind row by row (each row's cells first, then the length check, as PG's
/// `transformValuesClause` does), then resolve each column's common type.
fn bind_standalone(b: &mut Binder, values: &Values) -> Result<BoundValues> {
    let mut cols: Vec<Vec<BoundArg>> = Vec::new();
    let mut width: Option<usize> = None;
    for row in &values.rows {
        let mut cells: Vec<BoundArg> = Vec::with_capacity(row.content.len());
        for e in &row.content {
            cells.push(bind_plain_cell(b, e)?);
        }
        // PG `exprLocation(sublist)`: the location of the first member of the offending row
        // (`nodeFuncs.c`, `T_List`).
        let first_pos = cells.first().and_then(|c| c.pos);
        if let Some(w) = width {
            if w != cells.len() {
                return Err(diag::values_length_mismatch(first_pos));
            }
        } else {
            width = Some(cells.len());
            cols = (0..cells.len()).map(|_| Vec::with_capacity(values.rows.len())).collect();
        }
        for (c, arg) in cells.into_iter().enumerate() {
            if let Some(col) = cols.get_mut(c) {
                col.push(arg);
            }
        }
    }

    let nrows = values.rows.len();
    let ncols = cols.len();
    let mut rows: Vec<Vec<BoundExpr>> = (0..nrows).map(|_| Vec::with_capacity(ncols)).collect();
    let mut types: Vec<TypeId> = Vec::with_capacity(ncols);
    for col in cols {
        let (exprs, ty) = coerce::coerce_to_common(b, col, "VALUES")?;
        types.push(ty);
        // `coerce_to_common` returns one expression per argument, in order: row r gets entry r.
        for (r, e) in exprs.into_iter().enumerate() {
            if let Some(row) = rows.get_mut(r) {
                row.push(e);
            }
        }
    }
    Ok(BoundValues { rows, types })
}

/// One cell of an INSERT source row. `DEFAULT` stays unbound until its target column is known.
enum Cell {
    Default(Option<u32>),
    Value(BoundArg),
}

impl Cell {
    fn pos(&self) -> Option<u32> {
        match self {
            Cell::Default(p) => *p,
            Cell::Value(arg) => arg.pos,
        }
    }
}

/// INSERT source: per row, bind the cells (DEFAULT allowed), check the cell count against the target
/// list (PG `transformInsertRow`), then assignment-coerce each cell to its own target column.
fn bind_insert_source(b: &mut Binder, values: &Values, targets: &[InsertTargetCol]) -> Result<BoundValues> {
    let mut rows: Vec<Vec<BoundExpr>> = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let mut cells: Vec<Cell> = Vec::with_capacity(row.content.len());
        for e in &row.content {
            if is_default_keyword(e) {
                cells.push(Cell::Default(b.pos(e)));
            } else {
                cells.push(Cell::Value(expr::bind_arg(b, e, ExprCtx::values())?));
            }
        }
        if cells.len() > targets.len() {
            // Position = first surplus expression (PG `exprLocation(list_nth(exprlist, icolumns))`).
            let pos = cells.get(targets.len()).and_then(Cell::pos);
            return Err(diag::insert_too_many_expressions(pos));
        }
        if cells.len() < targets.len() {
            return Err(diag::insert_too_many_columns(None));
        }
        let mut out: Vec<BoundExpr> = Vec::with_capacity(cells.len());
        for (cell, target) in cells.into_iter().zip(targets.iter()) {
            let bound = match cell {
                Cell::Default(_) => BoundExpr::new(ExprKind::SetToDefault { attnum: target.attnum }, target.ty),
                Cell::Value(arg) => coerce::coerce_assignment(b, &arg, &target.name, target.ty)?,
            };
            out.push(bound);
        }
        rows.push(out);
    }
    let types: Vec<TypeId> = targets.iter().map(|t| t.ty).collect();
    Ok(BoundValues { rows, types })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::context::{BindContext, BindSession};
    use crate::source::SourceMap;
    use pgarbor_catalog::testing;
    use pgarbor_types::context::IoConfig;
    use sqlparser::ast::{Ident, SetExpr, Statement};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// Parse `sql` (a bare VALUES statement) and return its `Values` body.
    fn values_of(sql: &str) -> Values {
        let stmts = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("test SQL parses");
        match stmts.into_iter().next() {
            Some(Statement::Query(q)) => match *q.body {
                SetExpr::Values(v) => v,
                other => panic!("expected VALUES, got {other:?}"),
            },
            other => panic!("expected a query, got {other:?}"),
        }
    }

    /// Bind the VALUES in `sql` against a bootstrapped catalog. Returns an owned result.
    fn bind(sql: &str, targets: Option<&[InsertTargetCol]>) -> Result<BoundValues> {
        let (_kv, env) = testing::bootstrapped()?;
        let values = values_of(sql);
        let src = SourceMap::new(sql);
        let ctx = BindContext {
            env: &env,
            session: BindSession { database: "postgres".to_string() },
            io_config: IoConfig::default(),
            statement_timestamp: 0,
            declared_params: Vec::new(),
        };
        let mut b = Binder::new(&ctx, &src);
        bind_values_body(&mut b, &values, targets)
    }

    fn int_text_targets() -> [InsertTargetCol; 2] {
        [
            InsertTargetCol { attnum: 1, name: "a".to_string(), ty: TypeId::INT4 },
            InsertTargetCol { attnum: 2, name: "b".to_string(), ty: TypeId::TEXT },
        ]
    }

    #[test]
    fn standalone_default_is_rejected() {
        let err = bind("values (default)", None).err().expect("DEFAULT must be rejected");
        assert_eq!(err.message, "DEFAULT is not allowed in this context");
        assert_eq!(err.position, Some(9));
    }

    #[test]
    fn insert_more_expressions_than_targets() {
        let targets = int_text_targets();
        let err = bind("values (default, default, default)", Some(&targets[..])).err().expect("too many cells");
        assert_eq!(err.message, "INSERT has more expressions than target columns");
        assert_eq!(err.position, Some(27));
    }

    #[test]
    fn insert_more_targets_than_expressions() {
        let targets = int_text_targets();
        let err = bind("values (default)", Some(&targets[..])).err().expect("too few cells");
        assert_eq!(err.message, "INSERT has more target columns than expressions");
    }

    #[test]
    fn output_columns_use_column_n_names_or_aliases() {
        let values = BoundValues { rows: Vec::new(), types: vec![TypeId::INT4, TypeId::TEXT] };
        let cols = values_output_columns(&values, &[]);
        assert_eq!(cols[0].name, "column1");
        assert_eq!(cols[1].name, "column2");
        let aliased = values_output_columns(&values, &["x".to_string()]);
        assert_eq!(aliased[0].name, "x");
        assert_eq!(aliased[1].name, "column2");
    }

    #[test]
    fn default_keyword_detection() {
        assert!(is_default_keyword(&Expr::Identifier(Ident::new("DEFAULT"))));
        assert!(is_default_keyword(&Expr::Identifier(Ident::new("default"))));
        assert!(!is_default_keyword(&Expr::Identifier(Ident::with_quote('"', "DEFAULT"))));
        assert!(!is_default_keyword(&Expr::Identifier(Ident::new("defaults"))));
    }

    #[test]
    #[ignore = "needs SQL-10 (expr::bind_arg) and SQL-09 (coerce)"]
    fn standalone_common_types() {
        let v = bind("values (1,'a'),(2,'b')", None).ok().expect("binds");
        assert_eq!(v.types, vec![TypeId::INT4, TypeId::TEXT]);
    }

    #[test]
    #[ignore = "needs SQL-10 (expr::bind_arg) and SQL-09 (coerce)"]
    fn standalone_numeric_column() {
        let v = bind("values (1),(2.5)", None).ok().expect("binds");
        assert_eq!(v.types, vec![TypeId::NUMERIC]);
    }

    #[test]
    #[ignore = "needs SQL-10 (expr::bind_arg)"]
    fn standalone_length_mismatch() {
        let err = bind("values (1),(1,2)", None).err().expect("mismatch");
        assert_eq!(err.message, "VALUES lists must all be the same length");
    }

    #[test]
    #[ignore = "needs SQL-10 (expr::bind_arg) and SQL-09 (coerce)"]
    fn insert_default_maps_to_set_to_default() {
        let targets = int_text_targets();
        let v = bind("values (1, default)", Some(&targets[..])).ok().expect("binds");
        assert_eq!(v.types, vec![TypeId::INT4, TypeId::TEXT]);
        let second = &v.rows[0][1];
        assert!(matches!(second.kind, ExprKind::SetToDefault { attnum: 2 }));
        assert_eq!(second.ty, TypeId::TEXT);
    }
}
