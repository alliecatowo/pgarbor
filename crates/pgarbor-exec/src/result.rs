//! Statement results (ARCHITECTURE §10). Fully implemented, frozen (v1).
use pgarbor_plan::plan::ColumnDesc;
use pgarbor_types::Datum;

/// What executing one statement produced.
// CONTRACT
#[derive(Clone, Debug, PartialEq)]
pub enum ExecResult {
    /// The empty statement (`;`): the client gets EmptyQueryResponse.
    Empty,
    /// A statement without a result set. `tag` is the complete PG command tag, e.g.
    /// `CREATE TABLE`, `INSERT 0 3` (rows are never returned for it), `UPDATE 2`, `DELETE 0`,
    /// `TRUNCATE TABLE`, `DROP INDEX`, `BEGIN`, `SET`.
    Command { tag: String },
    /// A statement with a result set (`SELECT`, DML with RETURNING, `SHOW`, `EXPLAIN`, `VALUES`).
    /// `tag` is the complete tag: `SELECT 5`, `INSERT 0 1` (with RETURNING), `UPDATE 2`, `SHOW`,
    /// `EXPLAIN`.
    Rows { desc: Vec<ColumnDesc>, rows: Vec<Vec<Datum>>, tag: String },
}

impl ExecResult {
    /// A `Command` result.
    pub fn command(tag: impl Into<String>) -> ExecResult {
        ExecResult::Command { tag: tag.into() }
    }

    /// The command tag (`""` for `Empty`).
    pub fn tag(&self) -> &str {
        match self {
            ExecResult::Empty => "",
            ExecResult::Command { tag } | ExecResult::Rows { tag, .. } => tag,
        }
    }

    /// Number of rows in a `Rows` result.
    pub fn row_count(&self) -> usize {
        match self {
            ExecResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }

    /// Rows affected as printed in the tag's last number (`INSERT 0 3` -> 3, `SELECT 5` -> 5,
    /// `CREATE TABLE` -> None).
    pub fn affected_from_tag(&self) -> Option<u64> {
        self.tag().rsplit(' ').next().and_then(|n| n.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_helpers() {
        assert_eq!(ExecResult::command("INSERT 0 3").affected_from_tag(), Some(3));
        assert_eq!(ExecResult::command("CREATE TABLE").affected_from_tag(), None);
        assert_eq!(ExecResult::Empty.tag(), "");
    }
}
