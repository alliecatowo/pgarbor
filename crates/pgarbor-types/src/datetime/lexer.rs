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
use pgarbor_common::Result;

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

/// Maximum number of fields (PG `MAXDATEFIELDS`).
const MAX_DATE_FIELDS: usize = 25;
/// PG work buffer size: `MAXDATELEN + MAXDATEFIELDS`.
const WORKBUF_LEN: usize = 128 + 25;

/// PG core keyword table (`datetktbl`), used only to decide whether `alpha` followed by a digit or `+`
/// starts a time zone name. Sorted bytewise.
const CORE_TOKENS: &[&str] = &[
    "+infinity", "-infinity", "ad", "allballs", "am", "apr", "april", "at", "aug", "august", "bc", "d", "dec",
    "december", "dow", "doy", "dst", "epoch", "feb", "february", "fri", "friday", "h", "infinity", "isodow",
    "isoyear", "j", "jan", "january", "jd", "jul", "julian", "july", "jun", "june", "m", "mar", "march", "may",
    "mm", "mon", "monday", "nov", "november", "now", "oct", "october", "on", "pm", "s", "sat", "saturday", "sep",
    "sept", "september", "sun", "sunday", "t", "thu", "thur", "thurs", "thursday", "today", "tomorrow", "tue",
    "tues", "tuesday", "wed", "wednesday", "weds", "y", "yesterday",
];

#[inline]
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Cursor over the input bytes plus the emulated PG work-buffer accounting.
struct Lx<'a> {
    b: &'a [u8],
    pos: usize,
    used: usize,
    cur: String,
}

impl<'a> Lx<'a> {
    #[inline]
    fn peek(&self) -> u8 {
        self.b.get(self.pos).copied().unwrap_or(0)
    }

    /// PG `APPEND_CHAR` of the current byte (optionally lower-cased); advances the cursor.
    #[inline]
    fn take(&mut self, lower: bool) -> std::result::Result<(), DtErr> {
        if self.used + 1 >= WORKBUF_LEN {
            return Err(DtErr::BadFormat);
        }
        let c = self.b[self.pos];
        self.pos += 1;
        self.used += 1;
        let c = if lower { c.to_ascii_lowercase() } else { c };
        self.cur.push(c as char);
        Ok(())
    }

    fn take_while(&mut self, lower: bool, f: impl Fn(u8) -> bool) -> std::result::Result<(), DtErr> {
        while self.pos < self.b.len() && f(self.b[self.pos]) {
            self.take(lower)?;
        }
        Ok(())
    }
}

fn lex(input: &str) -> std::result::Result<Vec<Field>, DtErr> {
    let mut bytes = input.as_bytes();
    // PG's C-string semantics: an embedded NUL ends the input.
    if let Some(n) = bytes.iter().position(|&c| c == 0) {
        bytes = &bytes[..n];
    }
    let mut lx = Lx { b: bytes, pos: 0, used: 0, cur: String::new() };
    let mut out: Vec<Field> = Vec::new();
    while lx.pos < lx.b.len() {
        let c = lx.peek();
        if is_space(c) {
            lx.pos += 1;
            continue;
        }
        if out.len() >= MAX_DATE_FIELDS {
            return Err(DtErr::BadFormat);
        }
        lx.cur.clear();
        let kind;
        if c.is_ascii_digit() {
            lx.take(false)?;
            lx.take_while(false, |c| c.is_ascii_digit())?;
            let c = lx.peek();
            if c == b':' {
                kind = FieldKind::Time;
                lx.take(false)?;
                lx.take_while(false, |c| c.is_ascii_digit() || c == b':' || c == b'.')?;
            } else if c == b'-' || c == b'/' || c == b'.' {
                let delim = c;
                lx.take(false)?;
                if lx.peek().is_ascii_digit() {
                    let mut k = if delim == b'.' { FieldKind::Number } else { FieldKind::Date };
                    lx.take_while(false, |c| c.is_ascii_digit())?;
                    if lx.peek() == delim {
                        k = FieldKind::Date;
                        lx.take(false)?;
                        lx.take_while(false, |c| c.is_ascii_digit() || c == delim)?;
                    }
                    kind = k;
                } else {
                    kind = FieldKind::Date;
                    lx.take_while(true, |c| c.is_ascii_alphanumeric() || c == delim)?;
                }
            } else {
                kind = FieldKind::Number;
            }
        } else if c == b'.' {
            lx.take(false)?;
            lx.take_while(false, |c| c.is_ascii_digit())?;
            kind = FieldKind::Number;
        } else if c.is_ascii_alphabetic() {
            lx.take(true)?;
            lx.take_while(true, |c| c.is_ascii_alphabetic())?;
            let n = lx.peek();
            let mut is_date = false;
            if n == b'-' || n == b'/' || n == b'.' {
                is_date = true;
            } else if (n == b'+' || n.is_ascii_digit()) && CORE_TOKENS.binary_search(&lx.cur.as_str()).is_err() {
                is_date = true;
            }
            if is_date {
                kind = FieldKind::Date;
                loop {
                    lx.take(true)?;
                    let c = lx.peek();
                    if !(matches!(c, b'+' | b'-' | b'/' | b'_' | b'.' | b':') || c.is_ascii_alphanumeric()) {
                        break;
                    }
                }
            } else {
                kind = FieldKind::String;
            }
        } else if c == b'+' || c == b'-' {
            lx.take(false)?;
            while is_space(lx.peek()) {
                lx.pos += 1;
            }
            let n = lx.peek();
            if n.is_ascii_digit() {
                kind = FieldKind::Tz;
                lx.take(false)?;
                lx.take_while(false, |c| c.is_ascii_digit() || c == b':' || c == b'.' || c == b'-')?;
            } else if n.is_ascii_alphabetic() {
                kind = FieldKind::Special;
                lx.take(true)?;
                lx.take_while(true, |c| c.is_ascii_alphabetic())?;
            } else {
                return Err(DtErr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            lx.pos += 1;
            continue;
        } else {
            return Err(DtErr::BadFormat);
        }
        // Field terminator occupies one buffer byte.
        lx.used += 1;
        out.push(Field { text: std::mem::take(&mut lx.cur), kind });
    }
    Ok(out)
}

/// Split a datetime/interval input string into fields (PG `ParseDateTime`).
/// Outer `Err` = engine error (never for user input); inner `Err(DtErr)` = malformed input, which the
/// caller turns into the PG message with `dt_parse_error(err, input, type_name)`.
// CONTRACT
pub fn parse_datetime(input: &str) -> Result<std::result::Result<Vec<Field>, DtErr>> {
    Ok(lex(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use FieldKind::*;

    fn f(s: &str) -> Vec<(std::string::String, FieldKind)> {
        parse_datetime(s).unwrap().unwrap().into_iter().map(|x| (x.text, x.kind)).collect()
    }
    fn t(s: &str, k: FieldKind) -> (std::string::String, FieldKind) {
        (s.to_string(), k)
    }

    #[test]
    fn core_tokens_sorted() {
        assert!(CORE_TOKENS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn timestamp_with_offset() {
        assert_eq!(
            f("2024-02-29 13:01:02.5+02"),
            vec![t("2024-02-29", Date), t("13:01:02.5", Time), t("+02", Tz)]
        );
    }

    #[test]
    fn spaced_negative_zone() {
        assert_eq!(
            f("1999-01-08 04:05:06 -8:00"),
            vec![t("1999-01-08", Date), t("04:05:06", Time), t("-8:00", Tz)]
        );
    }

    #[test]
    fn text_month() {
        assert_eq!(f("January 8, 1999"), vec![t("january", String), t("8", Number), t("1999", Number)]);
    }

    #[test]
    fn specials() {
        assert_eq!(f("-infinity"), vec![t("-infinity", Special)]);
        assert_eq!(f("+Infinity"), vec![t("+infinity", Special)]);
        assert_eq!(f("EPOCH"), vec![t("epoch", String)]);
    }

    #[test]
    fn too_many_fields() {
        let ok = vec!["1"; 25].join(" ");
        assert_eq!(parse_datetime(&ok).unwrap().unwrap().len(), 25);
        let bad = vec!["1"; 26].join(" ");
        assert_eq!(parse_datetime(&bad).unwrap(), Err(DtErr::BadFormat));
    }

    #[test]
    fn dates_and_numbers() {
        assert_eq!(f("02/29/2024"), vec![t("02/29/2024", Date)]);
        assert_eq!(f("2024.02.29"), vec![t("2024.02.29", Date)]);
        assert_eq!(f("1.5"), vec![t("1.5", Number)]);
        assert_eq!(f(".5"), vec![t(".5", Number)]);
        assert_eq!(f("8-Jan-1999"), vec![t("8-jan-1999", Date)]);
        assert_eq!(f("Jan-08-1999"), vec![t("jan-08-1999", Date)]);
        assert_eq!(f("20011225T040506"), vec![t("20011225", Number), t("t", String), t("040506", Number)]);
    }

    #[test]
    fn zone_names() {
        assert_eq!(f("America/New_York"), vec![t("america/new_york", Date)]);
        assert_eq!(f("PST8PDT"), vec![t("pst8pdt", Date)]);
        assert_eq!(f("t12"), vec![t("t", String), t("12", Number)]);
        assert_eq!(f("12:00 PST"), vec![t("12:00", Time), t("pst", String)]);
    }

    #[test]
    fn punctuation_and_errors() {
        assert_eq!(f("1,2"), vec![t("1", Number), t("2", Number)]);
        assert_eq!(parse_datetime("+ ").unwrap(), Err(DtErr::BadFormat));
        assert_eq!(parse_datetime("1 \u{e9}").unwrap(), Err(DtErr::BadFormat));
        assert_eq!(f(""), vec![]);
        assert_eq!(f("- 5"), vec![t("-5", Tz)]);
    }

    #[test]
    fn work_buffer_limit() {
        let long = "1".repeat(200);
        assert_eq!(parse_datetime(&long).unwrap(), Err(DtErr::BadFormat));
    }
}
