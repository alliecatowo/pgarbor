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
//! | `p` | internal position | `internal_position` |
//! | `q` | internal query | `internal_query` |
//! | `W` | context | `context` |
//! | `s` | schema name | `schema` |
//! | `t` | table name | `table` |
//! | `c` | column name | `column` |
//! | `d` | data type name | `datatype` |
//! | `n` | constraint name | `constraint` |
//!
//! Emission order is exactly `S V C M D H P p q W s t c d n` (amendment common-01 added `p q W`).
//! `F`/`L`/`R` (source file/line/routine) are NOT sent: [`PgError`] has no such fields and fabricating values would break
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
pub fn fields_from_pg_error(err: &PgError) -> pgarbor_common::Result<ErrorFields> {
    let mut fields = Vec::with_capacity(12);
    let severity = err.severity.as_str().to_owned();
    fields.push((b'S', severity.clone()));
    fields.push((b'V', severity));
    fields.push((b'C', err.sqlstate.code().to_owned()));
    fields.push((b'M', sanitize_value(&err.message)));

    push_optional(&mut fields, b'D', err.detail.as_deref());
    push_optional(&mut fields, b'H', err.hint.as_deref());
    if let Some(position) = err.position {
        fields.push((b'P', position.to_string()));
    }
    if let Some(position) = err.internal_position {
        fields.push((b'p', position.to_string()));
    }
    push_optional(&mut fields, b'q', err.internal_query.as_deref());
    push_optional(&mut fields, b'W', err.context.as_deref());
    push_optional(&mut fields, b's', err.schema.as_deref());
    push_optional(&mut fields, b't', err.table.as_deref());
    push_optional(&mut fields, b'c', err.column.as_deref());
    push_optional(&mut fields, b'd', err.datatype.as_deref());
    push_optional(&mut fields, b'n', err.constraint.as_deref());

    Ok(ErrorFields { fields })
}

/// Same fields for a NoticeResponse. Identical to [`fields_from_pg_error`] (the message type tag
/// differs, the fields do not); NOTICE/WARNING/INFO/LOG severities pass through unchanged.
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

fn push_optional(fields: &mut Vec<(u8, String)>, code: u8, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        fields.push((code, sanitize_value(value)));
    }
}

fn sanitize_value(value: &str) -> String {
    value.chars().map(|ch| if ch == '\0' { '?' } else { ch }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgarbor_common::SqlState;

    #[test]
    fn full_error_has_documented_order_and_values() {
        let mut error = PgError::new(SqlState::UNDEFINED_TABLE, "missing table")
            .with_detail("detail")
            .with_hint("hint")
            .with_position(12)
            .with_table("public", "items")
            .with_column("id")
            .with_datatype("integer")
            .with_constraint("items_pkey");
        error.message = "missing\0table".to_owned();

        let fields = fields_from_pg_error(&error).expect("field conversion is infallible");
        assert_eq!(
            fields.fields,
            vec![
                (b'S', "ERROR".into()), (b'V', "ERROR".into()), (b'C', "42P01".into()),
                (b'M', "missing?table".into()), (b'D', "detail".into()), (b'H', "hint".into()),
                (b'P', "12".into()), (b's', "public".into()), (b't', "items".into()),
                (b'c', "id".into()), (b'd', "integer".into()), (b'n', "items_pkey".into()),
            ]
        );
        assert!(!fields.fields.iter().any(|(code, _)| matches!(code, b'F' | b'L' | b'R' | b'W')));
    }

    #[test]
    fn minimal_error_keeps_empty_primary_message() {
        let error = PgError::new(SqlState::INTERNAL_ERROR, "");
        let fields = fields_from_pg_error(&error).expect("field conversion is infallible");
        assert_eq!(fields.fields, vec![
            (b'S', "ERROR".into()), (b'V', "ERROR".into()), (b'C', "XX000".into()), (b'M', "".into()),
        ]);
    }

    #[test]
    fn notice_severity_passes_through_and_empty_detail_is_omitted() {
        let notice = PgError::notice("hello").with_detail("");
        let fields = fields_for_notice(&notice).expect("field conversion is infallible");
        assert_eq!(fields.fields, vec![
            (b'S', "NOTICE".into()), (b'V', "NOTICE".into()), (b'C', "00000".into()), (b'M', "hello".into()),
        ]);
    }
}
