//! PG-exact parse-analysis error and notice constructors (SQL-13). Fully implemented, frozen (v1).
//!
//! Every message below was verified against the PG 18.6 oracle (SQLSTATE, wording, position
//! semantics). Binder code must build its errors with these functions (or `pgarbor_catalog::errors`
//! for catalog-object errors such as `relation "x" does not exist`) so wording stays in one place.
//! `pos` is the 1-based character position (`SourceMap::pos_of_span`) of the offending token:
//! * column errors point at the first identifier of the reference (`t1.zz` -> the `t`);
//! * operator/function errors point at the OPERATOR token / function name;
//! * `INSERT has more expressions than target columns` points at the first surplus expression;
//! * argument-type errors (`argument of WHERE must be type boolean`) point at the expression.
use pgarbor_common::{PgError, SqlState};

fn at(mut e: PgError, pos: Option<u32>) -> PgError {
    e.position = pos;
    e
}

/// 42703 `column "zz" does not exist` (unqualified) or `column t1.zz does not exist` (qualified: no
/// quotes around the dotted name; verified).
pub fn undefined_column(qualifier: Option<&str>, name: &str, pos: Option<u32>) -> PgError {
    let msg = match qualifier {
        Some(q) => format!("column {q}.{name} does not exist"),
        None => format!("column \"{name}\" does not exist"),
    };
    at(PgError::new(SqlState::UNDEFINED_COLUMN, msg), pos)
}

/// 42703 plus HINT `Perhaps you meant to reference the column "t1.a".` (PG's closest-name hint;
/// only when exactly one candidate within edit distance; `Perhaps you meant to reference the column
/// "a" or the column "b".` for two).
pub fn undefined_column_with_hint(qualifier: Option<&str>, name: &str, hint: String, pos: Option<u32>) -> PgError {
    undefined_column(qualifier, name, pos).with_hint(hint)
}

/// 42702 `column reference "a" is ambiguous`.
pub fn ambiguous_column(name: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::AMBIGUOUS_COLUMN, format!("column reference \"{name}\" is ambiguous")), pos)
}

/// 42P01 `missing FROM-clause entry for table "x"` (qualified reference to an unknown range entry).
pub fn missing_from_entry(table: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::UNDEFINED_TABLE, format!("missing FROM-clause entry for table \"{table}\"")), pos)
}

/// 42P01 `invalid reference to FROM-clause entry for table "t"` + HINT
/// `There is an entry for table "t", but it cannot be referenced from this part of the query.`
pub fn invalid_from_reference(table: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::UNDEFINED_TABLE, format!("invalid reference to FROM-clause entry for table \"{table}\""))
            .with_hint(format!(
                "There is an entry for table \"{table}\", but it cannot be referenced from this part of the query."
            )),
        pos,
    )
}

/// 42P01 `invalid reference to FROM-clause entry for table "t"` + HINT
/// `Perhaps you meant to reference the table alias "a".` (the table is in FROM under an alias that is
/// visible here: `FROM colref AS c ... colref.y`).
pub fn invalid_from_reference_alias(table: &str, alias: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::UNDEFINED_TABLE, format!("invalid reference to FROM-clause entry for table \"{table}\""))
            .with_hint(format!("Perhaps you meant to reference the table alias \"{alias}\".")),
        pos,
    )
}

/// 42712 `table name "a" specified more than once`.
pub fn duplicate_alias(name: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::DUPLICATE_ALIAS, format!("table name \"{name}\" specified more than once")), pos)
}

/// 42803 `column "t1.b" must appear in the GROUP BY clause or be used in an aggregate function`
/// (`rel.col` is always table-qualified by the range-entry alias).
pub fn ungrouped_column(rel: &str, col: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::GROUPING_ERROR,
            format!("column \"{rel}.{col}\" must appear in the GROUP BY clause or be used in an aggregate function"),
        ),
        pos,
    )
}

/// 42803 variant for an ungrouped column inside a sub-select of an outer-grouped query:
/// `subquery uses ungrouped column "t1.b" from outer query`.
pub fn ungrouped_column_in_subquery(rel: &str, col: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::GROUPING_ERROR,
            format!("subquery uses ungrouped column \"{rel}.{col}\" from outer query"),
        ),
        pos,
    )
}

/// Where an aggregate / window function / SRF is not allowed (PG `ParseExprKind` names).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExprClause {
    Where,
    GroupBy,
    Having,
    FromClause,
    JoinOn,
    JoinUsing,
    FunctionInFrom,
    Limit,
    Offset,
    PartitionBy,
    WindowOrderBy,
    WindowFrameRange,
    WindowFrameRows,
    WindowFrameGroups,
    Values,
    CheckConstraint,
    ColumnDefault,
    GeneratedColumn,
    IndexExpression,
    IndexPredicate,
    PolicyExpression,
    CallArgument,
    CopyWhere,
    ConflictTarget,
    DomainCheck,
    ExecuteParameter,
    AlterColumnType,
    ReturningInUpdate,
    Other(&'static str),
}

impl ExprClause {
    /// Wording used after `... are not allowed in ` (PG `ParseExprKindName`).
    pub fn name(self) -> &'static str {
        match self {
            ExprClause::Where => "WHERE",
            ExprClause::GroupBy => "GROUP BY",
            ExprClause::Having => "HAVING",
            ExprClause::FromClause => "FROM clause",
            ExprClause::JoinOn => "JOIN conditions",
            ExprClause::JoinUsing => "JOIN USING",
            ExprClause::FunctionInFrom => "functions in FROM",
            ExprClause::Limit => "LIMIT",
            ExprClause::Offset => "OFFSET",
            ExprClause::PartitionBy => "window PARTITION BY",
            ExprClause::WindowOrderBy => "window ORDER BY",
            ExprClause::WindowFrameRange => "window RANGE",
            ExprClause::WindowFrameRows => "window ROWS",
            ExprClause::WindowFrameGroups => "window GROUPS",
            ExprClause::Values => "VALUES",
            ExprClause::CheckConstraint => "CHECK constraints",
            ExprClause::ColumnDefault => "DEFAULT expressions",
            ExprClause::GeneratedColumn => "generated column expressions",
            ExprClause::IndexExpression => "index expressions",
            ExprClause::IndexPredicate => "index predicates",
            ExprClause::PolicyExpression => "policy expressions",
            ExprClause::CallArgument => "CALL arguments",
            ExprClause::CopyWhere => "COPY FROM WHERE conditions",
            ExprClause::ConflictTarget => "ON CONFLICT",
            ExprClause::DomainCheck => "CHECK constraints",
            ExprClause::ExecuteParameter => "EXECUTE parameters",
            ExprClause::AlterColumnType => "ALTER COLUMN ... TYPE",
            ExprClause::ReturningInUpdate => "UPDATE",
            ExprClause::Other(s) => s,
        }
    }
}

/// 42803 `aggregate functions are not allowed in WHERE`.
pub fn agg_not_allowed(clause: ExprClause, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::GROUPING_ERROR, format!("aggregate functions are not allowed in {}", clause.name())),
        pos,
    )
}

/// 42P20 `window functions are not allowed in WHERE`.
pub fn window_not_allowed(clause: ExprClause, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::WINDOWING_ERROR, format!("window functions are not allowed in {}", clause.name())),
        pos,
    )
}

/// 0A000 `set-returning functions are not allowed in WHERE`.
pub fn srf_not_allowed(clause: ExprClause, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("set-returning functions are not allowed in {}", clause.name()),
        ),
        pos,
    )
}

/// 42803 `aggregate function calls cannot be nested`.
pub fn nested_aggregate(pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::GROUPING_ERROR, "aggregate function calls cannot be nested"), pos)
}

/// 42804 `argument of WHERE must be type boolean, not type integer`
/// (`what` = `WHERE`, `HAVING`, `AND`, `OR`, `NOT`, `JOIN/ON`, `CASE/WHEN`, `FILTER`, `CHECK`...).
pub fn not_boolean(what: &str, actual_type: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::DATATYPE_MISMATCH, format!("argument of {what} must be type boolean, not type {actual_type}")),
        pos,
    )
}

/// 42804 `column "a" is of type integer but expression is of type text` + HINT
/// `You will need to rewrite or cast the expression.` (INSERT/UPDATE assignment mismatch).
pub fn assignment_type_mismatch(col: &str, col_type: &str, expr_type: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::DATATYPE_MISMATCH,
            format!("column \"{col}\" is of type {col_type} but expression is of type {expr_type}"),
        )
        .with_hint("You will need to rewrite or cast the expression."),
        pos,
    )
}

/// 42601 `INSERT has more expressions than target columns`.
pub fn insert_too_many_expressions(pos: Option<u32>) -> PgError {
    at(PgError::syntax("INSERT has more expressions than target columns", None), pos)
}

/// 42601 `INSERT has more target columns than expressions`.
pub fn insert_too_many_columns(pos: Option<u32>) -> PgError {
    at(PgError::syntax("INSERT has more target columns than expressions", None), pos)
}

/// 42703 `column "zz" of relation "t1" does not exist` (INSERT/UPDATE target list).
pub fn column_of_relation_missing(col: &str, rel: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{col}\" of relation \"{rel}\" does not exist")),
        pos,
    )
}

/// 42701 `column "a" specified more than once` (INSERT column list; position at the second `a`).
pub fn column_specified_twice(col: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::DUPLICATE_COLUMN, format!("column \"{col}\" specified more than once")), pos)
}

/// 42601 `multiple assignments to same column "a"` (UPDATE SET).
pub fn multiple_assignments(col: &str, pos: Option<u32>) -> PgError {
    at(PgError::syntax(format!("multiple assignments to same column \"{col}\""), None), pos)
}

/// 42P10 `ORDER BY position 3 is not in select list` (`GROUP BY position 3 is not in select list`).
pub fn position_not_in_select_list(clause: &str, n: i64, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::INVALID_COLUMN_REFERENCE, format!("{clause} position {n} is not in select list")),
        pos,
    )
}

/// 42702 `ORDER BY "x" is ambiguous` (also `GROUP BY "x" is ambiguous`).
pub fn output_name_ambiguous(clause: &str, name: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::AMBIGUOUS_COLUMN, format!("{clause} \"{name}\" is ambiguous")), pos)
}

/// 42P10 `for SELECT DISTINCT, ORDER BY expressions must appear in select list`.
pub fn distinct_order_by(pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
        ),
        pos,
    )
}

/// 42P10 `SELECT DISTINCT ON expressions must match initial ORDER BY expressions`.
pub fn distinct_on_mismatch(pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
        ),
        pos,
    )
}

/// 0A000 `invalid UNION/INTERSECT/EXCEPT ORDER BY clause` + DETAIL `Only result column names can be
/// used, not expressions or functions.` + HINT `Add the expression/function to every SELECT, or move
/// the UNION into a FROM clause.`
pub fn setop_order_by_expression(pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::FEATURE_NOT_SUPPORTED, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
            .with_detail("Only result column names can be used, not expressions or functions.")
            .with_hint("Add the expression/function to every SELECT, or move the UNION into a FROM clause."),
        pos,
    )
}

/// 42601 `each UNION query must have the same number of columns` (`op` = UNION|INTERSECT|EXCEPT).
pub fn setop_column_count(op: &str, pos: Option<u32>) -> PgError {
    at(PgError::syntax(format!("each {op} query must have the same number of columns"), None), pos)
}

/// 42804 `UNION types integer and text cannot be matched` (usually produced by
/// `resolve::coerce::select_common_type`; this builder is for the binder's own checks).
pub fn setop_type_mismatch(op: &str, a: &str, b: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::DATATYPE_MISMATCH, format!("{op} types {a} and {b} cannot be matched")),
        pos,
    )
}

/// 42601 `VALUES lists must all be the same length`.
pub fn values_length_mismatch(pos: Option<u32>) -> PgError {
    at(PgError::syntax("VALUES lists must all be the same length", None), pos)
}

/// 42601 `subquery must return only one column`.
pub fn subquery_one_column(pos: Option<u32>) -> PgError {
    at(PgError::syntax("subquery must return only one column", None), pos)
}

/// 42601 `subquery has too many columns` / `subquery has too few columns` (IN / ANY / row tests).
pub fn subquery_column_count(too_many: bool, pos: Option<u32>) -> PgError {
    let msg = if too_many { "subquery has too many columns" } else { "subquery has too few columns" };
    at(PgError::syntax(msg, None), pos)
}

/// 42601 `unequal number of entries in row expressions`.
pub fn unequal_row_entries(pos: Option<u32>) -> PgError {
    at(PgError::syntax("unequal number of entries in row expressions", None), pos)
}

/// 42809 `op ANY/ALL (array) requires array on right side`.
pub fn any_all_needs_array(pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::WRONG_OBJECT_TYPE, "op ANY/ALL (array) requires array on right side"), pos)
}

/// 2201W `LIMIT must not be negative` / 2201X `OFFSET must not be negative` (raised at run time by
/// the executor; builder provided for constant-folded limits).
pub fn negative_limit(is_offset: bool) -> PgError {
    if is_offset {
        PgError::new(SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE, "OFFSET must not be negative")
    } else {
        PgError::new(SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, "LIMIT must not be negative")
    }
}

/// 42P18 `could not determine data type of parameter $1`.
pub fn param_type_unknown(n: u32, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::INDETERMINATE_DATATYPE, format!("could not determine data type of parameter ${n}")), pos)
}

/// 42P02 `there is no parameter $1` (a `$n` in a statement bound without parameters / beyond the
/// declared count when the protocol fixed the parameter count).
pub fn no_such_parameter(n: u32, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::UNDEFINED_PARAMETER, format!("there is no parameter ${n}")), pos)
}

/// 42P08 `inconsistent types deduced for parameter $1` + DETAIL `integer versus text`.
pub fn inconsistent_param_types(n: u32, a: &str, b: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(SqlState::AMBIGUOUS_PARAMETER, format!("inconsistent types deduced for parameter ${n}"))
            .with_detail(format!("{a} versus {b}")),
        pos,
    )
}

/// 42883 `operator does not exist: integer + text` + HINT `No operator matches the given name and
/// argument types. You might need to add explicit type casts.` (normally produced by
/// `pgarbor_types::ops::resolve_operator`; the binder only attaches the position).
pub fn with_position(e: PgError, pos: Option<u32>) -> PgError {
    if e.position.is_none() { at(e, pos) } else { e }
}

/// 42846 `cannot cast type integer to uuid` (explicit cast with no pathway) -- position at the cast.
pub fn cannot_cast(from: &str, to: &str, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::CANNOT_COERCE, format!("cannot cast type {from} to {to}")), pos)
}

/// 42P19 `recursive reference to query "r" must not appear within its non-recursive term`
/// (other recursion errors: `recursive query "r" does not have the form non-recursive-term UNION
/// [ALL] recursive-term`, `recursive reference to query "r" must not appear more than once`,
/// `recursive reference to query "r" must not appear within a subquery`, `... within an outer join`,
/// `... within INTERSECT`/`EXCEPT`): pass the tail of the message in `what`.
pub fn invalid_recursion(msg: String, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::INVALID_RECURSION, msg), pos)
}

/// 42P01-class: `relation "t2" in FOR UPDATE clause not found in FROM clause`.
pub fn locking_relation_missing(rel: &str, strength_clause: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{rel}\" in {strength_clause} clause not found in FROM clause"),
        ),
        pos,
    )
}

/// 42P10-class: `table "g" has 1 columns available but 2 columns specified` (column alias list too long).
pub fn too_many_column_aliases(table: &str, available: usize, specified: usize, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::INVALID_COLUMN_REFERENCE,
            format!("table \"{table}\" has {available} columns available but {specified} columns specified"),
        ),
        pos,
    )
}

/// 42P10 `column "zz" specified in USING clause does not exist in left table` (`right table`).
pub fn using_column_missing(col: &str, side: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{col}\" specified in USING clause does not exist in {side} table"),
        ),
        pos,
    )
}

/// 42702 `common column name "a" appears more than once in left table` (`right table`).
pub fn using_column_ambiguous(col: &str, side: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::AMBIGUOUS_COLUMN,
            format!("common column name \"{col}\" appears more than once in {side} table"),
        ),
        pos,
    )
}

/// 0A000 for a construct PostgreSQL accepts but PGArbor deliberately does not implement (the
/// classifier's "explicitly unsupported" list, SQL-14 inventory). Same message/DETAIL shape as
/// `PgError::unimplemented` (`<what> is not supported by PGArbor yet`, DETAIL
/// `pgarbor-unimplemented: SQL-14`) so inventories count it, but it is NOT a lease placeholder: it
/// is built by a frozen file. Prefer `PgError::unimplemented("LEASE", what)` inside leased bodies.
pub fn unsupported_construct(what: &str, pos: Option<u32>) -> PgError {
    let e = PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by PGArbor yet"))
        .with_detail("pgarbor-unimplemented: SQL-14");
    at(e, pos)
}

/// 0A000 `cross-database references are not implemented: db.schema.t`.
pub fn cross_database(qualified: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cross-database references are not implemented: {qualified}"),
        ),
        pos,
    )
}

/// 42601 `syntax error at or near "x"` (token text as PG prints it).
pub fn syntax_error_near(token: &str, pos: Option<u32>) -> PgError {
    PgError::syntax(format!("syntax error at or near \"{token}\""), pos)
}

/// 42601 `syntax error at end of input` (position = one past the last character).
pub fn syntax_error_eof(pos: u32) -> PgError {
    PgError::syntax("syntax error at end of input", Some(pos))
}

/// 42601 `unterminated quoted string at or near "'abc"` (also `unterminated quoted identifier`,
/// `unterminated dollar-quoted string`, `unterminated /* comment`).
pub fn unterminated(what: &str, near: &str, pos: Option<u32>) -> PgError {
    PgError::syntax(format!("{what} at or near \"{near}\""), pos)
}

/// 42601 `syntax error` family with a custom text (`zero-length delimited identifier at or near
/// """"`, `invalid hexadecimal digit`...).
pub fn syntax(msg: impl Into<String>, pos: Option<u32>) -> PgError {
    PgError::syntax(msg, pos)
}

/// 22023 `invalid value for parameter ...` helper used by SET/COPY option checks.
pub fn invalid_parameter_value(msg: impl Into<String>, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::INVALID_PARAMETER_VALUE, msg), pos)
}

/// 42P16 `multiple default values specified for column "a" of table "t"` and similar
/// CREATE TABLE definition errors; `msg` is the full message.
pub fn invalid_table_definition(msg: impl Into<String>, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::INVALID_TABLE_DEFINITION, msg), pos)
}

/// 42P17 invalid object definition (`View columns ...`, `CREATE VIEW specifies more column names
/// than columns`, ...).
pub fn invalid_object_definition(msg: impl Into<String>, pos: Option<u32>) -> PgError {
    at(PgError::new(SqlState::INVALID_OBJECT_DEFINITION, msg), pos)
}

/// 42P20/0A000-style utility: `SELECT ... FOR UPDATE is not allowed with DISTINCT clause`
/// (`what` = `DISTINCT clause`, `GROUP BY clause`, `HAVING clause`, `aggregate functions`,
/// `window functions`, `set-returning functions in the target list`, `UNION/INTERSECT/EXCEPT`).
pub fn locking_not_allowed(strength_clause: &str, what: &str, pos: Option<u32>) -> PgError {
    at(
        PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{strength_clause} is not allowed with {what}"),
        ),
        pos,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_messages() {
        assert_eq!(undefined_column(Some("t1"), "zz", Some(8)).message, "column t1.zz does not exist");
        assert_eq!(undefined_column(None, "zz", None).message, "column \"zz\" does not exist");
        assert_eq!(
            ungrouped_column("t1", "b", None).message,
            "column \"t1.b\" must appear in the GROUP BY clause or be used in an aggregate function"
        );
        assert_eq!(agg_not_allowed(ExprClause::Where, None).message, "aggregate functions are not allowed in WHERE");
    }

    #[test]
    fn param_messages() {
        let e = inconsistent_param_types(1, "integer", "text", Some(8));
        assert_eq!(e.sqlstate, SqlState::AMBIGUOUS_PARAMETER);
        assert_eq!(e.detail.as_deref(), Some("integer versus text"));
    }
}
