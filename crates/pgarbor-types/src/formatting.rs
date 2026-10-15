//! `to_char`, `to_date`, `to_timestamp(text, text)`, `to_number` (TYPE-13, TYPE-14).
//! Lease TYP-31 (sonnet).
//!
//! PG18 reference: `src/backend/utils/adt/formatting.c` (template parser `parse_format`, keyword
//! tables `DCH_keywords` / `NUM_keywords`, `DCH_to_char`, `DCH_from_char`, `NUM_processor`).
//! Owned procs (ImplModule::Formatting): `timestamp_to_char timestamptz_to_char interval_to_char
//! numeric_to_char int4_to_char int8_to_char float4_to_char float8_to_char to_date to_timestamp
//! numeric_to_number`.
//!
//! Datetime patterns: `HH HH12 HH24 MI SS MS US FF1..FF6 SSSS AM/PM/am/pm/A.M. Y,YYY YYYY YYY YY Y IYYY
//! IYY IY I BC/AD MONTH Month month MON Mon mon MM DAY Day day DY Dy dy DDD IDDD DD D ID W WW IW CC J Q
//! RM rm TZ tz TZH TZM OF`; modifiers `FM` (fill mode: no padding), `TH`/`th` (ordinal suffix), `FX`,
//! `TM` (translation: C locale = English), quoted text `"..."`, backslash escapes. Upper/lower/capitalized
//! keyword variants change the output case and padding (Month is blank-padded to 9 chars unless FM).
//! Number patterns: `9 0 . , D G S PR MI PL SG EEEE V RN TH`, `FM`; overflow of the pattern prints `#`s.
//! Examples (PG docs/oracle): `to_char(timestamp '2002-04-20 17:31:12.66', 'HH12:MI:SS')` = `05:31:12`;
//! `to_char(125.8::real, '999D99')` = ` 125.80`; `to_char(-0.1, 'FM9.99')` = `-.1`;
//! `to_char(485, 'RN')` = `        CDLXXXV`; `to_date('05 Dec 2000', 'DD Mon YYYY')` = `2000-12-05`;
//! `to_number('12,454.8-', '99G999D9S')` = `-12454.8`. Errors follow PG wording, e.g. 22007
//! `invalid value "xx" for "MON"`, 22008 `date/time field value out of range: "..."`.
use crate::catalog_data::BuiltinProc;
use crate::func::FuncImpl;
use pgarbor_common::PgError;

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for a Formatting-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-31", format!("function {}", proc.name))
}
