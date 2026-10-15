//! ErrorResponse / NoticeResponse field encoding (lease WIR-04). PG: `utils/error/elog.c`
//! `send_message_to_frontend`, `protocol.sgml` "ErrorResponse"/"NoticeResponse".
//!
//! A [`PgError`] becomes an ordered list of `(field_code, value)` pairs. The codes and PG's emission
//! order are:
//!
//! | code | field | source in [`PgError`] |
//! |---|---|---|
//! | `S` | severity (localized; we send the English name) | `severity.as_str()`; see below for `Fatal`/`Notice` |
//! | `V` | severity, never localized | same text as `S` |
//! | `C` | SQLSTATE | `sqlstate.code()` |
//! | `M` | primary message | `message` |
//! | `D` | detail | `detail` (omitted when `None`) |
//! | `H` | hint | `hint` |
//! | `P` | cursor position (1-based char index, decimal) | `position` |
//! | `s` | schema name | `schema` |
//! | `t` | table name | `table` |
//! | `c` | column name | `column` |
//! | `d` | data type name | `datatype` |
//! | `n` | constraint name | `constraint` |
//!
//! Emission order is exactly `S V C M D H P s t c d n`. `F`/`L`/`R` (source file/line/routine) and
//! `W` (context) are NOT sent: [`PgError`] has no such fields and fabricating values would break
//! transcript parity checks (a `ServerConfig::emit_source_fields` switch is deliberately absent).
//!
//! Severity text: `Severity::as_str()` already returns PG's strings (`ERROR`, `FATAL`, `PANIC`,
//! `WARNING`, `NOTICE`, `DEBUG`, `INFO`, `LOG`). PG sends `S` and `V` identically in an English
//! locale. Empty `message` is sent as-is (never omit `M`). Fields with an empty string value other
//! than `M` are omitted (PG omits NULL fields; our `Option::Some("")` is treated as absent).
//! Interior NULs in values are replaced by `?` by the encoder.
use pgarbor_common::{PgError, Severity};

/// Ordered `(code, value)` pairs of one error or notice message.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ErrorFields {
    pub fields: Vec<(u8, String)>,
}

impl ErrorFields {
    /// Value of the first field with `code`.
    pub fn get(&self, code: u8) -> Option<&str> {
        self.fields.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_str())
    }
    /// SQLSTATE (`C`) or `""`.
    pub fn sqlstate(&self) -> &str {
        self.get(b'C').unwrap_or("")
    }
    /// Primary message (`M`) or `""`.
    pub fn message(&self) -> &str {
        self.get(b'M').unwrap_or("")
    }
}

/// Build the field list of `err` (module docs: codes, order, omissions).
///
/// Example: `PgError::new(SqlState::UNDEFINED_TABLE, "relation \"t\" does not exist").with_position(15)`
/// -> `[(b'S',"ERROR"),(b'V',"ERROR"),(b'C',"42P01"),(b'M',"relation \"t\" does not exist"),(b'P',"15")]`.
#[allow(unused_variables)]
pub fn fields_from_pg_error(err: &PgError) -> pgarbor_common::Result<ErrorFields> {
    Err(PgError::unimplemented("WIR-04", "PgError -> ErrorResponse fields (error_fields::fields_from_pg_error)"))
}

/// Same fields for a NoticeResponse. Identical to [`fields_from_pg_error`] (the message type tag
/// differs, the fields do not); NOTICE/WARNING/INFO/LOG severities pass through unchanged.
#[allow(unused_variables)]
pub fn fields_for_notice(notice: &PgError) -> pgarbor_common::Result<ErrorFields> {
    fields_from_pg_error(notice)
}

/// Convenience used everywhere an error must be sent: never fails. If building fields fails (it
/// cannot for valid input) fall back to `S`,`V`,`C`=`XX000`,`M`=`internal error building error
/// response`.
pub fn error_message(err: &PgError) -> crate::backend::Backend {
    let fields = fields_from_pg_error(err).unwrap_or_else(|_| fallback_fields());
    crate::backend::Backend::ErrorResponse(fields)
}

/// Notice counterpart of [`error_message`].
pub fn notice_message(notice: &PgError) -> crate::backend::Backend {
    let fields = fields_for_notice(notice).unwrap_or_else(|_| fallback_fields());
    crate::backend::Backend::NoticeResponse(fields)
}

/// Marker fields used when the real conversion is unavailable.
pub fn fallback_fields() -> ErrorFields {
    ErrorFields {
        fields: vec![
            (b'S', Severity::Error.as_str().to_string()),
            (b'V', Severity::Error.as_str().to_string()),
            (b'C', "XX000".to_string()),
            (b'M', "internal error building error response".to_string()),
        ],
    }
}
