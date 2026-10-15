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
use crate::context::FuncCtx;
use crate::datetime::calendar;
use crate::datetime::timestamp::adjust_timestamp_precision;
use crate::datetime::tz::{self, ZoneSpec};
use crate::datetime::{
    DATE_END_JULIAN, DATETIME_MIN_JULIAN, DtErr, POSTGRES_EPOCH_JDATE, Tm, dt_parse_error,
};
use crate::datum::{Datum, type_mismatch};
use crate::func::{FuncImpl, ScalarFn};
use crate::numeric::{self, Numeric, arith, funcs as nfuncs, io as nio};
use pgarbor_common::{PgError, Result, SqlState};

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "timestamp_to_char" => timestamp_to_char,
        "timestamptz_to_char" => timestamptz_to_char,
        "interval_to_char" => interval_to_char,
        "numeric_to_char" => numeric_to_char,
        "int4_to_char" => int4_to_char,
        "int8_to_char" => int8_to_char,
        "float4_to_char" => float4_to_char,
        "float8_to_char" => float8_to_char,
        "numeric_to_number" => numeric_to_number,
        "to_date" => to_date,
        "to_timestamp" => to_timestamp,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Formatting-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-31", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Constants and small helpers
// ---------------------------------------------------------------------------------------------

/// Locale symbols for the pinned `C.UTF-8` locale (`NUM_prepare_locale` defaults).
const L_NEGATIVE_SIGN: &str = "-";
const L_POSITIVE_SIGN: &str = "+";
const L_DECIMAL: &str = ".";
const L_THOUSANDS_SEP: &str = ",";
const L_CURRENCY_SYMBOL: &str = " ";

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTHS_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAYS_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const ADBC_STRINGS: [&str; 4] = ["ad", "bc", "AD", "BC"];
const ADBC_STRINGS_LONG: [&str; 4] = ["a.d.", "b.c.", "A.D.", "B.C."];
const AMPM_STRINGS: [&str; 4] = ["am", "pm", "AM", "PM"];
const AMPM_STRINGS_LONG: [&str; 4] = ["a.m.", "p.m.", "A.M.", "P.M."];
const RM_MONTHS_UPPER: [&str; 12] = [
    "XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I",
];
const RM_MONTHS_LOWER: [&str; 12] = [
    "xii", "xi", "x", "ix", "viii", "vii", "vi", "v", "iv", "iii", "ii", "i",
];
const RM1: [&str; 9] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
const RM10: [&str; 9] = ["X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
const RM100: [&str; 9] = ["C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
const MAX_ROMAN_LEN: usize = 15;
const TOKMAXLEN: usize = 10;

// Keyword suffix bits (`DCH_S_*`).
const S_FM: u8 = 0x01;
const S_TH_UP: u8 = 0x02;
const S_TH_LO: u8 = 0x04;
const S_SP: u8 = 0x08;
const S_TM: u8 = 0x10;

fn s_th(sfx: u8) -> bool {
    sfx & (S_TH_UP | S_TH_LO) != 0
}

/// C `isspace` in the C locale.
fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `is_separator_char`: ASCII printable, but not letter or digit.
fn is_separator_char(c: u8) -> bool {
    c > 0x20 && c < 0x7F && !c.is_ascii_alphanumeric()
}

/// Byte length of the UTF-8 character starting at `b[i]` (clipped to the buffer). 0 past the end.
fn mblen(b: &[u8], i: usize) -> usize {
    if i >= b.len() {
        return 0;
    }
    let c = b[i];
    let l = if c < 0x80 {
        1
    } else if c >= 0xF0 {
        4
    } else if c >= 0xE0 {
        3
    } else if c >= 0xC0 {
        2
    } else {
        1
    };
    l.min(b.len() - i)
}

fn bytes_at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn syntax_err(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, msg)
}

fn fmt_err(msg: impl Into<String>) -> PgError {
    PgError::new(SqlState::INVALID_DATETIME_FORMAT, msg)
}

/// `get_th`: ST/ND/RD/TH for the decimal string `num`.
fn get_th(num: &str, upper: bool) -> Result<&'static str> {
    let b = num.as_bytes();
    let len = b.len();
    let mut last = b.last().copied().unwrap_or(0);
    if !last.is_ascii_digit() {
        return Err(PgError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("\"{num}\" is not a number"),
        ));
    }
    if len > 1 && b[len - 2] == b'1' {
        last = 0;
    }
    Ok(match (last, upper) {
        (b'1', true) => "ST",
        (b'1', false) => "st",
        (b'2', true) => "ND",
        (b'2', false) => "nd",
        (b'3', true) => "RD",
        (b'3', false) => "rd",
        (_, true) => "TH",
        (_, false) => "th",
    })
}

/// Apply the TH/th suffix (if any) to an already formatted number.
fn apply_th(sfx: u8, mut s: String) -> Result<String> {
    if s_th(sfx) {
        let t = get_th(&s, sfx & S_TH_UP != 0)?;
        s.push_str(t);
    }
    Ok(s)
}

/// Zero-padded decimal like C `%0*d` (width 0 = no padding; the sign counts toward the width).
fn zpad(v: i64, w: usize) -> String {
    format!("{v:0w$}")
}

fn asc_initcap_like(s: &str, mode: u8) -> String {
    match mode {
        0 => s.to_ascii_uppercase(),
        1 => s.to_string(),
        _ => s.to_ascii_lowercase(),
    }
}

// ---------------------------------------------------------------------------------------------
// Keyword tables
// ---------------------------------------------------------------------------------------------

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dch {
    A_D,
    A_M,
    AD,
    AM,
    B_C,
    BC,
    CC,
    DAY,
    DDD,
    DD,
    DY,
    Day,
    Dy,
    D,
    FF1,
    FF2,
    FF3,
    FF4,
    FF5,
    FF6,
    FX,
    HH24,
    HH12,
    HH,
    IDDD,
    ID,
    IW,
    IYYY,
    IYY,
    IY,
    I,
    J,
    MI,
    MM,
    MONTH,
    MON,
    MS,
    Month,
    Mon,
    OF,
    P_M,
    PM,
    Q,
    RM,
    SSSS,
    SS,
    TZH,
    TZM,
    TZ,
    US,
    WW,
    W,
    Y_YYY,
    YYYY,
    YYY,
    YY,
    Y,
    a_d,
    a_m,
    ad,
    am,
    b_c,
    bc,
    day,
    dy,
    month,
    mon,
    p_m,
    pm,
    rm,
    tz,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DateMode {
    None,
    Gregorian,
    IsoWeek,
}

struct DchKw {
    name: &'static str,
    id: Dch,
    is_digit: bool,
    mode: DateMode,
}

const fn kw(name: &'static str, id: Dch, is_digit: bool, mode: DateMode) -> DchKw {
    DchKw {
        name,
        id,
        is_digit,
        mode,
    }
}

const M0: DateMode = DateMode::None;
const MG: DateMode = DateMode::Gregorian;
const MI_: DateMode = DateMode::IsoWeek;

/// `DCH_keywords` (order matters: longer keywords of one initial come first).
static DCH_KEYWORDS: &[DchKw] = &[
    kw("A.D.", Dch::A_D, false, M0),
    kw("A.M.", Dch::A_M, false, M0),
    kw("AD", Dch::AD, false, M0),
    kw("AM", Dch::AM, false, M0),
    kw("B.C.", Dch::B_C, false, M0),
    kw("BC", Dch::BC, false, M0),
    kw("CC", Dch::CC, true, M0),
    kw("DAY", Dch::DAY, false, M0),
    kw("DDD", Dch::DDD, true, MG),
    kw("DD", Dch::DD, true, MG),
    kw("DY", Dch::DY, false, M0),
    kw("Day", Dch::Day, false, M0),
    kw("Dy", Dch::Dy, false, M0),
    kw("D", Dch::D, true, MG),
    kw("FF1", Dch::FF1, true, M0),
    kw("FF2", Dch::FF2, true, M0),
    kw("FF3", Dch::FF3, true, M0),
    kw("FF4", Dch::FF4, true, M0),
    kw("FF5", Dch::FF5, true, M0),
    kw("FF6", Dch::FF6, true, M0),
    kw("FX", Dch::FX, false, M0),
    kw("HH24", Dch::HH24, true, M0),
    kw("HH12", Dch::HH12, true, M0),
    kw("HH", Dch::HH, true, M0),
    kw("IDDD", Dch::IDDD, true, MI_),
    kw("ID", Dch::ID, true, MI_),
    kw("IW", Dch::IW, true, MI_),
    kw("IYYY", Dch::IYYY, true, MI_),
    kw("IYY", Dch::IYY, true, MI_),
    kw("IY", Dch::IY, true, MI_),
    kw("I", Dch::I, true, MI_),
    kw("J", Dch::J, true, M0),
    kw("MI", Dch::MI, true, M0),
    kw("MM", Dch::MM, true, MG),
    kw("MONTH", Dch::MONTH, false, MG),
    kw("MON", Dch::MON, false, MG),
    kw("MS", Dch::MS, true, M0),
    kw("Month", Dch::Month, false, MG),
    kw("Mon", Dch::Mon, false, MG),
    kw("OF", Dch::OF, false, M0),
    kw("P.M.", Dch::P_M, false, M0),
    kw("PM", Dch::PM, false, M0),
    kw("Q", Dch::Q, true, M0),
    kw("RM", Dch::RM, false, MG),
    kw("SSSSS", Dch::SSSS, true, M0),
    kw("SSSS", Dch::SSSS, true, M0),
    kw("SS", Dch::SS, true, M0),
    kw("TZH", Dch::TZH, false, M0),
    kw("TZM", Dch::TZM, true, M0),
    kw("TZ", Dch::TZ, false, M0),
    kw("US", Dch::US, true, M0),
    kw("WW", Dch::WW, true, MG),
    kw("W", Dch::W, true, MG),
    kw("Y,YYY", Dch::Y_YYY, true, MG),
    kw("YYYY", Dch::YYYY, true, MG),
    kw("YYY", Dch::YYY, true, MG),
    kw("YY", Dch::YY, true, MG),
    kw("Y", Dch::Y, true, MG),
    kw("a.d.", Dch::a_d, false, M0),
    kw("a.m.", Dch::a_m, false, M0),
    kw("ad", Dch::ad, false, M0),
    kw("am", Dch::am, false, M0),
    kw("b.c.", Dch::b_c, false, M0),
    kw("bc", Dch::bc, false, M0),
    kw("cc", Dch::CC, true, M0),
    kw("day", Dch::day, false, M0),
    kw("ddd", Dch::DDD, true, MG),
    kw("dd", Dch::DD, true, MG),
    kw("dy", Dch::dy, false, M0),
    kw("d", Dch::D, true, MG),
    kw("ff1", Dch::FF1, true, M0),
    kw("ff2", Dch::FF2, true, M0),
    kw("ff3", Dch::FF3, true, M0),
    kw("ff4", Dch::FF4, true, M0),
    kw("ff5", Dch::FF5, true, M0),
    kw("ff6", Dch::FF6, true, M0),
    kw("fx", Dch::FX, false, M0),
    kw("hh24", Dch::HH24, true, M0),
    kw("hh12", Dch::HH12, true, M0),
    kw("hh", Dch::HH, true, M0),
    kw("iddd", Dch::IDDD, true, MI_),
    kw("id", Dch::ID, true, MI_),
    kw("iw", Dch::IW, true, MI_),
    kw("iyyy", Dch::IYYY, true, MI_),
    kw("iyy", Dch::IYY, true, MI_),
    kw("iy", Dch::IY, true, MI_),
    kw("i", Dch::I, true, MI_),
    kw("j", Dch::J, true, M0),
    kw("mi", Dch::MI, true, M0),
    kw("mm", Dch::MM, true, MG),
    kw("month", Dch::month, false, MG),
    kw("mon", Dch::mon, false, MG),
    kw("ms", Dch::MS, true, M0),
    kw("of", Dch::OF, false, M0),
    kw("p.m.", Dch::p_m, false, M0),
    kw("pm", Dch::pm, false, M0),
    kw("q", Dch::Q, true, M0),
    kw("rm", Dch::rm, false, MG),
    kw("sssss", Dch::SSSS, true, M0),
    kw("ssss", Dch::SSSS, true, M0),
    kw("ss", Dch::SS, true, M0),
    kw("tzh", Dch::TZH, false, M0),
    kw("tzm", Dch::TZM, true, M0),
    kw("tz", Dch::tz, false, M0),
    kw("us", Dch::US, true, M0),
    kw("ww", Dch::WW, true, MG),
    kw("w", Dch::W, true, MG),
    kw("y,yyy", Dch::Y_YYY, true, MG),
    kw("yyyy", Dch::YYYY, true, MG),
    kw("yyy", Dch::YYY, true, MG),
    kw("yy", Dch::YY, true, MG),
    kw("y", Dch::Y, true, MG),
];

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NumId {
    Comma,
    Dec,
    N0,
    N9,
    B,
    C,
    D,
    E,
    FM,
    G,
    L,
    MI,
    PL,
    PR,
    RN,
    SG,
    SP,
    S,
    TH,
    V,
    rn,
    th,
}

struct NumKw {
    name: &'static str,
    id: NumId,
}

/// `NUM_keywords`.
static NUM_KEYWORDS: &[NumKw] = &[
    NumKw {
        name: ",",
        id: NumId::Comma,
    },
    NumKw {
        name: ".",
        id: NumId::Dec,
    },
    NumKw {
        name: "0",
        id: NumId::N0,
    },
    NumKw {
        name: "9",
        id: NumId::N9,
    },
    NumKw {
        name: "B",
        id: NumId::B,
    },
    NumKw {
        name: "C",
        id: NumId::C,
    },
    NumKw {
        name: "D",
        id: NumId::D,
    },
    NumKw {
        name: "EEEE",
        id: NumId::E,
    },
    NumKw {
        name: "FM",
        id: NumId::FM,
    },
    NumKw {
        name: "G",
        id: NumId::G,
    },
    NumKw {
        name: "L",
        id: NumId::L,
    },
    NumKw {
        name: "MI",
        id: NumId::MI,
    },
    NumKw {
        name: "PL",
        id: NumId::PL,
    },
    NumKw {
        name: "PR",
        id: NumId::PR,
    },
    NumKw {
        name: "RN",
        id: NumId::RN,
    },
    NumKw {
        name: "SG",
        id: NumId::SG,
    },
    NumKw {
        name: "SP",
        id: NumId::SP,
    },
    NumKw {
        name: "S",
        id: NumId::S,
    },
    NumKw {
        name: "TH",
        id: NumId::TH,
    },
    NumKw {
        name: "V",
        id: NumId::V,
    },
    NumKw {
        name: "b",
        id: NumId::B,
    },
    NumKw {
        name: "c",
        id: NumId::C,
    },
    NumKw {
        name: "d",
        id: NumId::D,
    },
    NumKw {
        name: "eeee",
        id: NumId::E,
    },
    NumKw {
        name: "fm",
        id: NumId::FM,
    },
    NumKw {
        name: "g",
        id: NumId::G,
    },
    NumKw {
        name: "l",
        id: NumId::L,
    },
    NumKw {
        name: "mi",
        id: NumId::MI,
    },
    NumKw {
        name: "pl",
        id: NumId::PL,
    },
    NumKw {
        name: "pr",
        id: NumId::PR,
    },
    NumKw {
        name: "rn",
        id: NumId::rn,
    },
    NumKw {
        name: "sg",
        id: NumId::SG,
    },
    NumKw {
        name: "sp",
        id: NumId::SP,
    },
    NumKw {
        name: "s",
        id: NumId::S,
    },
    NumKw {
        name: "th",
        id: NumId::th,
    },
    NumKw {
        name: "v",
        id: NumId::V,
    },
];

// ---------------------------------------------------------------------------------------------
// Format-string parser (`parse_format`)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LitKind {
    Char,
    Separator,
    Space,
}

#[derive(Clone, Debug)]
enum Node {
    /// Keyword node: index into `DCH_KEYWORDS` / `NUM_KEYWORDS` plus suffix bits.
    Action { key: usize, suffix: u8 },
    /// Literal text (one character).
    Lit { kind: LitKind, text: String },
}

fn find_dch_keyword(rest: &[u8]) -> Option<usize> {
    DCH_KEYWORDS
        .iter()
        .position(|k| rest.starts_with(k.name.as_bytes()))
}

fn find_num_keyword(rest: &[u8]) -> Option<usize> {
    NUM_KEYWORDS
        .iter()
        .position(|k| rest.starts_with(k.name.as_bytes()))
}

/// `suff_search` over the DCH suffix table; `prefix` selects the prefix (FM, TM) or postfix (TH, SP) kind.
fn suff_search(rest: &[u8], prefix: bool) -> Option<(u8, usize)> {
    const SUFFIXES: [(&str, u8, bool); 7] = [
        ("FM", S_FM, true),
        ("fm", S_FM, true),
        ("TM", S_TM, true),
        ("tm", S_TM, true),
        ("TH", S_TH_UP, false),
        ("th", S_TH_LO, false),
        ("SP", S_SP, false),
    ];
    for (name, bit, is_prefix) in SUFFIXES.iter() {
        if *is_prefix == prefix && rest.starts_with(name.as_bytes()) {
            return Some((*bit, name.len()));
        }
    }
    None
}

/// Port of `parse_format`. `dch` selects the datetime tables (with FM/TM/TH/SP suffixes), otherwise
/// the number tables; `prep` is invoked for each keyword of a number picture (`NUMDesc_prepare`).
fn parse_format(
    fmt: &str,
    dch: bool,
    prep: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<Vec<Node>> {
    let b = fmt.as_bytes();
    let mut i = 0usize;
    let mut nodes: Vec<Node> = Vec::with_capacity(b.len());
    while i < b.len() {
        let mut suffix = 0u8;
        if dch {
            if let Some((bit, len)) = suff_search(&b[i..], true) {
                suffix |= bit;
                i += len;
            }
        }
        let key = if i < b.len() {
            if dch {
                find_dch_keyword(&b[i..])
            } else {
                find_num_keyword(&b[i..])
            }
        } else {
            None
        };
        if let Some(k) = key {
            let klen = if dch {
                DCH_KEYWORDS[k].name.len()
            } else {
                NUM_KEYWORDS[k].name.len()
            };
            i += klen;
            if !dch {
                prep(k)?;
            }
            if dch && i < b.len() {
                if let Some((bit, len)) = suff_search(&b[i..], false) {
                    suffix |= bit;
                    i += len;
                }
            }
            nodes.push(Node::Action { key: k, suffix });
        } else if i < b.len() {
            if b[i] == b'"' {
                // Double-quoted literal string.
                i += 1;
                while i < b.len() {
                    if b[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    let chlen = mblen(b, i);
                    nodes.push(Node::Lit {
                        kind: LitKind::Char,
                        text: lossy(&b[i..i + chlen]),
                    });
                    i += chlen;
                }
            } else {
                // Outside quotes a backslash is only special in front of a double quote.
                if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 1;
                }
                let chlen = mblen(b, i);
                let kind = if dch && is_separator_char(b[i]) {
                    LitKind::Separator
                } else if c_isspace(b[i]) {
                    LitKind::Space
                } else {
                    LitKind::Char
                };
                nodes.push(Node::Lit {
                    kind,
                    text: lossy(&b[i..i + chlen]),
                });
                i += chlen;
            }
        }
    }
    Ok(nodes)
}

fn parse_dch_format(fmt: &str) -> Result<Vec<Node>> {
    parse_format(fmt, true, &mut |_| Ok(()))
}

// ---------------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------------

fn arg<'a>(args: &'a [Datum], i: usize) -> Result<&'a Datum> {
    args.get(i)
        .ok_or_else(|| PgError::internal(format!("formatting function: missing argument {i}")))
}

fn arg_text<'a>(args: &'a [Datum], i: usize) -> Result<&'a str> {
    match arg(args, i)? {
        Datum::Text(s) => Ok(s.as_str()),
        other => Err(type_mismatch("text", other)),
    }
}

// ---------------------------------------------------------------------------------------------
// Datetime to_char (`DCH_to_char`)
// ---------------------------------------------------------------------------------------------

/// Broken-down input of `DCH_to_char` (`TmToChar`).
#[derive(Clone, Debug, Default)]
struct TmToChar {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i64,
    min: i32,
    sec: i32,
    yday: i32,
    wday: i32,
    /// Seconds EAST of UTC.
    gmtoff: i32,
    fsec: i32,
    tzn: Option<String>,
}

impl TmToChar {
    fn zero() -> TmToChar {
        TmToChar {
            mon: 1,
            mday: 1,
            ..TmToChar::default()
        }
    }
    fn from_tm(t: &Tm) -> TmToChar {
        TmToChar {
            year: t.year,
            mon: t.mon,
            mday: t.mday,
            hour: i64::from(t.hour),
            min: t.min,
            sec: t.sec,
            yday: t.yday,
            wday: t.wday,
            gmtoff: 0,
            fsec: t.fsec,
            tzn: None,
        }
    }
}

/// `ADJUST_YEAR`.
fn adjust_year(year: i32, is_interval: bool) -> i32 {
    if is_interval {
        year
    } else if year <= 0 {
        year.wrapping_sub(1).wrapping_neg()
    } else {
        year
    }
}

fn invalid_for_interval() -> PgError {
    fmt_err("invalid format specification for an interval value")
        .with_hint("Intervals are not tied to specific calendar dates.")
}

fn dch_to_char(nodes: &[Node], is_interval: bool, tm: &TmToChar) -> Result<String> {
    let mut out = String::new();
    let pm = tm.hour % 24 >= 12;
    for n in nodes {
        let (ki, sfx) = match n {
            Node::Lit { text, .. } => {
                out.push_str(text);
                continue;
            }
            Node::Action { key, suffix } => (*key, *suffix),
        };
        let id = DCH_KEYWORDS[ki].id;
        let fm = sfx & S_FM != 0;
        let tmode = sfx & S_TM != 0;
        // Fixed-width number helper: width for FM / non-negative / negative values.
        let w = |fm: bool, neg_ok: bool, pos: usize, neg: usize| -> usize {
            if fm {
                0
            } else if neg_ok {
                pos
            } else {
                neg
            }
        };
        macro_rules! no_interval {
            () => {
                if is_interval {
                    return Err(invalid_for_interval());
                }
            };
        }
        match id {
            Dch::A_M | Dch::P_M => out.push_str(if pm { "P.M." } else { "A.M." }),
            Dch::AM | Dch::PM => out.push_str(if pm { "PM" } else { "AM" }),
            Dch::a_m | Dch::p_m => out.push_str(if pm { "p.m." } else { "a.m." }),
            Dch::am | Dch::pm => out.push_str(if pm { "pm" } else { "am" }),
            Dch::HH | Dch::HH12 => {
                let h = tm.hour % 12;
                let v = if h == 0 { 12 } else { h };
                let t = zpad(v, w(fm, tm.hour >= 0, 2, 3));
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::HH24 => {
                let t = zpad(tm.hour, w(fm, tm.hour >= 0, 2, 3));
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::MI => {
                let t = zpad(i64::from(tm.min), w(fm, tm.min >= 0, 2, 3));
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::SS => {
                let t = zpad(i64::from(tm.sec), w(fm, tm.sec >= 0, 2, 3));
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::FF1 => out.push_str(&apply_th(sfx, format!("{:01}", tm.fsec / 100_000))?),
            Dch::FF2 => out.push_str(&apply_th(sfx, format!("{:02}", tm.fsec / 10_000))?),
            Dch::FF3 | Dch::MS => out.push_str(&apply_th(sfx, format!("{:03}", tm.fsec / 1_000))?),
            Dch::FF4 => out.push_str(&apply_th(sfx, format!("{:04}", tm.fsec / 100))?),
            Dch::FF5 => out.push_str(&apply_th(sfx, format!("{:05}", tm.fsec / 10))?),
            Dch::FF6 | Dch::US => out.push_str(&apply_th(sfx, format!("{:06}", tm.fsec))?),
            Dch::SSSS => {
                let v = tm.hour * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec);
                out.push_str(&apply_th(sfx, v.to_string())?);
            }
            Dch::tz => {
                no_interval!();
                if let Some(z) = &tm.tzn {
                    out.push_str(&z.to_ascii_lowercase());
                }
            }
            Dch::TZ => {
                no_interval!();
                if let Some(z) = &tm.tzn {
                    out.push_str(z);
                }
            }
            Dch::TZH => {
                no_interval!();
                let sign = if tm.gmtoff >= 0 { '+' } else { '-' };
                out.push_str(&format!("{}{:02}", sign, tm.gmtoff.unsigned_abs() / 3600));
            }
            Dch::TZM => {
                no_interval!();
                out.push_str(&format!("{:02}", (tm.gmtoff.unsigned_abs() % 3600) / 60));
            }
            Dch::OF => {
                no_interval!();
                let sign = if tm.gmtoff >= 0 { '+' } else { '-' };
                let width = if fm { 0 } else { 2 };
                let a = tm.gmtoff.unsigned_abs();
                out.push(sign);
                out.push_str(&format!("{:0width$}", a / 3600, width = width));
                if a % 3600 != 0 {
                    out.push_str(&format!(":{:02}", (a % 3600) / 60));
                }
            }
            Dch::A_D | Dch::B_C => {
                no_interval!();
                out.push_str(if tm.year <= 0 { "B.C." } else { "A.D." });
            }
            Dch::AD | Dch::BC => {
                no_interval!();
                out.push_str(if tm.year <= 0 { "BC" } else { "AD" });
            }
            Dch::a_d | Dch::b_c => {
                no_interval!();
                out.push_str(if tm.year <= 0 { "b.c." } else { "a.d." });
            }
            Dch::ad | Dch::bc => {
                no_interval!();
                out.push_str(if tm.year <= 0 { "bc" } else { "ad" });
            }
            Dch::MONTH | Dch::Month | Dch::month => {
                no_interval!();
                if tm.mon == 0 {
                    continue;
                }
                let Some(name) = MONTHS_FULL.get((tm.mon - 1) as usize) else {
                    continue;
                };
                let mode = match id {
                    Dch::MONTH => 0,
                    Dch::Month => 1,
                    _ => 2,
                };
                let t = asc_initcap_like(name, mode);
                if fm || tmode {
                    out.push_str(&t);
                } else {
                    out.push_str(&format!("{t:<9}"));
                }
            }
            Dch::MON | Dch::Mon | Dch::mon => {
                no_interval!();
                if tm.mon == 0 {
                    continue;
                }
                let Some(name) = MONTHS_ABBR.get((tm.mon - 1) as usize) else {
                    continue;
                };
                let mode = match id {
                    Dch::MON => 0,
                    Dch::Mon => 1,
                    _ => 2,
                };
                out.push_str(&asc_initcap_like(name, mode));
            }
            Dch::MM => {
                let t = zpad(i64::from(tm.mon), w(fm, tm.mon >= 0, 2, 3));
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::DAY | Dch::Day | Dch::day => {
                no_interval!();
                let Some(name) = DAYS_FULL.get(tm.wday as usize) else {
                    continue;
                };
                let mode = match id {
                    Dch::DAY => 0,
                    Dch::Day => 1,
                    _ => 2,
                };
                let t = asc_initcap_like(name, mode);
                if fm || tmode {
                    out.push_str(&t);
                } else {
                    out.push_str(&format!("{t:<9}"));
                }
            }
            Dch::DY | Dch::Dy | Dch::dy => {
                no_interval!();
                let Some(name) = DAYS_SHORT.get(tm.wday as usize) else {
                    continue;
                };
                let mode = match id {
                    Dch::DY => 0,
                    Dch::Dy => 1,
                    _ => 2,
                };
                out.push_str(&asc_initcap_like(name, mode));
            }
            Dch::DDD | Dch::IDDD => {
                let v = if id == Dch::DDD {
                    tm.yday
                } else {
                    calendar::date2isoyearday(tm.year, tm.mon, tm.mday)?
                };
                out.push_str(&apply_th(sfx, zpad(i64::from(v), if fm { 0 } else { 3 }))?);
            }
            Dch::DD => {
                out.push_str(&apply_th(
                    sfx,
                    zpad(i64::from(tm.mday), if fm { 0 } else { 2 }),
                )?);
            }
            Dch::D => {
                no_interval!();
                out.push_str(&apply_th(sfx, (tm.wday + 1).to_string())?);
            }
            Dch::ID => {
                no_interval!();
                let v = if tm.wday == 0 { 7 } else { tm.wday };
                out.push_str(&apply_th(sfx, v.to_string())?);
            }
            Dch::WW => {
                let v = tm.yday.wrapping_sub(1) / 7 + 1;
                out.push_str(&apply_th(sfx, zpad(i64::from(v), if fm { 0 } else { 2 }))?);
            }
            Dch::IW => {
                let v = calendar::date2isoweek(tm.year, tm.mon, tm.mday)?;
                out.push_str(&apply_th(sfx, zpad(i64::from(v), if fm { 0 } else { 2 }))?);
            }
            Dch::Q => {
                if tm.mon == 0 {
                    continue;
                }
                out.push_str(&apply_th(
                    sfx,
                    (tm.mon.wrapping_sub(1) / 3 + 1).to_string(),
                )?);
            }
            Dch::CC => {
                let i = if is_interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year - 1) / 100 + 1
                } else {
                    tm.year / 100 - 1
                };
                let t = if (-99..=99).contains(&i) {
                    zpad(i64::from(i), w(fm, i >= 0, 2, 3))
                } else {
                    i.to_string()
                };
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::Y_YYY => {
                let y = adjust_year(tm.year, is_interval);
                let i = y / 1000;
                let t = format!("{},{:03}", i, y - i * 1000);
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::YYYY | Dch::IYYY | Dch::YYY | Dch::IYY | Dch::YY | Dch::IY | Dch::Y | Dch::I => {
                let iso = matches!(id, Dch::IYYY | Dch::IYY | Dch::IY | Dch::I);
                let y = if iso {
                    adjust_year(
                        calendar::date2isoyear(tm.year, tm.mon, tm.mday)?,
                        is_interval,
                    )
                } else {
                    adjust_year(tm.year, is_interval)
                };
                let adj_pos = adjust_year(tm.year, is_interval) >= 0;
                let t = match id {
                    Dch::YYYY | Dch::IYYY => zpad(i64::from(y), w(fm, adj_pos, 4, 5)),
                    Dch::YYY | Dch::IYY => zpad(i64::from(y % 1000), w(fm, adj_pos, 3, 4)),
                    Dch::YY | Dch::IY => zpad(i64::from(y % 100), w(fm, adj_pos, 2, 3)),
                    _ => format!("{}", y % 10),
                };
                out.push_str(&apply_th(sfx, t)?);
            }
            Dch::RM | Dch::rm => {
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                let months: &[&str; 12] = if id == Dch::RM {
                    &RM_MONTHS_UPPER
                } else {
                    &RM_MONTHS_LOWER
                };
                let mon = if tm.mon == 0 {
                    if tm.year >= 0 { 0 } else { 11 }
                } else if tm.mon < 0 {
                    (-(tm.mon + 1)) as usize
                } else {
                    (12 - tm.mon) as usize
                };
                let Some(name) = months.get(mon) else {
                    continue;
                };
                if fm {
                    out.push_str(name);
                } else {
                    out.push_str(&format!("{name:<4}"));
                }
            }
            Dch::W => {
                out.push_str(&apply_th(
                    sfx,
                    (tm.mday.wrapping_sub(1) / 7 + 1).to_string(),
                )?);
            }
            Dch::J => {
                out.push_str(&apply_th(
                    sfx,
                    calendar::date2j(tm.year, tm.mon, tm.mday).to_string(),
                )?);
            }
            Dch::FX => {}
        }
    }
    Ok(out)
}

fn timestamp_arg(args: &[Datum], i: usize, tz: bool) -> Result<i64> {
    match (arg(args, i)?, tz) {
        (Datum::Timestamp(v), false) => Ok(*v),
        (Datum::TimestampTz(v), true) => Ok(*v),
        (other, false) => Err(type_mismatch("timestamp", other)),
        (other, true) => Err(type_mismatch("timestamptz", other)),
    }
}

fn timestamp_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let ts = timestamp_arg(args, 0, false)?;
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() || ts == i64::MAX || ts == i64::MIN {
        return Ok(Datum::Null);
    }
    let t = calendar::timestamp2tm(ts, None)?;
    let tc = TmToChar::from_tm(&t);
    let nodes = parse_dch_format(fmt)?;
    Ok(Datum::Text(dch_to_char(&nodes, false, &tc)?))
}

fn timestamptz_to_char(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let ts = timestamp_arg(args, 0, true)?;
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() || ts == i64::MAX || ts == i64::MIN {
        return Ok(Datum::Null);
    }
    let east = tz::offset_at_utc(&ctx.io.time_zone, ts)?;
    let abbrev = tz::abbrev_at_utc(&ctx.io.time_zone, ts)?;
    let t = calendar::timestamp2tm(ts, Some(-east))?;
    let mut tc = TmToChar::from_tm(&t);
    tc.gmtoff = east;
    tc.tzn = Some(abbrev);
    let nodes = parse_dch_format(fmt)?;
    Ok(Datum::Text(dch_to_char(&nodes, false, &tc)?))
}

fn interval_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let iv = match arg(args, 0)? {
        Datum::Interval(v) => *v,
        other => return Err(type_mismatch("interval", other)),
    };
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() || iv.is_infinite() {
        return Ok(Datum::Null);
    }
    let itm = calendar::interval2itm(&iv)?;
    let mut tc = TmToChar::zero();
    tc.fsec = itm.usec;
    tc.sec = itm.sec;
    tc.min = itm.min;
    tc.hour = itm.hour;
    tc.mday = itm.mday;
    tc.mon = itm.mon;
    tc.year = itm.year;
    // wday is meaningless, yday approximates the total span in days.
    tc.yday = ((i64::from(itm.year) * 12 + i64::from(itm.mon)) * 30 + i64::from(itm.mday)) as i32;
    let nodes = parse_dch_format(fmt)?;
    Ok(Datum::Text(dch_to_char(&nodes, true, &tc)?))
}

// ---------------------------------------------------------------------------------------------
// Datetime from-char (`DCH_from_char`, `do_to_timestamp`)
// ---------------------------------------------------------------------------------------------

/// Parsed fields (`TmFromChar`).
#[derive(Clone, Debug, Default)]
struct FromChar {
    mode: Option<DateMode>,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock12: bool,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
    has_tz: bool,
    /// Seconds EAST of UTC for a TZ abbreviation.
    gmtoffset: i32,
}

fn strspace_len(b: &[u8], s: usize) -> usize {
    let mut n = 0;
    while s + n < b.len() && c_isspace(b[s + n]) {
        n += 1;
    }
    n
}

/// C `strtol(base 10)` starting at `start`: (value, end index, ERANGE). No digits -> end == start.
fn strtol(b: &[u8], start: usize) -> (i64, usize, bool) {
    let mut i = start;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let ds = i;
    let mut acc: i128 = 0;
    let mut range = false;
    while i < b.len() && b[i].is_ascii_digit() {
        if !range {
            acc = acc * 10 + i128::from(b[i] - b'0');
            if acc > i128::from(i64::MAX) + 1 {
                range = true;
            }
        }
        i += 1;
    }
    if i == ds {
        return (0, start, false);
    }
    let v = if neg { -acc } else { acc };
    if range || v > i128::from(i64::MAX) || v < i128::from(i64::MIN) {
        return (if neg { i64::MIN } else { i64::MAX }, i, true);
    }
    (v as i64, i, false)
}

fn from_char_set_mode(fc: &mut FromChar, mode: DateMode) -> Result<()> {
    if mode != DateMode::None {
        match fc.mode {
            None => fc.mode = Some(mode),
            Some(m) if m != mode => {
                return Err(fmt_err("invalid combination of date conventions")
                    .with_hint("Do not mix Gregorian and ISO week date conventions in a formatting template."));
            }
            _ => {}
        }
    }
    Ok(())
}

fn from_char_set_int(dest: &mut i32, value: i32, name: &str) -> Result<()> {
    if *dest != 0 && *dest != value {
        return Err(fmt_err(format!(
            "conflicting values for \"{name}\" field in formatting string"
        ))
        .with_detail("This value contradicts a previous setting for the same field type."));
    }
    *dest = value;
    Ok(())
}

/// `from_char_parse_int_len`; returns the number of characters consumed (including skipped blanks).
fn parse_int_len(
    dest: Option<&mut i32>,
    inb: &[u8],
    s: &mut usize,
    len: usize,
    name: &str,
    slurp: bool,
) -> Result<usize> {
    let init = *s;
    *s += strspace_len(inb, *s);
    let rest = &inb[(*s).min(inb.len())..];
    let used_total = rest.len();
    let copy = &rest[..used_total.min(len)];
    let result: i64;
    let erange: bool;
    if slurp {
        let (v, end, er) = strtol(inb, init);
        result = v;
        erange = er;
        *s = end;
    } else {
        if used_total < len {
            return Err(fmt_err(format!(
                "source string too short for \"{name}\" formatting field"
            ))
            .with_detail(format!(
                "Field requires {len} characters, but only {used_total} remain."
            ))
            .with_hint(
                "If your source string is not fixed-width, try using the \"FM\" modifier.",
            ));
        }
        let (v, end, er) = strtol(copy, 0);
        let used = end;
        if used > 0 && used < len {
            return Err(
                fmt_err(format!("invalid value \"{}\" for \"{name}\"", lossy(copy)))
                    .with_detail(format!(
                        "Field requires {len} characters, but only {used} could be parsed."
                    ))
                    .with_hint(
                        "If your source string is not fixed-width, try using the \"FM\" modifier.",
                    ),
            );
        }
        result = v;
        erange = er;
        *s += used;
    }
    if *s == init {
        return Err(
            fmt_err(format!("invalid value \"{}\" for \"{name}\"", lossy(copy)))
                .with_detail("Value must be an integer."),
        );
    }
    if erange || result < i64::from(i32::MIN) || result > i64::from(i32::MAX) {
        return Err(PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("value for \"{name}\" in source string is out of range"),
        )
        .with_detail(format!(
            "Value must be in the range {} to {}.",
            i32::MIN,
            i32::MAX
        )));
    }
    if let Some(d) = dest {
        from_char_set_int(d, result as i32, name)?;
    }
    Ok(*s - init)
}

/// `seq_search_ascii`: case-insensitive prefix match against `array`; (index, matched length).
fn seq_search_ascii(name: &[u8], array: &[&str]) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    let firstc = name[0].to_ascii_lowercase();
    for (idx, a) in array.iter().enumerate() {
        let ab = a.as_bytes();
        if ab.is_empty() || ab[0].to_ascii_lowercase() != firstc {
            continue;
        }
        let mut p = 1usize;
        let mut n = 1usize;
        loop {
            if p >= ab.len() {
                return Some((idx, n));
            }
            if n >= name.len() {
                break;
            }
            if ab[p].to_ascii_lowercase() != name[n].to_ascii_lowercase() {
                break;
            }
            p += 1;
            n += 1;
        }
    }
    None
}

fn from_char_seq_search(inb: &[u8], s: &mut usize, array: &[&str], name: &str) -> Result<usize> {
    let rest = &inb[(*s).min(inb.len())..];
    match seq_search_ascii(rest, array) {
        Some((idx, len)) => {
            *s += len;
            Ok(idx)
        }
        None => {
            let end = rest
                .iter()
                .position(|c| c_isspace(*c))
                .unwrap_or(rest.len());
            Err(fmt_err(format!(
                "invalid value \"{}\" for \"{name}\"",
                lossy(&rest[..end])
            ))
            .with_detail("The given value did not match any of the allowed values for this field."))
        }
    }
}

fn skip_thth(inb: &[u8], s: &mut usize, sfx: u8) {
    if s_th(sfx) {
        for _ in 0..2 {
            if bytes_at(inb, *s) != 0 {
                *s += mblen(inb, *s).max(1);
            }
        }
    }
}

/// `is_next_separator` for node `i`.
fn is_next_separator(nodes: &[Node], i: usize) -> bool {
    let Some(cur) = nodes.get(i) else {
        return false;
    };
    if let Node::Action { suffix, .. } = cur {
        if s_th(*suffix) {
            return true;
        }
    }
    match nodes.get(i + 1) {
        None => true,
        Some(Node::Action { key, .. }) => !DCH_KEYWORDS[*key].is_digit,
        Some(Node::Lit { text, .. }) => {
            let tb = text.as_bytes();
            !(tb.len() == 1 && tb[0].is_ascii_digit())
        }
    }
}

fn adjust_partial_year_to_2020(year: i32) -> i32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

/// `DecodeTimezoneAbbrevPrefix`: (length, seconds east) of a recognised abbreviation at the start of `rest`.
fn decode_tz_abbrev_prefix(rest: &[u8]) -> Result<Option<(usize, i32)>> {
    let mut len = 0usize;
    while len < TOKMAXLEN && len < rest.len() && rest[len].is_ascii_alphabetic() {
        len += 1;
    }
    let low: Vec<u8> = rest[..len].iter().map(|c| c.to_ascii_lowercase()).collect();
    while len > 0 {
        let word = lossy(&low[..len]);
        match tz::lookup_zone(&word)? {
            Ok(ZoneSpec::Fixed { secs_east, .. }) => return Ok(Some((len, secs_east))),
            Err(e @ DtErr::UnsupportedTimezone(_)) => {
                return Err(dt_parse_error(&e, &word, "timestamp"));
            }
            _ => {}
        }
        len -= 1;
    }
    Ok(None)
}

/// Minimal `sscanf("%d,%03d%n")`: returns (millennia, years, chars consumed) when both fields match.
fn scan_y_yyy(b: &[u8]) -> Option<(i64, i64, usize)> {
    let mut i = 0usize;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    let (m, end, _) = strtol(b, i);
    if end == i {
        return None;
    }
    i = end;
    if bytes_at(b, i) != b',' {
        return None;
    }
    i += 1;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    // %03d: at most three characters, sign included.
    let start = i;
    let limit = (i + 3).min(b.len());
    let (y, end, _) = strtol(&b[..limit], start);
    if end == start {
        return None;
    }
    Some((m, y, end))
}

fn dch_from_char(nodes: &[Node], input: &[u8]) -> Result<FromChar> {
    let mut out = FromChar::default();
    let mut s = 0usize;
    let mut fx_mode = false;
    let mut extra_skip: i32 = 0;
    let mut ni = 0usize;
    while ni < nodes.len() && bytes_at(input, s) != 0 {
        let n = &nodes[ni];
        let is_action = matches!(n, Node::Action { .. });
        let is_fx = matches!(n, Node::Action { key, .. } if DCH_KEYWORDS[*key].id == Dch::FX);
        if !fx_mode && !is_fx && (is_action || ni == 0) {
            while bytes_at(input, s) != 0 && c_isspace(input[s]) {
                s += 1;
                extra_skip += 1;
            }
        }
        let (ki, sfx) = match n {
            Node::Lit {
                kind: LitKind::Space | LitKind::Separator,
                ..
            } => {
                if !fx_mode {
                    extra_skip -= 1;
                    let c = bytes_at(input, s);
                    if c_isspace(c) || is_separator_char(c) {
                        s += 1;
                        extra_skip += 1;
                    }
                } else {
                    s += mblen(input, s);
                }
                ni += 1;
                continue;
            }
            Node::Lit {
                kind: LitKind::Char,
                ..
            } => {
                if !fx_mode {
                    if extra_skip > 0 {
                        extra_skip -= 1;
                    } else {
                        s += mblen(input, s);
                    }
                } else {
                    s += mblen(input, s);
                }
                ni += 1;
                continue;
            }
            Node::Action { key, suffix } => (*key, *suffix),
        };
        let kwd = &DCH_KEYWORDS[ki];
        let name = kwd.name;
        let klen = name.len();
        let slurp = (sfx & S_FM != 0) || is_next_separator(nodes, ni);
        from_char_set_mode(&mut out, kwd.mode)?;
        match kwd.id {
            Dch::FX => fx_mode = true,
            Dch::A_M | Dch::P_M | Dch::a_m | Dch::p_m => {
                let v = from_char_seq_search(input, &mut s, &AMPM_STRINGS_LONG, name)?;
                from_char_set_int(&mut out.pm, (v % 2) as i32, name)?;
                out.clock12 = true;
            }
            Dch::AM | Dch::PM | Dch::am | Dch::pm => {
                let v = from_char_seq_search(input, &mut s, &AMPM_STRINGS, name)?;
                from_char_set_int(&mut out.pm, (v % 2) as i32, name)?;
                out.clock12 = true;
            }
            Dch::HH | Dch::HH12 => {
                parse_int_len(Some(&mut out.hh), input, &mut s, 2, name, slurp)?;
                out.clock12 = true;
                skip_thth(input, &mut s, sfx);
            }
            Dch::HH24 => {
                parse_int_len(Some(&mut out.hh), input, &mut s, 2, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::MI => {
                parse_int_len(Some(&mut out.mi), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::SS => {
                parse_int_len(Some(&mut out.ss), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::MS => {
                let len = parse_int_len(Some(&mut out.ms), input, &mut s, 3, name, slurp)?;
                out.ms = out.ms.wrapping_mul(if len == 1 {
                    100
                } else if len == 2 {
                    10
                } else {
                    1
                });
                skip_thth(input, &mut s, sfx);
            }
            Dch::FF1 | Dch::FF2 | Dch::FF3 | Dch::FF4 | Dch::FF5 | Dch::FF6 | Dch::US => {
                let width = match kwd.id {
                    Dch::US => 6,
                    Dch::FF1 => {
                        out.ff = 1;
                        1
                    }
                    Dch::FF2 => {
                        out.ff = 2;
                        2
                    }
                    Dch::FF3 => {
                        out.ff = 3;
                        3
                    }
                    Dch::FF4 => {
                        out.ff = 4;
                        4
                    }
                    Dch::FF5 => {
                        out.ff = 5;
                        5
                    }
                    _ => {
                        out.ff = 6;
                        6
                    }
                };
                let len = parse_int_len(Some(&mut out.us), input, &mut s, width, name, slurp)?;
                out.us = out.us.wrapping_mul(match len {
                    1 => 100_000,
                    2 => 10_000,
                    3 => 1_000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                });
                skip_thth(input, &mut s, sfx);
            }
            Dch::SSSS => {
                parse_int_len(Some(&mut out.ssss), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::tz | Dch::TZ | Dch::OF => {
                let mut handled = false;
                if matches!(kwd.id, Dch::tz | Dch::TZ) {
                    let rest = &input[s.min(input.len())..];
                    match decode_tz_abbrev_prefix(rest)? {
                        Some((tzlen, east)) => {
                            out.has_tz = true;
                            out.gmtoffset = east;
                            out.tzsign = 0;
                            s += tzlen;
                            handled = true;
                        }
                        None => {
                            if bytes_at(input, s).is_ascii_alphabetic() {
                                return Err(fmt_err(format!(
                                    "invalid value \"{}\" for \"{name}\"",
                                    lossy(rest)
                                ))
                                .with_detail("Time zone abbreviation is not recognized."));
                            }
                        }
                    }
                }
                if !handled {
                    let c = bytes_at(input, s);
                    if c == b'+' || c == b'-' || c == b' ' {
                        out.tzsign = if c == b'-' { -1 } else { 1 };
                        s += 1;
                    } else if extra_skip > 0 && s > 0 && input[s - 1] == b'-' {
                        out.tzsign = -1;
                    } else {
                        out.tzsign = 1;
                    }
                    parse_int_len(Some(&mut out.tzh), input, &mut s, 2, name, slurp)?;
                    if bytes_at(input, s) == b':' {
                        s += 1;
                        parse_int_len(Some(&mut out.tzm), input, &mut s, 2, name, slurp)?;
                    }
                }
            }
            Dch::TZH => {
                let c = bytes_at(input, s);
                if c == b'+' || c == b'-' || c == b' ' {
                    out.tzsign = if c == b'-' { -1 } else { 1 };
                    s += 1;
                } else if extra_skip > 0 && s > 0 && input[s - 1] == b'-' {
                    out.tzsign = -1;
                } else {
                    out.tzsign = 1;
                }
                parse_int_len(Some(&mut out.tzh), input, &mut s, 2, name, slurp)?;
            }
            Dch::TZM => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                parse_int_len(Some(&mut out.tzm), input, &mut s, 2, name, slurp)?;
            }
            Dch::A_D | Dch::B_C | Dch::a_d | Dch::b_c => {
                let v = from_char_seq_search(input, &mut s, &ADBC_STRINGS_LONG, name)?;
                from_char_set_int(&mut out.bc, (v % 2) as i32, name)?;
            }
            Dch::AD | Dch::BC | Dch::ad | Dch::bc => {
                let v = from_char_seq_search(input, &mut s, &ADBC_STRINGS, name)?;
                from_char_set_int(&mut out.bc, (v % 2) as i32, name)?;
            }
            Dch::MONTH | Dch::Month | Dch::month => {
                let v = from_char_seq_search(input, &mut s, &MONTHS_FULL, name)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, name)?;
            }
            Dch::MON | Dch::Mon | Dch::mon => {
                let v = from_char_seq_search(input, &mut s, &MONTHS_ABBR, name)?;
                from_char_set_int(&mut out.mm, v as i32 + 1, name)?;
            }
            Dch::MM => {
                parse_int_len(Some(&mut out.mm), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::DAY | Dch::Day | Dch::day => {
                let v = from_char_seq_search(input, &mut s, &DAYS_FULL, name)?;
                from_char_set_int(&mut out.d, v as i32, name)?;
                out.d = out.d.wrapping_add(1);
            }
            Dch::DY | Dch::Dy | Dch::dy => {
                let v = from_char_seq_search(input, &mut s, &DAYS_SHORT, name)?;
                from_char_set_int(&mut out.d, v as i32, name)?;
                out.d = out.d.wrapping_add(1);
            }
            Dch::DDD => {
                parse_int_len(Some(&mut out.ddd), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::IDDD => {
                parse_int_len(Some(&mut out.ddd), input, &mut s, 3, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::DD => {
                parse_int_len(Some(&mut out.dd), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::D => {
                parse_int_len(Some(&mut out.d), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::ID => {
                parse_int_len(Some(&mut out.d), input, &mut s, 1, name, slurp)?;
                // Shift numbering to match Gregorian where Sunday = 1.
                out.d = out.d.wrapping_add(1);
                if out.d > 7 {
                    out.d = 1;
                }
                skip_thth(input, &mut s, sfx);
            }
            Dch::WW | Dch::IW => {
                parse_int_len(Some(&mut out.ww), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::Q => {
                // Parsed but ignored.
                parse_int_len(None, input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::CC => {
                parse_int_len(Some(&mut out.cc), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::Y_YYY => {
                let rest = &input[s.min(input.len())..];
                let Some((millennia, years, nch)) = scan_y_yyy(rest) else {
                    return Err(fmt_err(format!(
                        "invalid value \"{}\" for \"Y,YYY\"",
                        lossy(rest)
                    )));
                };
                let total = millennia
                    .checked_mul(1000)
                    .and_then(|m| m.checked_add(years));
                let overflow = || {
                    PgError::new(
                        SqlState::DATETIME_FIELD_OVERFLOW,
                        "value for \"Y,YYY\" in source string is out of range",
                    )
                };
                let total = total.ok_or_else(overflow)?;
                let total = i32::try_from(total).map_err(|_| overflow())?;
                from_char_set_int(&mut out.year, total, name)?;
                out.yysz = 4;
                s += nch;
                skip_thth(input, &mut s, sfx);
            }
            Dch::YYYY | Dch::IYYY => {
                parse_int_len(Some(&mut out.year), input, &mut s, klen, name, slurp)?;
                out.yysz = 4;
                skip_thth(input, &mut s, sfx);
            }
            Dch::YYY | Dch::IYY | Dch::YY | Dch::IY | Dch::Y | Dch::I => {
                let len = parse_int_len(Some(&mut out.year), input, &mut s, klen, name, slurp)?;
                if len < 4 {
                    out.year = adjust_partial_year_to_2020(out.year);
                }
                out.yysz = match kwd.id {
                    Dch::YYY | Dch::IYY => 3,
                    Dch::YY | Dch::IY => 2,
                    _ => 1,
                };
                skip_thth(input, &mut s, sfx);
            }
            Dch::RM | Dch::rm => {
                let v = from_char_seq_search(input, &mut s, &RM_MONTHS_LOWER, name)?;
                from_char_set_int(&mut out.mm, 12 - v as i32, name)?;
            }
            Dch::W => {
                parse_int_len(Some(&mut out.w), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
            Dch::J => {
                parse_int_len(Some(&mut out.j), input, &mut s, klen, name, slurp)?;
                skip_thth(input, &mut s, sfx);
            }
        }
        // Ignore all spaces after fields.
        if !fx_mode {
            extra_skip = 0;
            while bytes_at(input, s) != 0 && c_isspace(input[s]) {
                s += 1;
                extra_skip += 1;
            }
        }
        ni += 1;
    }
    Ok(out)
}

/// Result of `do_to_timestamp`.
#[derive(Clone, Debug)]
struct ParsedDateTime {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    fsec: i64,
    has_tz: bool,
    /// Seconds WEST of UTC when `has_tz`.
    tz_west: i32,
    fprec: i32,
}

const FM_YEAR: u32 = 1;
const FM_MONTH: u32 = 2;
const FM_DAY: u32 = 4;

fn field_overflow(date_str: &str) -> PgError {
    dt_parse_error(&DtErr::FieldOverflow, date_str, "timestamp")
}

fn isoweekdate2date(isoweek: i32, wday: i32, year: i32) -> Result<(i32, i32, i32)> {
    let mut jday = calendar::isoweek2j(year, isoweek)?;
    // Gregorian week start (Sunday=1) to ISO week start (Monday=1).
    if wday > 1 {
        jday = jday.wrapping_add(wday.wrapping_sub(2));
    } else {
        jday = jday.wrapping_add(6);
    }
    Ok(calendar::j2date(jday))
}

/// Port of `do_to_timestamp` (non-standard mode).
fn do_to_timestamp(date_str: &str, fmt: &str) -> Result<ParsedDateTime> {
    let mut tm_year = 0i32;
    let mut tm_mon = 1i32;
    let mut tm_mday = 1i32;
    let mut tm_hour = 0i32;
    let mut tm_min = 0i32;
    let mut tm_sec = 0i32;
    let mut fsec: i64 = 0;
    let mut fmask: u32 = 0;
    let mut tmfc = FromChar::default();

    if !fmt.is_empty() {
        let nodes = parse_dch_format(fmt)?;
        tmfc = dch_from_char(&nodes, date_str.as_bytes())?;
    }

    if tmfc.ssss != 0 {
        let mut x = tmfc.ssss;
        tm_hour = x / 3600;
        x %= 3600;
        tm_min = x / 60;
        x %= 60;
        tm_sec = x;
    }
    if tmfc.ss != 0 {
        tm_sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm_min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm_hour = tmfc.hh;
    }

    if tmfc.clock12 {
        if tm_hour < 1 || tm_hour > 12 {
            return Err(fmt_err(format!(
                "hour \"{tm_hour}\" is invalid for the 12-hour clock"
            ))
            .with_hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        if tmfc.pm != 0 && tm_hour < 12 {
            tm_hour += 12;
        } else if tmfc.pm == 0 && tm_hour == 12 {
            tm_hour = 0;
        }
    }

    if tmfc.year != 0 {
        // If CC and YY (or Y) are provided, use YY as 2 low-order digits for the year in the century.
        if tmfc.cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                tmfc.cc = tmfc.cc.wrapping_neg();
            }
            tm_year = tmfc.year % 100;
            if tm_year != 0 {
                if tmfc.cc >= 0 {
                    let tmp = (tmfc.cc - 1)
                        .checked_mul(100)
                        .and_then(|t| tm_year.checked_add(t));
                    tm_year = tmp.ok_or_else(|| field_overflow(date_str))?;
                } else {
                    let tmp = (tmfc.cc + 1)
                        .checked_mul(100)
                        .and_then(|t| t.checked_sub(tm_year))
                        .and_then(|t| t.checked_add(1));
                    tm_year = tmp.ok_or_else(|| field_overflow(date_str))?;
                }
            } else {
                // Century year for dates ending in "00".
                tm_year = tmfc
                    .cc
                    .wrapping_mul(100)
                    .wrapping_add(if tmfc.cc >= 0 { 0 } else { 1 });
            }
        } else {
            // A 4-digit year wins over CC.
            tm_year = tmfc.year;
            if tmfc.bc != 0 {
                tm_year = tm_year.wrapping_neg();
            }
            if tm_year < 0 {
                tm_year += 1;
            }
        }
        fmask |= FM_YEAR;
    } else if tmfc.cc != 0 {
        // Use the first year of the century.
        if tmfc.bc != 0 {
            tmfc.cc = tmfc.cc.wrapping_neg();
        }
        if tmfc.cc >= 0 {
            let t = (tmfc.cc - 1)
                .checked_mul(100)
                .and_then(|t| t.checked_add(1));
            tm_year = t.ok_or_else(|| field_overflow(date_str))?;
        } else {
            let t = tmfc.cc.checked_mul(100).and_then(|t| t.checked_add(1));
            tm_year = t.ok_or_else(|| field_overflow(date_str))?;
        }
        fmask |= FM_YEAR;
    }

    if tmfc.j != 0 {
        let (y, m, d) = calendar::j2date(tmfc.j);
        tm_year = y;
        tm_mon = m;
        tm_mday = d;
        fmask |= FM_YEAR | FM_MONTH | FM_DAY;
    }

    if tmfc.ww != 0 {
        if tmfc.mode == Some(DateMode::IsoWeek) {
            // Without a day of week the date is left at the Monday of the ISO week.
            let (y, m, d) = if tmfc.d != 0 {
                isoweekdate2date(tmfc.ww, tmfc.d, tm_year)?
            } else {
                calendar::j2date(calendar::isoweek2j(tm_year, tmfc.ww)?)
            };
            tm_year = y;
            tm_mon = m;
            tm_mday = d;
            fmask |= FM_YEAR | FM_MONTH | FM_DAY;
        } else {
            let t = tmfc
                .ww
                .checked_sub(1)
                .and_then(|t| t.checked_mul(7))
                .and_then(|t| t.checked_add(1));
            tmfc.ddd = t.ok_or_else(|| field_overflow(date_str))?;
        }
    }

    if tmfc.w != 0 {
        let t = tmfc
            .w
            .checked_sub(1)
            .and_then(|t| t.checked_mul(7))
            .and_then(|t| t.checked_add(1));
        tmfc.dd = t.ok_or_else(|| field_overflow(date_str))?;
    }
    if tmfc.dd != 0 {
        tm_mday = tmfc.dd;
        fmask |= FM_DAY;
    }
    if tmfc.mm != 0 {
        tm_mon = tmfc.mm;
        fmask |= FM_MONTH;
    }

    if tmfc.ddd != 0 && (tm_mon <= 1 || tm_mday <= 1) {
        // The month and day have not been set: use the day-of-year field to populate them.
        if tm_year == 0 && tmfc.bc == 0 {
            return Err(fmt_err(
                "cannot calculate day of year without year information",
            ));
        }
        if tmfc.mode == Some(DateMode::IsoWeek) {
            let j0 = calendar::isoweek2j(tm_year, 1)? - 1;
            let (y, m, d) = calendar::j2date(j0.wrapping_add(tmfc.ddd));
            tm_year = y;
            tm_mon = m;
            tm_mday = d;
            fmask |= FM_YEAR | FM_MONTH | FM_DAY;
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[usize::from(calendar::is_leap(tm_year))];
            let mut i = 1usize;
            while i <= 12 {
                if tmfc.ddd <= y[i] {
                    break;
                }
                i += 1;
            }
            if tm_mon <= 1 {
                tm_mon = i as i32;
            }
            if tm_mday <= 1 {
                tm_mday = tmfc.ddd - y[i - 1];
            }
            fmask |= FM_MONTH | FM_DAY;
        }
    }

    if tmfc.ms != 0 {
        let t = tmfc
            .ms
            .checked_mul(1000)
            .and_then(|t| i32::try_from(i64::from(t) + fsec).ok());
        fsec = i64::from(t.ok_or_else(|| field_overflow(date_str))?);
    }
    if tmfc.us != 0 {
        fsec += i64::from(tmfc.us);
    }
    let fprec = tmfc.ff;

    // Range-check date fields according to the bit mask computed above (ValidateDate, isjulian).
    if fmask != 0 {
        let bad = (fmask & FM_MONTH != 0 && !(1..=12).contains(&tm_mon))
            || (fmask & FM_DAY != 0 && !(1..=31).contains(&tm_mday))
            || (fmask & (FM_YEAR | FM_MONTH | FM_DAY) == (FM_YEAR | FM_MONTH | FM_DAY)
                && tm_mday > calendar::days_in_month(tm_year, tm_mon));
        if bad {
            return Err(field_overflow(date_str));
        }
    }

    // Range-check time fields too.
    if !(0..24).contains(&tm_hour)
        || !(0..60).contains(&tm_min)
        || !(0..60).contains(&tm_sec)
        || !(0..1_000_000).contains(&fsec)
    {
        return Err(field_overflow(date_str));
    }

    let mut has_tz = false;
    let mut tz_west = 0i32;
    if tmfc.tzsign != 0 {
        // TZH and/or TZM fields.
        if tmfc.tzh < 0 || tmfc.tzh > 15 || tmfc.tzm < 0 || tmfc.tzm >= 60 {
            return Err(dt_parse_error(
                &DtErr::TzDispOverflow,
                date_str,
                "timestamp",
            ));
        }
        has_tz = true;
        tz_west = (tmfc.tzh * 60 + tmfc.tzm) * 60;
        if tmfc.tzsign > 0 {
            tz_west = -tz_west;
        }
    } else if tmfc.has_tz {
        has_tz = true;
        tz_west = -tmfc.gmtoffset;
    }

    Ok(ParsedDateTime {
        year: tm_year,
        mon: tm_mon,
        mday: tm_mday,
        hour: tm_hour,
        min: tm_min,
        sec: tm_sec,
        fsec,
        has_tz,
        tz_west,
        fprec,
    })
}

fn to_timestamp(args: &[Datum], ctx: &FuncCtx) -> Result<Datum> {
    let date_str = arg_text(args, 0)?;
    let fmt = arg_text(args, 1)?;
    let p = do_to_timestamp(date_str, fmt)?;
    let tm = Tm {
        year: p.year,
        mon: p.mon,
        mday: p.mday,
        hour: p.hour,
        min: p.min,
        sec: p.sec,
        fsec: p.fsec as i32,
        ..Tm::default()
    };
    let west = if p.has_tz {
        p.tz_west
    } else {
        // DetermineTimeZoneOffset: interpret the fields in the session zone.
        let local = calendar::tm2timestamp(&tm, None)?;
        -tz::offset_at_local(&ctx.io.time_zone, local)?
    };
    let mut result = calendar::tm2timestamp(&tm, Some(west))?;
    if p.fprec != 0 {
        result = adjust_timestamp_precision(result, p.fprec)?;
    }
    Ok(Datum::TimestampTz(result))
}

fn to_date(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let date_str = arg_text(args, 0)?;
    let fmt = arg_text(args, 1)?;
    let p = do_to_timestamp(date_str, fmt)?;
    let out_of_range_date = || {
        PgError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("date out of range: \"{date_str}\""),
        )
    };
    // IS_VALID_JULIAN: prevent overflow in the Julian-day routines.
    let (y, m) = (p.year, p.mon);
    let valid_julian =
        (y > -4713 || (y == -4713 && m >= 11)) && (y < 5_874_898 || (y == 5_874_898 && m < 6));
    if !valid_julian {
        return Err(out_of_range_date());
    }
    let result = calendar::date2j(p.year, p.mon, p.mday) - POSTGRES_EPOCH_JDATE;
    // IS_VALID_DATE
    if !((DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE) <= result
        && result < (DATE_END_JULIAN - POSTGRES_EPOCH_JDATE))
    {
        return Err(out_of_range_date());
    }
    Ok(Datum::Date(result))
}

// ---------------------------------------------------------------------------------------------
// Number formatting (`NUMDesc`, `NUM_processor`)
// ---------------------------------------------------------------------------------------------

const NUM_F_DECIMAL: u32 = 1 << 1;
const NUM_F_LDECIMAL: u32 = 1 << 2;
const NUM_F_ZERO: u32 = 1 << 3;
const NUM_F_BLANK: u32 = 1 << 4;
const NUM_F_FILLMODE: u32 = 1 << 5;
const NUM_F_LSIGN: u32 = 1 << 6;
const NUM_F_BRACKET: u32 = 1 << 7;
const NUM_F_MINUS: u32 = 1 << 8;
const NUM_F_PLUS: u32 = 1 << 9;
const NUM_F_ROMAN: u32 = 1 << 10;
const NUM_F_MULTI: u32 = 1 << 11;
const NUM_F_PLUS_POST: u32 = 1 << 12;
const NUM_F_MINUS_POST: u32 = 1 << 13;
const NUM_F_EEEE: u32 = 1 << 14;

const NUM_LSIGN_PRE: i32 = -1;
const NUM_LSIGN_POST: i32 = 1;
const NUM_LSIGN_NONE: i32 = 0;

#[derive(Clone, Copy, Debug, Default)]
struct NumDesc {
    pre: i32,
    post: i32,
    lsign: i32,
    flag: u32,
    pre_lsign_num: i32,
    multi: i32,
    zero_start: i32,
    zero_end: i32,
}

impl NumDesc {
    fn has(&self, f: u32) -> bool {
        self.flag & f != 0
    }
    fn is_decimal(&self) -> bool {
        self.has(NUM_F_DECIMAL)
    }
    fn is_zero(&self) -> bool {
        self.has(NUM_F_ZERO)
    }
    fn is_fillmode(&self) -> bool {
        self.has(NUM_F_FILLMODE)
    }
    fn is_bracket(&self) -> bool {
        self.has(NUM_F_BRACKET)
    }
    fn is_lsign(&self) -> bool {
        self.has(NUM_F_LSIGN)
    }
    fn is_plus(&self) -> bool {
        self.has(NUM_F_PLUS)
    }
    fn is_minus(&self) -> bool {
        self.has(NUM_F_MINUS)
    }
    fn is_roman(&self) -> bool {
        self.has(NUM_F_ROMAN)
    }
    fn is_multi(&self) -> bool {
        self.has(NUM_F_MULTI)
    }
    fn is_eeee(&self) -> bool {
        self.has(NUM_F_EEEE)
    }
}

/// `NUMDesc_prepare`: account for one keyword of a number picture.
fn numdesc_prepare(num: &mut NumDesc, id: NumId) -> Result<()> {
    if num.is_eeee() && id != NumId::E {
        return Err(syntax_err("\"EEEE\" must be the last pattern used"));
    }
    match id {
        NumId::N9 => {
            if num.is_bracket() {
                return Err(syntax_err("\"9\" must be ahead of \"PR\""));
            }
            if num.is_multi() {
                num.multi += 1;
            } else if num.is_decimal() {
                num.post += 1;
            } else {
                num.pre += 1;
            }
        }
        NumId::N0 => {
            if num.is_bracket() {
                return Err(syntax_err("\"0\" must be ahead of \"PR\""));
            }
            if !num.is_zero() && !num.is_decimal() {
                num.flag |= NUM_F_ZERO;
                num.zero_start = num.pre + 1;
            }
            if !num.is_decimal() {
                num.pre += 1;
            } else {
                num.post += 1;
            }
            num.zero_end = num.pre + num.post;
        }
        NumId::B => {
            if num.pre == 0 && num.post == 0 && !num.is_zero() {
                num.flag |= NUM_F_BLANK;
            }
        }
        NumId::D | NumId::Dec => {
            if id == NumId::D {
                num.flag |= NUM_F_LDECIMAL;
            }
            if num.is_decimal() {
                return Err(syntax_err("multiple decimal points"));
            }
            if num.is_multi() {
                return Err(syntax_err("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_DECIMAL;
        }
        NumId::FM => num.flag |= NUM_F_FILLMODE,
        NumId::S => {
            if num.is_lsign() {
                return Err(syntax_err("cannot use \"S\" twice"));
            }
            if num.is_plus() || num.is_minus() || num.is_bracket() {
                return Err(syntax_err(
                    "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                ));
            }
            if !num.is_decimal() {
                num.lsign = NUM_LSIGN_PRE;
                num.pre_lsign_num = num.pre;
                num.flag |= NUM_F_LSIGN;
            } else if num.lsign == NUM_LSIGN_NONE {
                num.lsign = NUM_LSIGN_POST;
                num.flag |= NUM_F_LSIGN;
            }
        }
        NumId::MI => {
            if num.is_lsign() {
                return Err(syntax_err("cannot use \"S\" and \"MI\" together"));
            }
            num.flag |= NUM_F_MINUS;
            if num.is_decimal() {
                num.flag |= NUM_F_MINUS_POST;
            }
        }
        NumId::PL => {
            if num.is_lsign() {
                return Err(syntax_err("cannot use \"S\" and \"PL\" together"));
            }
            num.flag |= NUM_F_PLUS;
            if num.is_decimal() {
                num.flag |= NUM_F_PLUS_POST;
            }
        }
        NumId::SG => {
            if num.is_lsign() {
                return Err(syntax_err("cannot use \"S\" and \"SG\" together"));
            }
            num.flag |= NUM_F_MINUS;
            num.flag |= NUM_F_PLUS;
        }
        NumId::PR => {
            if num.is_lsign() || num.is_plus() || num.is_minus() {
                return Err(syntax_err(
                    "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                ));
            }
            num.flag |= NUM_F_BRACKET;
        }
        NumId::rn | NumId::RN => {
            if num.is_roman() {
                return Err(syntax_err("cannot use \"RN\" twice"));
            }
            num.flag |= NUM_F_ROMAN;
        }
        NumId::L | NumId::G => {}
        NumId::V => {
            if num.is_decimal() {
                return Err(syntax_err("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_MULTI;
        }
        NumId::E => {
            if num.is_eeee() {
                return Err(syntax_err("cannot use \"EEEE\" twice"));
            }
            if num.has(NUM_F_BLANK)
                || num.is_fillmode()
                || num.is_lsign()
                || num.is_bracket()
                || num.is_minus()
                || num.is_plus()
                || num.is_roman()
                || num.is_multi()
            {
                return Err(
                    syntax_err("\"EEEE\" is incompatible with other formats").with_detail(
                        "\"EEEE\" may only be used together with digit and decimal point patterns.",
                    ),
                );
            }
            num.flag |= NUM_F_EEEE;
        }
        NumId::Comma | NumId::C | NumId::SP | NumId::TH | NumId::th => {}
    }
    if num.is_roman() && (num.flag & !(NUM_F_ROMAN | NUM_F_FILLMODE)) != 0 {
        return Err(syntax_err("\"RN\" is incompatible with other formats")
            .with_detail("\"RN\" may only be used together with \"FM\"."));
    }
    Ok(())
}

fn parse_num_format(fmt: &str) -> Result<(Vec<Node>, NumDesc)> {
    let mut num = NumDesc::default();
    let nodes = parse_format(fmt, false, &mut |k| {
        numdesc_prepare(&mut num, NUM_KEYWORDS[k].id)
    })?;
    Ok((nodes, num))
}

/// `int_to_roman`: upper-case, not blank-padded; out-of-range gives fifteen `#`.
fn int_to_roman(number: i32) -> String {
    if !(1..=3999).contains(&number) {
        return "#".repeat(MAX_ROMAN_LEN);
    }
    let digits = number.to_string();
    let mut len = digits.len();
    let mut result = String::new();
    for ch in digits.bytes() {
        let d = i32::from(ch) - i32::from(b'0' + 1);
        if d >= 0 {
            let d = d as usize;
            match len {
                4 => {
                    for _ in 0..=d {
                        result.push('M');
                    }
                }
                3 => result.push_str(RM100[d]),
                2 => result.push_str(RM10[d]),
                1 => result.push_str(RM1[d]),
                _ => {}
            }
        }
        len -= 1;
    }
    result
}

fn roman_val(c: u8) -> i32 {
    match c {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    }
}

fn is_valid_sub_comb(curr: u8, next: u8) -> bool {
    (curr == b'I' && (next == b'V' || next == b'X'))
        || (curr == b'X' && (next == b'L' || next == b'C'))
        || (curr == b'C' && (next == b'D' || next == b'M'))
}

/// State of `NUM_processor` (`NUMProc`) for both directions.
struct NumProc<'a> {
    is_to_char: bool,
    num: NumDesc,
    /// `'+'`, `'-'` or 0.
    sign: u8,
    sign_wrote: bool,
    num_count: i32,
    num_in: bool,
    num_curr: i32,
    out_pre_spaces: i32,
    read_dec: bool,
    read_post: i32,
    read_pre: i32,
    number: Vec<u8>,
    number_p: usize,
    inout: &'a [u8],
    inout_p: usize,
    out: Vec<u8>,
    last_relevant: Option<usize>,
}

impl<'a> NumProc<'a> {
    fn nb(&self, i: usize) -> u8 {
        self.number.get(i).copied().unwrap_or(0)
    }
    fn ib(&self, i: usize) -> u8 {
        self.inout.get(i).copied().unwrap_or(0)
    }
    /// `OVERLOAD_TEST`: reached the end of the input.
    fn overload(&self) -> bool {
        self.inout_p >= self.inout.len()
    }
    /// `AMOUNT_TEST(s)`: at least `s` bytes remain.
    fn amount_test(&self, s: usize) -> bool {
        self.inout_p + s <= self.inout.len()
    }
    fn is_predec_space(&self) -> bool {
        !self.num.is_zero() && self.number_p == 0 && self.nb(0) == b'0' && self.num.post != 0
    }
    fn last_relevant_is_dot(&self) -> bool {
        self.last_relevant.is_some_and(|i| self.nb(i) == b'.')
    }
    fn add_symbol(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }
    fn sign_symbol(&self) -> &'static str {
        if self.sign == b'-' {
            L_NEGATIVE_SIGN
        } else {
            L_POSITIVE_SIGN
        }
    }
    fn set_number(&mut self, c: u8) {
        self.number.truncate(self.number_p);
        self.number.push(c);
        self.number_p += 1;
    }

    /// `NUM_numpart_to_char`.
    fn numpart_to_char(&mut self, id: NumId) {
        if self.num.is_roman() {
            return;
        }
        self.num_in = false;

        // Write the sign if a real number will be written to the output.
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (self.num.is_zero() && self.num.zero_start == self.num_curr))
            && (!self.is_predec_space() || self.last_relevant_is_dot())
        {
            if self.num.is_lsign() {
                if self.num.lsign == NUM_LSIGN_PRE {
                    let sym = self.sign_symbol();
                    self.add_symbol(sym);
                    self.sign_wrote = true;
                }
            } else if self.num.is_bracket() {
                self.out.push(if self.sign == b'+' { b' ' } else { b'<' });
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !self.num.is_fillmode() {
                    self.out.push(b' ');
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.out.push(b'-');
                self.sign_wrote = true;
            }
        }

        // Digits / FM / zero / decimal point.
        if matches!(id, NumId::N9 | NumId::N0 | NumId::D | NumId::Dec) {
            if self.num_curr < self.out_pre_spaces
                && (self.num.zero_start > self.num_curr || !self.num.is_zero())
            {
                // Write blank space.
                if !self.num.is_fillmode() {
                    self.out.push(b' ');
                }
            } else if self.num.is_zero()
                && self.num_curr < self.out_pre_spaces
                && self.num.zero_start <= self.num_curr
            {
                // Write zero.
                self.out.push(b'0');
                self.num_in = true;
            } else {
                if self.nb(self.number_p) == b'.' {
                    // Write the decimal point.
                    if self.last_relevant.is_none() || !self.last_relevant_is_dot() {
                        self.add_symbol(L_DECIMAL);
                    } else if self.num.is_fillmode() && self.last_relevant_is_dot() {
                        // Oracle 'n' -- FM9.9 --> 'n.'
                        self.add_symbol(L_DECIMAL);
                    }
                } else if self.last_relevant.is_some_and(|lr| self.number_p > lr) && id != NumId::N0
                {
                    // Beyond the last relevant digit: write nothing.
                } else if self.is_predec_space() {
                    // '0.1' -- 9.9 --> '  .1'
                    if !self.num.is_fillmode() {
                        self.out.push(b' ');
                    } else if self.last_relevant_is_dot() {
                        // '0' -- FM9.9 --> '0.'
                        self.out.push(b'0');
                    }
                } else {
                    // Write digit.
                    let c = self.nb(self.number_p);
                    self.out.push(c);
                    self.num_in = true;
                }
                // Do not exceed the string length.
                if self.nb(self.number_p) != 0 {
                    self.number_p += 1;
                }
            }

            let mut end = self.num_count
                + i32::from(self.out_pre_spaces != 0)
                + i32::from(self.num.is_decimal());
            if self.last_relevant.is_some_and(|lr| lr == self.number_p) {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.num.is_bracket() {
                    self.out.push(if self.sign == b'+' { b' ' } else { b'>' });
                } else if self.num.is_lsign() && self.num.lsign == NUM_LSIGN_POST {
                    let sym = self.sign_symbol();
                    self.add_symbol(sym);
                }
            }
        }
        self.num_curr += 1;
    }

    /// `NUM_eat_non_data_chars`.
    fn eat_non_data_chars(&mut self, n: usize) {
        for _ in 0..n {
            if self.overload() {
                break;
            }
            let c = self.ib(self.inout_p);
            if b"0123456789.,+-".contains(&c) {
                break;
            }
            self.inout_p += mblen(self.inout, self.inout_p).max(1);
        }
    }

    /// `NUM_numpart_from_char`.
    fn numpart_from_char(&mut self, id: NumId) {
        let mut isread = false;
        if self.overload() {
            return;
        }
        if self.ib(self.inout_p) == b' ' {
            self.inout_p += 1;
        }
        if self.overload() {
            return;
        }

        // Read the sign before the number.
        if self.nb(0) == b' '
            && matches!(id, NumId::N0 | NumId::N9)
            && self.read_pre + self.read_post == 0
        {
            if self.num.is_lsign() && self.num.lsign == NUM_LSIGN_PRE {
                if self.amount_test(L_NEGATIVE_SIGN.len())
                    && self.inout[self.inout_p..].starts_with(L_NEGATIVE_SIGN.as_bytes())
                {
                    self.inout_p += L_NEGATIVE_SIGN.len();
                    self.number[0] = b'-';
                } else if self.amount_test(L_POSITIVE_SIGN.len())
                    && self.inout[self.inout_p..].starts_with(L_POSITIVE_SIGN.as_bytes())
                {
                    self.inout_p += L_POSITIVE_SIGN.len();
                    self.number[0] = b'+';
                }
            } else {
                let c = self.ib(self.inout_p);
                if c == b'-' || (self.num.is_bracket() && c == b'<') {
                    self.number[0] = b'-';
                    self.inout_p += 1;
                } else if c == b'+' {
                    self.number[0] = b'+';
                    self.inout_p += 1;
                }
            }
        }
        if self.overload() {
            return;
        }

        // Read digit or decimal point.
        if self.ib(self.inout_p).is_ascii_digit() {
            if self.read_dec && self.read_post == self.num.post {
                return;
            }
            let c = self.ib(self.inout_p);
            self.set_number(c);
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.num.is_decimal() && !self.read_dec {
            let x = L_DECIMAL.len();
            if self.amount_test(x) && self.inout[self.inout_p..].starts_with(L_DECIMAL.as_bytes()) {
                self.inout_p += x - 1;
                self.set_number(b'.');
                self.read_dec = true;
                isread = true;
            }
        }
        if self.overload() {
            return;
        }

        // Read the sign behind the "last" number.
        if self.nb(0) == b' ' && self.read_pre + self.read_post > 0 {
            if self.num.is_lsign()
                && isread
                && self.inout_p + 1 < self.inout.len()
                && !self.ib(self.inout_p + 1).is_ascii_digit()
            {
                let tmp = self.inout_p;
                self.inout_p += 1;
                if self.amount_test(L_NEGATIVE_SIGN.len())
                    && self.inout[self.inout_p..].starts_with(L_NEGATIVE_SIGN.as_bytes())
                {
                    self.inout_p += L_NEGATIVE_SIGN.len() - 1;
                    self.number[0] = b'-';
                } else if self.amount_test(L_POSITIVE_SIGN.len())
                    && self.inout[self.inout_p..].starts_with(L_POSITIVE_SIGN.as_bytes())
                {
                    self.inout_p += L_POSITIVE_SIGN.len() - 1;
                    self.number[0] = b'+';
                }
                if self.nb(0) == b' ' {
                    // No sign read.
                    self.inout_p = tmp;
                }
            } else if !isread && !self.num.is_lsign() && (self.num.is_plus() || self.num.is_minus())
            {
                let c = self.ib(self.inout_p);
                if c == b'-' || c == b'+' {
                    self.number[0] = c;
                }
            }
        }
    }

    /// `roman_to_int`; -1 for an invalid numeral. Advances `inout_p` past the characters consumed.
    fn roman_to_int(&mut self) -> i32 {
        let mut result = 0i32;
        let mut repeat_count = 1;
        let (mut v_count, mut l_count, mut d_count) = (0, 0, 0);
        let mut subtraction_encountered = false;
        let mut last_subtracted_value = 0;

        while !self.overload() && c_isspace(self.ib(self.inout_p)) {
            self.inout_p += 1;
        }
        let mut chars = [0u8; MAX_ROMAN_LEN];
        let mut values = [0i32; MAX_ROMAN_LEN];
        let mut len = 0usize;
        while len < MAX_ROMAN_LEN && !self.overload() {
            let c = self.ib(self.inout_p).to_ascii_uppercase();
            let v = roman_val(c);
            if v == 0 {
                break;
            }
            chars[len] = c;
            values[len] = v;
            self.inout_p += 1;
            len += 1;
        }
        if len == 0 {
            return -1;
        }

        let mut i = 0usize;
        while i < len {
            let curr_char = chars[i];
            let curr_value = values[i];
            if subtraction_encountered && curr_value >= last_subtracted_value {
                return -1;
            }
            if (v_count > 0 && curr_value >= roman_val(b'V'))
                || (l_count > 0 && curr_value >= roman_val(b'L'))
                || (d_count > 0 && curr_value >= roman_val(b'D'))
            {
                return -1;
            }
            match curr_char {
                b'V' => v_count += 1,
                b'L' => l_count += 1,
                b'D' => d_count += 1,
                _ => {}
            }
            if i < len - 1 {
                let next_char = chars[i + 1];
                let next_value = values[i + 1];
                if curr_value < next_value {
                    if !is_valid_sub_comb(curr_char, next_char) {
                        return -1;
                    }
                    if repeat_count > 1 {
                        return -1;
                    }
                    if (v_count > 0 && next_value >= roman_val(b'V'))
                        || (l_count > 0 && next_value >= roman_val(b'L'))
                        || (d_count > 0 && next_value >= roman_val(b'D'))
                    {
                        return -1;
                    }
                    match next_char {
                        b'V' => v_count += 1,
                        b'L' => l_count += 1,
                        b'D' => d_count += 1,
                        _ => {}
                    }
                    i += 1;
                    repeat_count = 1;
                    subtraction_encountered = true;
                    last_subtracted_value = curr_value;
                    result += next_value - curr_value;
                } else {
                    if curr_char == next_char {
                        repeat_count += 1;
                        if repeat_count > 3 {
                            return -1;
                        }
                    } else {
                        repeat_count = 1;
                    }
                    result += curr_value;
                }
            } else {
                result += curr_value;
            }
            i += 1;
        }
        result
    }

    /// `last_relevant` for fill mode (`get_last_relevant_decnum`): index of the last relevant digit
    /// after the decimal point, or of the point itself.
    fn get_last_relevant_decnum(&self) -> Option<usize> {
        let p = self.number.iter().position(|c| *c == b'.')?;
        let mut result = p;
        for (i, c) in self.number.iter().enumerate().skip(p + 1) {
            if *c == 0 {
                break;
            }
            if *c != b'0' {
                result = i;
            }
        }
        Some(result)
    }

    /// The main node loop of `NUM_processor`. Returns whether the node list ran to completion.
    fn run(&mut self, nodes: &[Node]) -> Result<()> {
        for n in nodes {
            if !self.is_to_char && self.overload() {
                break;
            }
            match n {
                Node::Action { key, .. } => {
                    let id = NUM_KEYWORDS[*key].id;
                    // `advance` false mirrors a C `continue` (skip the trailing `inout_p++`).
                    let advance: bool = match id {
                        NumId::N9 | NumId::N0 | NumId::Dec | NumId::D => {
                            if self.is_to_char {
                                self.numpart_to_char(id);
                                false
                            } else {
                                self.numpart_from_char(id);
                                true
                            }
                        }
                        NumId::Comma => {
                            if self.is_to_char {
                                if !self.num_in {
                                    if self.num.is_fillmode() {
                                        false
                                    } else {
                                        self.out.push(b' ');
                                        true
                                    }
                                } else {
                                    self.out.push(b',');
                                    true
                                }
                            } else if !self.num_in && self.num.is_fillmode() {
                                false
                            } else {
                                self.ib(self.inout_p) == b','
                            }
                        }
                        NumId::G => {
                            if self.is_to_char {
                                if !self.num_in {
                                    if self.num.is_fillmode() {
                                        false
                                    } else {
                                        for _ in L_THOUSANDS_SEP.chars() {
                                            self.out.push(b' ');
                                        }
                                        true
                                    }
                                } else {
                                    self.add_symbol(L_THOUSANDS_SEP);
                                    true
                                }
                            } else if !self.num_in && self.num.is_fillmode() {
                                false
                            } else if self.amount_test(L_THOUSANDS_SEP.len())
                                && self.inout[self.inout_p..]
                                    .starts_with(L_THOUSANDS_SEP.as_bytes())
                            {
                                self.inout_p += L_THOUSANDS_SEP.len() - 1;
                                true
                            } else {
                                false
                            }
                        }
                        NumId::L => {
                            if self.is_to_char {
                                self.add_symbol(L_CURRENCY_SYMBOL);
                                true
                            } else {
                                self.eat_non_data_chars(L_CURRENCY_SYMBOL.chars().count());
                                false
                            }
                        }
                        NumId::RN | NumId::rn => {
                            if self.is_to_char {
                                let rest: Vec<u8> =
                                    self.number.get(self.number_p..).unwrap_or(&[]).to_vec();
                                let mut text = lossy(&rest);
                                if id == NumId::rn {
                                    text = text.to_ascii_lowercase();
                                }
                                if self.num.is_fillmode() {
                                    self.out.extend_from_slice(text.as_bytes());
                                } else {
                                    self.out.extend_from_slice(format!("{text:>15}").as_bytes());
                                }
                                true
                            } else {
                                let roman = self.roman_to_int();
                                if roman < 0 {
                                    return Err(PgError::new(
                                        SqlState::INVALID_TEXT_REPRESENTATION,
                                        "invalid Roman numeral",
                                    ));
                                }
                                let s = roman.to_string();
                                for c in s.bytes() {
                                    self.set_number(c);
                                }
                                self.num.pre = s.len() as i32;
                                self.num.post = 0;
                                false
                            }
                        }
                        NumId::th | NumId::TH => {
                            if self.num.is_roman()
                                || self.nb(0) == b'#'
                                || self.sign == b'-'
                                || self.num.is_decimal()
                            {
                                false
                            } else if self.is_to_char {
                                let text = lossy(&self.number);
                                let t = get_th(&text, id == NumId::TH)?;
                                self.add_symbol(t);
                                true
                            } else {
                                // All variants of 'th' occupy 2 characters.
                                self.eat_non_data_chars(2);
                                false
                            }
                        }
                        NumId::MI => {
                            if self.is_to_char {
                                if self.sign == b'-' {
                                    self.out.push(b'-');
                                    true
                                } else if self.num.is_fillmode() {
                                    false
                                } else {
                                    self.out.push(b' ');
                                    true
                                }
                            } else if self.ib(self.inout_p) == b'-' {
                                self.number[0] = b'-';
                                true
                            } else {
                                self.eat_non_data_chars(1);
                                false
                            }
                        }
                        NumId::PL => {
                            if self.is_to_char {
                                if self.sign == b'+' {
                                    self.out.push(b'+');
                                    true
                                } else if self.num.is_fillmode() {
                                    false
                                } else {
                                    self.out.push(b' ');
                                    true
                                }
                            } else if self.ib(self.inout_p) == b'+' {
                                self.number[0] = b'+';
                                true
                            } else {
                                self.eat_non_data_chars(1);
                                false
                            }
                        }
                        NumId::SG => {
                            if self.is_to_char {
                                self.out.push(self.sign);
                                true
                            } else {
                                let c = self.ib(self.inout_p);
                                if c == b'-' {
                                    self.number[0] = b'-';
                                    true
                                } else if c == b'+' {
                                    self.number[0] = b'+';
                                    true
                                } else {
                                    self.eat_non_data_chars(1);
                                    false
                                }
                            }
                        }
                        NumId::B
                        | NumId::C
                        | NumId::E
                        | NumId::FM
                        | NumId::PR
                        | NumId::S
                        | NumId::SP
                        | NumId::V => false,
                    };
                    if !advance {
                        continue;
                    }
                }
                Node::Lit { text, .. } => {
                    // to_char copies the literal; to_number skips one input character per literal.
                    if self.is_to_char {
                        self.out.extend_from_slice(text.as_bytes());
                    } else {
                        self.inout_p += mblen(self.inout, self.inout_p).max(1);
                    }
                    continue;
                }
            }
            if !self.is_to_char {
                self.inout_p += 1;
            }
        }
        Ok(())
    }
}

/// `NUM_processor` for to_char. `number` is the digit string (without sign) or roman numeral.
fn num_processor_to_char(
    nodes: &[Node],
    num: &mut NumDesc,
    number: &str,
    out_pre_spaces: i32,
    sign: u8,
) -> Result<String> {
    if num.zero_start != 0 {
        num.zero_start -= 1;
    }
    if num.is_eeee() {
        return Ok(number.to_string());
    }
    let mut np = NumProc {
        is_to_char: true,
        num: *num,
        sign,
        sign_wrote: false,
        num_count: 0,
        num_in: false,
        num_curr: 0,
        out_pre_spaces,
        read_dec: false,
        read_post: 0,
        read_pre: 0,
        number: number.as_bytes().to_vec(),
        number_p: 0,
        inout: &[],
        inout_p: 0,
        out: Vec::new(),
        last_relevant: None,
    };

    // Sign handling.
    if np.num.is_plus() || np.num.is_minus() {
        np.sign_wrote = !(np.num.is_plus() && !np.num.is_minus());
    } else {
        if np.sign != b'-' && np.num.is_fillmode() {
            np.num.flag &= !NUM_F_BRACKET;
        }
        // A '+' in fill mode without a locale sign needs no sign.
        np.sign_wrote = np.sign == b'+' && np.num.is_fillmode() && !np.num.is_lsign();
        if np.num.lsign == NUM_LSIGN_PRE && np.num.pre == np.num.pre_lsign_num {
            np.num.lsign = NUM_LSIGN_POST;
        }
    }

    np.num_count = np.num.post + np.num.pre - 1;

    if np.num.is_fillmode() && np.num.is_decimal() {
        np.last_relevant = np.get_last_relevant_decnum();
        // If any '0' specifiers are present, make sure we don't strip those digits.
        if np.last_relevant.is_some() && np.num.zero_end > np.out_pre_spaces {
            let len = np.number.len() as i32;
            let last_zero_pos = (len - 1).min(np.num.zero_end - np.out_pre_spaces);
            let last_zero = last_zero_pos.max(0) as usize;
            if np.last_relevant.is_some_and(|lr| lr < last_zero) {
                np.last_relevant = Some(last_zero);
            }
        }
    }
    if !np.sign_wrote && np.out_pre_spaces == 0 {
        np.num_count += 1;
    }

    np.run(nodes)?;
    *num = np.num;
    // The C buffer is a NUL-terminated string: cut at the first NUL.
    if let Some(p) = np.out.iter().position(|c| *c == 0) {
        np.out.truncate(p);
    }
    Ok(lossy(&np.out))
}

/// `NUM_processor` for to_number: returns the extracted number string (sign slot first).
fn num_processor_from_char(nodes: &[Node], num: &mut NumDesc, input: &[u8]) -> Result<String> {
    if num.zero_start != 0 {
        num.zero_start -= 1;
    }
    if num.is_eeee() {
        return Err(PgError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "\"EEEE\" not supported for input",
        ));
    }
    let mut np = NumProc {
        is_to_char: false,
        num: *num,
        sign: 0,
        sign_wrote: false,
        num_count: 0,
        num_in: false,
        num_curr: 0,
        out_pre_spaces: 0,
        read_dec: false,
        read_post: 0,
        read_pre: 0,
        number: vec![b' '],
        number_p: 1,
        inout: input,
        inout_p: 0,
        out: Vec::new(),
        last_relevant: None,
    };
    np.num_count = np.num.post + np.num.pre - 1;
    np.run(nodes)?;
    // Terminate the number string.
    let end = if np.number_p >= 1 && np.nb(np.number_p - 1) == b'.' {
        np.number_p - 1
    } else {
        np.number_p
    };
    np.number.truncate(end);
    np.num.post = np.read_post;
    *num = np.num;
    Ok(lossy(&np.number))
}

// ---------------------------------------------------------------------------------------------
// Number to_char / to_number SQL functions
// ---------------------------------------------------------------------------------------------

/// C `%.{prec}f` of a double (glibc spelling of the non-finite values).
fn c_fixed(v: f64, prec: usize) -> String {
    if v.is_nan() {
        "nan".to_string()
    } else if v.is_infinite() {
        if v < 0.0 {
            "-inf".to_string()
        } else {
            "inf".to_string()
        }
    } else {
        format!("{v:.prec$}")
    }
}

/// C `%+.{prec}e` of a finite double: sign always present, exponent at least two digits.
fn c_exp_format(v: f64, prec: usize) -> String {
    let neg = v.is_sign_negative();
    let s = format!("{:.*e}", prec, v.abs());
    let sign = if neg { '-' } else { '+' };
    match s.split_once('e') {
        Some((mant, exp)) => {
            let e: i32 = exp.parse().unwrap_or(0);
            format!(
                "{}{}e{}{:02}",
                sign,
                mant,
                if e < 0 { '-' } else { '+' },
                e.abs()
            )
        }
        None => format!("{sign}{s}"),
    }
}

/// Placeholder string for an unprintable EEEE value (`NaN`/`Infinity`): `#` everywhere except the
/// leading space and the decimal point.
fn eeee_overflow_string(num: &NumDesc) -> String {
    let n = (num.pre + num.post + 6).max(2) as usize;
    let mut v = vec![b'#'; n];
    v[0] = b' ';
    let dot = (num.pre + 1) as usize;
    if dot < n {
        v[dot] = b'.';
    }
    lossy(&v)
}

/// Pad (`out_pre_spaces`) or overflow-fill (`#`) the digit string against the picture.
fn layout_numstr(numstr: String, num: &NumDesc) -> (String, i32) {
    let pre_len = numstr.find('.').unwrap_or(numstr.len()) as i32;
    if pre_len < num.pre {
        (numstr, num.pre - pre_len)
    } else if pre_len > num.pre {
        let n = (num.pre + num.post + 1).max(1) as usize;
        let mut v = vec![b'#'; n];
        let dot = num.pre as usize;
        if dot < n {
            v[dot] = b'.';
        }
        (lossy(&v), 0)
    } else {
        (numstr, 0)
    }
}

/// Split a leading '-' off a printed number: (sign byte, digits).
fn split_sign(org: &str) -> (u8, &str) {
    match org.strip_prefix('-') {
        Some(rest) => (b'-', rest),
        None => (b'+', org),
    }
}

fn numeric_pow10(exp: i32) -> Result<Numeric> {
    let ten = Numeric::from_i64(10);
    numeric::transcendental::power(&ten, &Numeric::from_i64(i64::from(exp)))
}

/// Shared tail of the integer paths: sign split, zero-padded decimals, picture overflow.
fn finish_integer(org: String, num: &NumDesc) -> (String, u8, i32) {
    let (sign, body) = split_sign(&org);
    let mut numstr = body.to_string();
    if num.post > 0 {
        numstr.push('.');
        numstr.push_str(&"0".repeat(num.post as usize));
    }
    let (numstr, pre) = layout_numstr(numstr, num);
    (numstr, sign, pre)
}

fn to_char_result(
    nodes: &[Node],
    num: &mut NumDesc,
    numstr: &str,
    pre: i32,
    sign: u8,
) -> Result<Datum> {
    Ok(Datum::Text(num_processor_to_char(
        nodes, num, numstr, pre, sign,
    )?))
}

fn int4_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = match arg(args, 0)? {
        Datum::Int4(v) => *v,
        other => return Err(type_mismatch("integer", other)),
    };
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() {
        return Ok(Datum::text(""));
    }
    let (nodes, mut num) = parse_num_format(fmt)?;
    let (numstr, sign, pre): (String, u8, i32) = if num.is_roman() {
        (int_to_roman(value), 0, 0)
    } else if num.is_eeee() {
        let mut s = c_exp_format(f64::from(value), num.post as usize);
        if s.starts_with('+') {
            s.replace_range(0..1, " ");
        }
        (s, 0, 0)
    } else {
        let mut v = value;
        if num.is_multi() {
            let p = 10f64.powi(num.multi);
            let factor = if p >= 2_147_483_648.0 {
                i32::MIN
            } else {
                p as i32
            };
            v = v.wrapping_mul(factor);
            num.pre += num.multi;
        }
        finish_integer(v.to_string(), &num)
    };
    to_char_result(&nodes, &mut num, &numstr, pre, sign)
}

fn int8_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = match arg(args, 0)? {
        Datum::Int8(v) => *v,
        other => return Err(type_mismatch("bigint", other)),
    };
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() {
        return Ok(Datum::text(""));
    }
    let (nodes, mut num) = parse_num_format(fmt)?;
    let (numstr, sign, pre): (String, u8, i32) = if num.is_roman() {
        (int_to_roman(i32::try_from(value).unwrap_or(i32::MAX)), 0, 0)
    } else if num.is_eeee() {
        // Go via numeric to avoid losing precision.
        let org = nio::format_numeric_sci(&Numeric::from_i64(value), num.post)?;
        (
            if org.starts_with('-') {
                org
            } else {
                format!(" {org}")
            },
            0,
            0,
        )
    } else {
        let mut v = value;
        if num.is_multi() {
            let p = 10f64.powi(num.multi);
            let range_err =
                || PgError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range");
            if !(p < 9.223_372_036_854_775_807e18) {
                return Err(range_err());
            }
            v = v.checked_mul(p as i64).ok_or_else(range_err)?;
            num.pre += num.multi;
        }
        finish_integer(v.to_string(), &num)
    };
    to_char_result(&nodes, &mut num, &numstr, pre, sign)
}

fn float_to_char_impl(value: f64, is_f4: bool, fmt: &str) -> Result<Datum> {
    if fmt.is_empty() {
        return Ok(Datum::text(""));
    }
    let (nodes, mut num) = parse_num_format(fmt)?;
    let dig: i32 = if is_f4 { 6 } else { 15 };
    let (numstr, sign, pre): (String, u8, i32) = if num.is_roman() {
        let r = value.round_ties_even();
        let iv = if !r.is_nan() && r >= -2_147_483_648.0 && r < 2_147_483_648.0 {
            r as i32
        } else {
            i32::MAX
        };
        (int_to_roman(iv), 0, 0)
    } else if num.is_eeee() {
        if value.is_nan() || value.is_infinite() {
            (eeee_overflow_string(&num), 0, 0)
        } else {
            let mut s = c_exp_format(value, num.post as usize);
            if s.starts_with('+') {
                s.replace_range(0..1, " ");
            }
            (s, 0, 0)
        }
    } else {
        let mut val = value;
        if num.is_multi() {
            let m = 10f64.powi(num.multi);
            val = if is_f4 {
                f64::from((value as f32) * (m as f32))
            } else {
                value * m
            };
            num.pre += num.multi;
        }
        let pre_len = c_fixed(val.abs(), 0).len() as i32;
        // Adjust post digits to fit the maximum float digits.
        if pre_len >= dig {
            num.post = 0;
        } else if pre_len + num.post > dig {
            num.post = dig - pre_len;
        }
        let org = c_fixed(val, num.post as usize);
        let (s, body) = split_sign(&org);
        let (n2, p2) = layout_numstr(body.to_string(), &num);
        (n2, s, p2)
    };
    to_char_result(&nodes, &mut num, &numstr, pre, sign)
}

fn float4_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = match arg(args, 0)? {
        Datum::Float4(v) => f64::from(*v),
        other => return Err(type_mismatch("real", other)),
    };
    float_to_char_impl(value, true, arg_text(args, 1)?)
}

fn float8_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = match arg(args, 0)? {
        Datum::Float8(v) => *v,
        other => return Err(type_mismatch("double precision", other)),
    };
    float_to_char_impl(value, false, arg_text(args, 1)?)
}

fn numeric_to_char(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = match arg(args, 0)? {
        Datum::Numeric(n) => n,
        other => return Err(type_mismatch("numeric", other)),
    };
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() {
        return Ok(Datum::text(""));
    }
    let (nodes, mut num) = parse_num_format(fmt)?;
    let (numstr, sign, pre): (String, u8, i32) = if num.is_roman() {
        // Round and convert to int; on overflow just use INT32_MAX (int_to_roman copes).
        (
            int_to_roman(nfuncs::to_i32(value).unwrap_or(i32::MAX)),
            0,
            0,
        )
    } else if num.is_eeee() {
        let org = nio::format_numeric_sci(value, num.post)?;
        if org == "NaN" || org == "Infinity" || org == "-Infinity" {
            (eeee_overflow_string(&num), 0, 0)
        } else if !org.starts_with('-') {
            (format!(" {org}"), 0, 0)
        } else {
            (org, 0, 0)
        }
    } else {
        let mut val = value.clone();
        if num.is_multi() {
            let x = numeric_pow10(num.multi)?;
            val = arith::mul(value, &x)?;
            num.pre += num.multi;
        }
        let x = arith::round(&val, num.post)?;
        let org = nio::format_numeric(&x)?;
        let (s, body) = split_sign(&org);
        let (n2, p2) = layout_numstr(body.to_string(), &num);
        (n2, s, p2)
    };
    to_char_result(&nodes, &mut num, &numstr, pre, sign)
}

fn numeric_to_number(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let value = arg_text(args, 0)?;
    let fmt = arg_text(args, 1)?;
    if fmt.is_empty() {
        return Ok(Datum::Null);
    }
    let (nodes, mut num) = parse_num_format(fmt)?;
    let numstr = num_processor_from_char(&nodes, &mut num, value.as_bytes())?;
    let scale = num.post;
    let precision = num.pre + num.multi + scale;
    let mut result = nio::parse_numeric(&numstr)?;
    if (0..=numeric::NUMERIC_MAX_PRECISION).contains(&precision) {
        result = nio::apply_typmod(result, ((precision << 16) | scale) + 4)?;
    }
    if num.is_multi() {
        let x = numeric_pow10(-num.multi)?;
        result = arith::mul(&result, &x)?;
    }
    Ok(Datum::Numeric(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::{Interval, TypeId};
    use crate::registry;

    fn call(f: ScalarFn, args: &[Datum]) -> Result<Datum> {
        let io = IoConfig::default();
        let sess = TestSession::default();
        let ctx = FuncCtx {
            env: registry::builtin_env(),
            io: &io,
            session: &sess,
            arg_types: &[],
            result_type: TypeId::TEXT,
            collation: 0,
        };
        f(args, &ctx)
    }

    fn txt(d: Result<Datum>) -> String {
        match d {
            Ok(Datum::Text(s)) => s,
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn ts(y: i32, mo: i32, d: i32, h: i32, mi: i32, s: i32, fsec: i32) -> i64 {
        let tm = Tm {
            year: y,
            mon: mo,
            mday: d,
            hour: h,
            min: mi,
            sec: s,
            fsec,
            ..Tm::default()
        };
        calendar::tm2timestamp(&tm, None).expect("timestamp")
    }

    fn tsc(t: i64, fmt: &str) -> String {
        txt(call(
            timestamp_to_char,
            &[Datum::Timestamp(t), Datum::text(fmt)],
        ))
    }

    fn num(s: &str) -> Datum {
        Datum::Numeric(nio::parse_numeric(s).expect("numeric"))
    }

    fn nc(v: &str, fmt: &str) -> String {
        txt(call(numeric_to_char, &[num(v), Datum::text(fmt)]))
    }

    fn ic(v: i32, fmt: &str) -> String {
        txt(call(int4_to_char, &[Datum::Int4(v), Datum::text(fmt)]))
    }

    #[test]
    fn lookup_covers_all_owned_procs() {
        for p in crate::catalog_data::PROCS
            .iter()
            .filter(|p| p.module == crate::catalog_data::ImplModule::Formatting)
        {
            assert!(
                lookup(p).is_some(),
                "missing implementation for {}",
                p.prosrc
            );
        }
    }

    #[test]
    fn timestamp_acceptance() {
        let t = ts(2002, 4, 20, 17, 31, 12, 660_000);
        assert_eq!(tsc(t, "HH12:MI:SS"), "05:31:12");
        assert_eq!(tsc(t, "HH24:MI:SS.MS"), "17:31:12.660");
        assert_eq!(tsc(t, "HH:MI PM"), "05:31 PM");
        assert_eq!(tsc(t, "US"), "660000");
        assert_eq!(tsc(t, "FF2"), "66");
        assert_eq!(tsc(t, "SSSS"), "63072");
    }

    #[test]
    fn timestamp_names_and_padding() {
        let t = ts(2021, 3, 5, 4, 5, 6, 0);
        assert_eq!(tsc(t, "YYYY-MM-DD HH24:MI:SS"), "2021-03-05 04:05:06");
        assert_eq!(tsc(t, "Day, DD Month YYYY"), "Friday   , 05 March     2021");
        assert_eq!(tsc(t, "FMDay, FMDD FMMonth YYYY"), "Friday, 5 March 2021");
        assert_eq!(tsc(t, "Dy Mon DY MON dy mon"), "Fri Mar FRI MAR fri mar");
        assert_eq!(tsc(t, "DDD IW Q J"), "064 09 1 2459279");
        assert_eq!(tsc(t, "RM"), "III ");
        assert_eq!(tsc(t, "FMrm"), "iii");
        assert_eq!(tsc(t, "DDth"), "05th");
        assert_eq!(tsc(t, "FMDDth"), "5th");
        assert_eq!(tsc(t, "\"Year:\" YYYY"), "Year: 2021");
        assert_eq!(tsc(t, "AD BC a.d. B.C."), "AD AD a.d. A.D.");
        assert_eq!(tsc(t, "D ID CC Y,YYY"), "6 5 21 2,021");
    }

    #[test]
    fn timestamp_null_cases() {
        assert_eq!(
            call(timestamp_to_char, &[Datum::Timestamp(0), Datum::text("")]).unwrap(),
            Datum::Null
        );
        assert_eq!(
            call(
                timestamp_to_char,
                &[Datum::Timestamp(i64::MAX), Datum::text("YYYY")]
            )
            .unwrap(),
            Datum::Null
        );
    }

    #[test]
    fn timestamptz_utc() {
        let t = ts(2021, 3, 5, 4, 5, 6, 0);
        let r = txt(call(
            timestamptz_to_char,
            &[
                Datum::TimestampTz(t),
                Datum::text("HH24:MI TZ TZH:TZM OF tz"),
            ],
        ));
        assert_eq!(r, "04:05 UTC +00:00 +00 utc");
    }

    #[test]
    fn interval_to_char_basic() {
        let iv = Interval::new(0, 0, (15 * 3600 + 2 * 60 + 12) * 1_000_000);
        assert_eq!(
            txt(call(
                interval_to_char,
                &[Datum::Interval(iv), Datum::text("HH24:MI:SS")]
            )),
            "15:02:12"
        );
        let err = call(interval_to_char, &[Datum::Interval(iv), Datum::text("Day")]).unwrap_err();
        assert_eq!(err.sqlstate, SqlState::INVALID_DATETIME_FORMAT);
    }

    #[test]
    fn number_acceptance() {
        assert_eq!(
            txt(call(
                float4_to_char,
                &[Datum::Float4(125.8), Datum::text("999D99")]
            )),
            " 125.80"
        );
        assert_eq!(nc("-0.1", "FM9.99"), "-.1");
        assert_eq!(ic(485, "RN"), "        CDLXXXV");
    }

    #[test]
    fn number_to_char_misc() {
        assert_eq!(ic(485, "999"), " 485");
        assert_eq!(ic(-485, "999"), "-485");
        assert_eq!(ic(12, "99"), " 12");
        assert_eq!(ic(1234, "FM9,999"), "1,234");
        assert_eq!(ic(-5, "999PR"), "  <5>");
        assert_eq!(ic(5, "S999"), "  +5");
        assert_eq!(ic(12345, "99"), " ##");
        assert_eq!(ic(485, "FMRN"), "CDLXXXV");
        assert_eq!(ic(0, "9"), " 0");
        assert_eq!(nc("1234.5", "9,999.99"), " 1,234.50");
        assert_eq!(nc("-12.345", "99.99"), "-12.35");
        assert_eq!(nc("0.5", "0.9"), " 0.5");
        assert_eq!(nc("1234.5", "9.99EEEE"), " 1.23e+03");
        assert_eq!(
            txt(call(int8_to_char, &[Datum::Int8(42), Datum::text("0000")])),
            " 0042"
        );
        assert_eq!(
            txt(call(
                float8_to_char,
                &[Datum::Float8(1.5), Datum::text("9.99")]
            )),
            " 1.50"
        );
    }

    #[test]
    fn number_format_errors() {
        let e = call(int4_to_char, &[Datum::Int4(1), Datum::text("9.9.9")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
        let e = call(int4_to_char, &[Datum::Int4(1), Datum::text("RN9PR")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::SYNTAX_ERROR);
    }

    #[test]
    fn to_number_cases() {
        let r = call(
            numeric_to_number,
            &[Datum::text("12,454.8-"), Datum::text("99G999D9S")],
        )
        .unwrap();
        assert_eq!(r, num("-12454.8"));
        let r = call(
            numeric_to_number,
            &[Datum::text("1,234.5"), Datum::text("9,999.9")],
        )
        .unwrap();
        assert_eq!(r, num("1234.5"));
        let r = call(
            numeric_to_number,
            &[Datum::text("CDLXXXV"), Datum::text("RN")],
        )
        .unwrap();
        assert_eq!(r, num("485"));
        let r = call(
            numeric_to_number,
            &[Datum::text("<12.5>"), Datum::text("999.9PR")],
        )
        .unwrap();
        assert_eq!(r, num("-12.5"));
        assert_eq!(
            call(numeric_to_number, &[Datum::text("1"), Datum::text("")]).unwrap(),
            Datum::Null
        );
    }

    #[test]
    fn to_date_cases() {
        let r = call(
            to_date,
            &[Datum::text("05 Dec 2000"), Datum::text("DD Mon YYYY")],
        )
        .unwrap();
        assert_eq!(
            r,
            Datum::Date(calendar::date2j(2000, 12, 5) - POSTGRES_EPOCH_JDATE)
        );
        let r = call(
            to_date,
            &[Datum::text("2021-03-05"), Datum::text("YYYY-MM-DD")],
        )
        .unwrap();
        assert_eq!(
            r,
            Datum::Date(calendar::date2j(2021, 3, 5) - POSTGRES_EPOCH_JDATE)
        );
        let e = call(
            to_date,
            &[Datum::text("2021-02-30"), Datum::text("YYYY-MM-DD")],
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, SqlState::DATETIME_FIELD_OVERFLOW);
        assert_eq!(
            e.message,
            "date/time field value out of range: \"2021-02-30\""
        );
        let e = call(to_date, &[Datum::text("xx"), Datum::text("MON")]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_DATETIME_FORMAT);
        assert_eq!(e.message, "invalid value \"xx\" for \"MON\"");
    }

    #[test]
    fn to_timestamp_cases() {
        let r = call(
            to_timestamp,
            &[
                Datum::text("2021-03-05 14:30:15"),
                Datum::text("YYYY-MM-DD HH24:MI:SS"),
            ],
        )
        .unwrap();
        assert_eq!(r, Datum::TimestampTz(ts(2021, 3, 5, 14, 30, 15, 0)));
        let r = call(
            to_timestamp,
            &[
                Datum::text("2021-03-05 03:04 PM"),
                Datum::text("YYYY-MM-DD HH:MI PM"),
            ],
        )
        .unwrap();
        assert_eq!(r, Datum::TimestampTz(ts(2021, 3, 5, 15, 4, 0, 0)));
        let r = call(
            to_timestamp,
            &[
                Datum::text("2021-03-05 10:00 +05:30"),
                Datum::text("YYYY-MM-DD HH24:MI TZH:TZM"),
            ],
        )
        .unwrap();
        assert_eq!(r, Datum::TimestampTz(ts(2021, 3, 5, 4, 30, 0, 0)));
        let e = call(
            to_timestamp,
            &[
                Datum::text("2021-03-05 13:00 PM"),
                Datum::text("YYYY-MM-DD HH:MI PM"),
            ],
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, SqlState::INVALID_DATETIME_FORMAT);
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(int_to_roman(1994), "MCMXCIV");
        assert_eq!(int_to_roman(0), "###############");
        assert_eq!(int_to_roman(4000), "###############");
    }
}
