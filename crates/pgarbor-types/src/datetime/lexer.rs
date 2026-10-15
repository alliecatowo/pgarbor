//! Datetime input tokenizer (PG `ParseDateTime`) (TYPE-06). Lease TYP-22 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/datetime.c` `ParseDateTime()` — reproduce its field
//! splitting EXACTLY, because `decode.rs` (DecodeDateTime/DecodeTimeOnly) and `interval.rs`
//! (DecodeInterval) interpret fields by their kind.
//!
//! Summary of PG's rules (read the source for the full state machine):
//! - Whitespace separates fields; alphabetic characters are lower-cased (`pg_tolower`) while copying
//!   field text, so all later keyword/zone matching is case-insensitive.
//! - Leading digit: read digits; then `:` → TIME field (digits, `:`, `.`); `-` `/` `.` → DATE field
//!   (e.g. `2024-02-29`, `02/29/2024`, `2024.02.29`; a `.` after digits may also make a NUMBER with a
//!   fraction, e.g. `1.5`); otherwise NUMBER (may include `.` fraction).
//! - Leading `.`: NUMBER (fraction) or DATE.
//! - Leading alpha: read alpha; if followed by `-`/`/`/`.` + more → DATE (e.g. `jan-01-2020`) or a
//!   time zone name containing `/` (`america/new_york`) → TZ-ish STRING; else STRING.
//! - Leading `+`/`-`: if followed by digits → TZ field (`+05:30`, `-8`) ; if followed by alpha
//!   → SPECIAL (`-infinity`, `+infinity`); otherwise error.
//! - Other punctuation is skipped as a separator (PG `ispunct` handling) where allowed, else BadFormat.
//! - Max 25 fields (MAXDATEFIELDS); exceeding → `DtErr::BadFormat`.
//! Example: `"2024-02-29 13:01:02.5+02"` → [Date("2024-02-29"), Time("13:01:02.5"), Tz("+02")];
//! `"1999-01-08 04:05:06 -8:00"` → [Date, Time("04:05:06"), Tz("-8:00")];
//! `"January 8, 1999"` → [String("january"), Number("8"), Number("1999")].
use super::DtErr;
use pgarbor_common::{PgError, Result};

/// Field classes (PG `DTK_NUMBER`, `DTK_STRING`, `DTK_DATE`, `DTK_TIME`, `DTK_TZ`, `DTK_SPECIAL`).
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Number,
    String,
    Date,
    Time,
    Tz,
    Special,
}

/// One lexed field. `text` is lower-cased as PG does.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub kind: FieldKind,
}

/// Split a datetime/interval input string into fields (PG `ParseDateTime`).
/// Outer `Err` = engine error (never for user input); inner `Err(DtErr)` = malformed input, which the
/// caller turns into the PG message with `dt_parse_error(err, input, type_name)`.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_datetime(input: &str) -> Result<std::result::Result<Vec<Field>, DtErr>> {
    Err(PgError::unimplemented("TYP-22", "datetime input parsing"))
}
