//! PostgreSQL error model (SQL-13, ARCH-05/08). Messages must match PostgreSQL 18 wording.
use std::fmt;

/// Five-character SQLSTATE code.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlState(pub [u8; 5]);

impl SqlState {
    pub const SUCCESSFUL_COMPLETION: SqlState = SqlState(*b"00000");
    pub const WARNING: SqlState = SqlState(*b"01000");
    pub const NO_DATA: SqlState = SqlState(*b"02000");
    pub const CONNECTION_EXCEPTION: SqlState = SqlState(*b"08000");
    pub const CONNECTION_FAILURE: SqlState = SqlState(*b"08006");
    pub const PROTOCOL_VIOLATION: SqlState = SqlState(*b"08P01");
    pub const FEATURE_NOT_SUPPORTED: SqlState = SqlState(*b"0A000");
    pub const INVALID_TRANSACTION_INITIATION: SqlState = SqlState(*b"0B000");
    pub const CARDINALITY_VIOLATION: SqlState = SqlState(*b"21000");
    pub const DATA_EXCEPTION: SqlState = SqlState(*b"22000");
    pub const STRING_DATA_RIGHT_TRUNCATION: SqlState = SqlState(*b"22001");
    pub const NULL_VALUE_NOT_ALLOWED: SqlState = SqlState(*b"22004");
    pub const ARRAY_SUBSCRIPT_ERROR: SqlState = SqlState(*b"2202E");
    pub const CHARACTER_NOT_IN_REPERTOIRE: SqlState = SqlState(*b"22021");
    pub const DATETIME_FIELD_OVERFLOW: SqlState = SqlState(*b"22008");
    pub const DIVISION_BY_ZERO: SqlState = SqlState(*b"22012");
    pub const INVALID_DATETIME_FORMAT: SqlState = SqlState(*b"22007");
    pub const INTERVAL_FIELD_OVERFLOW: SqlState = SqlState(*b"22015");
    pub const INVALID_ARGUMENT_FOR_LOGARITHM: SqlState = SqlState(*b"2201E");
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: SqlState = SqlState(*b"2201F");
    pub const INVALID_PARAMETER_VALUE: SqlState = SqlState(*b"22023");
    pub const INVALID_REGULAR_EXPRESSION: SqlState = SqlState(*b"2201B");
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: SqlState = SqlState(*b"2201W");
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: SqlState = SqlState(*b"2201X");
    pub const INVALID_TEXT_REPRESENTATION: SqlState = SqlState(*b"22P02");
    pub const INVALID_BINARY_REPRESENTATION: SqlState = SqlState(*b"22P03");
    pub const INVALID_ESCAPE_SEQUENCE: SqlState = SqlState(*b"22025");
    pub const NUMERIC_VALUE_OUT_OF_RANGE: SqlState = SqlState(*b"22003");
    pub const UNTRANSLATABLE_CHARACTER: SqlState = SqlState(*b"22P05");
    pub const INVALID_JSON_TEXT: SqlState = SqlState(*b"22032");
    pub const SQL_JSON_MEMBER_NOT_FOUND: SqlState = SqlState(*b"2203A");
    pub const DATETIME_VALUE_OUT_OF_RANGE: SqlState = SqlState(*b"22008");
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: SqlState = SqlState(*b"22009");
    pub const INTEGRITY_CONSTRAINT_VIOLATION: SqlState = SqlState(*b"23000");
    pub const RESTRICT_VIOLATION: SqlState = SqlState(*b"23001");
    pub const NOT_NULL_VIOLATION: SqlState = SqlState(*b"23502");
    pub const FOREIGN_KEY_VIOLATION: SqlState = SqlState(*b"23503");
    pub const UNIQUE_VIOLATION: SqlState = SqlState(*b"23505");
    pub const CHECK_VIOLATION: SqlState = SqlState(*b"23514");
    pub const EXCLUSION_VIOLATION: SqlState = SqlState(*b"23P01");
    pub const INVALID_CURSOR_STATE: SqlState = SqlState(*b"24000");
    pub const INVALID_TRANSACTION_STATE: SqlState = SqlState(*b"25000");
    pub const ACTIVE_SQL_TRANSACTION: SqlState = SqlState(*b"25001");
    pub const READ_ONLY_SQL_TRANSACTION: SqlState = SqlState(*b"25006");
    pub const NO_ACTIVE_SQL_TRANSACTION: SqlState = SqlState(*b"25P01");
    pub const IN_FAILED_SQL_TRANSACTION: SqlState = SqlState(*b"25P02");
    pub const INVALID_SQL_STATEMENT_NAME: SqlState = SqlState(*b"26000");
    pub const INVALID_AUTHORIZATION_SPECIFICATION: SqlState = SqlState(*b"28000");
    pub const INVALID_PASSWORD: SqlState = SqlState(*b"28P01");
    pub const INVALID_CATALOG_NAME: SqlState = SqlState(*b"3D000");
    pub const INVALID_CURSOR_NAME: SqlState = SqlState(*b"34000");
    pub const INVALID_SCHEMA_NAME: SqlState = SqlState(*b"3F000");
    pub const TRANSACTION_ROLLBACK: SqlState = SqlState(*b"40000");
    pub const SERIALIZATION_FAILURE: SqlState = SqlState(*b"40001");
    pub const DEADLOCK_DETECTED: SqlState = SqlState(*b"40P01");
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: SqlState = SqlState(*b"42000");
    pub const SYNTAX_ERROR: SqlState = SqlState(*b"42601");
    pub const INSUFFICIENT_PRIVILEGE: SqlState = SqlState(*b"42501");
    pub const GROUPING_ERROR: SqlState = SqlState(*b"42803");
    pub const INVALID_FOREIGN_KEY: SqlState = SqlState(*b"42830");
    pub const INVALID_NAME: SqlState = SqlState(*b"42602");
    pub const NAME_TOO_LONG: SqlState = SqlState(*b"42622");
    pub const RESERVED_NAME: SqlState = SqlState(*b"42939");
    pub const DATATYPE_MISMATCH: SqlState = SqlState(*b"42804");
    pub const INDETERMINATE_DATATYPE: SqlState = SqlState(*b"42P18");
    pub const COLLATION_MISMATCH: SqlState = SqlState(*b"42P21");
    pub const INDETERMINATE_COLLATION: SqlState = SqlState(*b"42P22");
    pub const WRONG_OBJECT_TYPE: SqlState = SqlState(*b"42809");
    pub const UNDEFINED_COLUMN: SqlState = SqlState(*b"42703");
    pub const UNDEFINED_FUNCTION: SqlState = SqlState(*b"42883");
    pub const UNDEFINED_TABLE: SqlState = SqlState(*b"42P01");
    pub const UNDEFINED_PARAMETER: SqlState = SqlState(*b"42P02");
    pub const UNDEFINED_OBJECT: SqlState = SqlState(*b"42704");
    pub const DUPLICATE_COLUMN: SqlState = SqlState(*b"42701");
    pub const DUPLICATE_DATABASE: SqlState = SqlState(*b"42P04");
    pub const DUPLICATE_FUNCTION: SqlState = SqlState(*b"42723");
    pub const DUPLICATE_PREPARED_STATEMENT: SqlState = SqlState(*b"42P05");
    pub const DUPLICATE_SCHEMA: SqlState = SqlState(*b"42P06");
    pub const DUPLICATE_TABLE: SqlState = SqlState(*b"42P07");
    pub const DUPLICATE_ALIAS: SqlState = SqlState(*b"42712");
    pub const DUPLICATE_OBJECT: SqlState = SqlState(*b"42710");
    pub const AMBIGUOUS_COLUMN: SqlState = SqlState(*b"42702");
    pub const AMBIGUOUS_FUNCTION: SqlState = SqlState(*b"42725");
    pub const AMBIGUOUS_PARAMETER: SqlState = SqlState(*b"42P08");
    pub const INVALID_COLUMN_REFERENCE: SqlState = SqlState(*b"42P10");
    pub const INVALID_COLUMN_DEFINITION: SqlState = SqlState(*b"42611");
    pub const INVALID_TABLE_DEFINITION: SqlState = SqlState(*b"42P16");
    pub const INVALID_OBJECT_DEFINITION: SqlState = SqlState(*b"42P17");
    pub const INVALID_RECURSION: SqlState = SqlState(*b"42P19");
    pub const WINDOWING_ERROR: SqlState = SqlState(*b"42P20");
    pub const CANNOT_COERCE: SqlState = SqlState(*b"42846");
    pub const DEPENDENT_OBJECTS_STILL_EXIST: SqlState = SqlState(*b"2BP01");
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: SqlState = SqlState(*b"55000");
    pub const OBJECT_IN_USE: SqlState = SqlState(*b"55006");
    pub const LOCK_NOT_AVAILABLE: SqlState = SqlState(*b"55P03");
    pub const QUERY_CANCELED: SqlState = SqlState(*b"57014");
    pub const ADMIN_SHUTDOWN: SqlState = SqlState(*b"57P01");
    pub const INSUFFICIENT_RESOURCES: SqlState = SqlState(*b"53000");
    pub const DISK_FULL: SqlState = SqlState(*b"53100");
    pub const OUT_OF_MEMORY: SqlState = SqlState(*b"53200");
    pub const TOO_MANY_CONNECTIONS: SqlState = SqlState(*b"53300");
    pub const PROGRAM_LIMIT_EXCEEDED: SqlState = SqlState(*b"54000");
    pub const STATEMENT_TOO_COMPLEX: SqlState = SqlState(*b"54001");
    pub const UNDEFINED_FILE: SqlState = SqlState(*b"58P01");
    pub const IO_ERROR: SqlState = SqlState(*b"58030");
    pub const CONFIG_FILE_ERROR: SqlState = SqlState(*b"F0000");
    pub const PLPGSQL_ERROR: SqlState = SqlState(*b"P0000");
    pub const RAISE_EXCEPTION: SqlState = SqlState(*b"P0001");
    pub const INTERNAL_ERROR: SqlState = SqlState(*b"XX000");
    pub const DATA_CORRUPTED: SqlState = SqlState(*b"XX001");
    pub const INDEX_CORRUPTED: SqlState = SqlState(*b"XX002");

    pub fn code(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("XX000")
    }
    pub fn from_code(code: &str) -> Option<SqlState> {
        let b = code.as_bytes();
        if b.len() != 5 || !b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            return None;
        }
        Some(SqlState([b[0], b[1], b[2], b[3], b[4]]))
    }
    /// Class (first two characters), e.g. "23" for integrity constraint violations.
    pub fn class(&self) -> &str {
        &self.code()[..2]
    }
}

impl fmt::Debug for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SqlState({})", self.code())
    }
}
impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Error severity as sent in the wire `S`/`V` fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Fatal,
    Panic,
    Warning,
    Notice,
    Debug,
    Info,
    Log,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Fatal => "FATAL",
            Severity::Panic => "PANIC",
            Severity::Warning => "WARNING",
            Severity::Notice => "NOTICE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Log => "LOG",
        }
    }
}

/// A PostgreSQL-shaped error (or notice) with structured fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgError {
    pub sqlstate: SqlState,
    pub severity: Severity,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character position in the original query string.
    pub position: Option<u32>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub datatype: Option<String>,
    pub constraint: Option<String>,
}

pub type Result<T> = std::result::Result<T, PgError>;

impl PgError {
    pub fn new(sqlstate: SqlState, message: impl Into<String>) -> PgError {
        PgError {
            sqlstate,
            severity: Severity::Error,
            message: message.into(),
            detail: None,
            hint: None,
            position: None,
            schema: None,
            table: None,
            column: None,
            datatype: None,
            constraint: None,
        }
    }
    /// Feature not implemented yet. `lease` names the owning task/lease so the factory can inventory it.
    pub fn unimplemented(lease: &str, what: impl Into<String>) -> PgError {
        let what = what.into();
        PgError::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by PGArbor yet"))
            .with_detail(format!("pgarbor-unimplemented: {lease}"))
    }
    pub fn syntax(message: impl Into<String>, position: Option<u32>) -> PgError {
        let mut e = PgError::new(SqlState::SYNTAX_ERROR, message);
        e.position = position;
        e
    }
    pub fn internal(message: impl Into<String>) -> PgError {
        PgError::new(SqlState::INTERNAL_ERROR, message)
    }
    pub fn corruption(message: impl Into<String>) -> PgError {
        PgError::new(SqlState::DATA_CORRUPTED, message)
    }
    pub fn io(context: &str, err: &std::io::Error) -> PgError {
        let state = match err.raw_os_error() {
            Some(28) => SqlState::DISK_FULL, // ENOSPC
            _ => SqlState::IO_ERROR,
        };
        PgError::new(state, format!("{context}: {err}"))
    }
    pub fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn with_hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
    pub fn with_position(mut self, p: u32) -> Self {
        self.position = Some(p);
        self
    }
    pub fn with_table(mut self, schema: impl Into<String>, table: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self.table = Some(table.into());
        self
    }
    pub fn with_column(mut self, c: impl Into<String>) -> Self {
        self.column = Some(c.into());
        self
    }
    pub fn with_constraint(mut self, c: impl Into<String>) -> Self {
        self.constraint = Some(c.into());
        self
    }
    pub fn with_datatype(mut self, d: impl Into<String>) -> Self {
        self.datatype = Some(d.into());
        self
    }
    pub fn with_severity(mut self, s: Severity) -> Self {
        self.severity = s;
        self
    }
    /// A NOTICE-level message (sent as NoticeResponse, not an error).
    pub fn notice(message: impl Into<String>) -> PgError {
        PgError::new(SqlState::SUCCESSFUL_COMPLETION, message).with_severity(Severity::Notice)
    }
    pub fn is_unimplemented(&self) -> bool {
        self.sqlstate == SqlState::FEATURE_NOT_SUPPORTED
            && self.detail.as_deref().is_some_and(|d| d.starts_with("pgarbor-unimplemented:"))
    }
}

impl fmt::Display for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.severity.as_str(), self.message, self.sqlstate)
    }
}

impl std::error::Error for PgError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unimplemented_is_tagged() {
        let e = PgError::unimplemented("EXEC-13", "ON CONFLICT DO UPDATE");
        assert_eq!(e.sqlstate.code(), "0A000");
        assert!(e.is_unimplemented());
        assert_eq!(SqlState::from_code("23505"), Some(SqlState::UNIQUE_VIOLATION));
        assert_eq!(SqlState::UNIQUE_VIOLATION.class(), "23");
    }
}
