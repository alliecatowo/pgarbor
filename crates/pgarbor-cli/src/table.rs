//! psql-style result tables (lease CLI-01). Pure string rendering, no I/O.
//!
//! ## Aligned format (`psql` default, `\pset format aligned`, border 1)
//! ```text
//!  id | name  |  price
//! ----+-------+--------
//!   1 | apple |   1.50
//!  22 | pear  |
//! (2 rows)
//! ```
//! * Column width = max display width (chars) of header and cells. Header cells are CENTERED
//!   (extra space goes to the right); data cells are LEFT aligned except numeric columns
//!   (`right_align[i] == true`) which are RIGHT aligned. Cells are separated by ` | `; each line starts
//!   with one space and the last cell is not padded when left-aligned (no trailing spaces).
//! * The rule line is `-` repeated `width + 2` per column joined by `+`.
//! * NULL is rendered by the caller as the empty string (psql default).
//! * Footer `(N rows)` / `(1 row)` / `(0 rows)`; omitted when `footer == false`.
//! * Values containing `\n` are printed raw (psql's `+` continuation markers are a documented gap).
//! * A result with zero columns prints only the footer-less empty string.
//!
//! ## CSV (`render_csv`)
//! RFC 4180: fields containing `,` `"` `\n` `\r` are quoted with `"` doubled; header row first; rows
//! end with `\n`; NULL (`None`) is an empty unquoted field, empty string is `""`.
//!
//! ## Expanded (`render_expanded`, psql `\x`)
//! ```text
//! -[ RECORD 1 ]-----
//! id   | 1
//! name | apple
//! ```
//! Header line `-[ RECORD n ]` padded with `-` to (name width + 3 + value width max), names left
//! aligned padded to the longest name.
use crate::ctx::CliError;

/// A table to print.
// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Per column: right-align (numeric types).
    pub right_align: Vec<bool>,
}

/// Aligned output (module docs).
pub fn render_aligned(t: &Table, footer: bool) -> Result<String, CliError> {
    if t.headers.is_empty() {
        return Ok(String::new());
    }
    let count = t.headers.len();
    let mut widths: Vec<usize> = t.headers.iter().map(|s| s.chars().count()).collect();
    for row in &t.rows {
        for (i, width) in widths.iter_mut().enumerate() {
            if let Some(Some(value)) = row.get(i) {
                *width = (*width).max(value.chars().count());
            }
        }
    }
    let mut out = String::new();
    out.push(' ');
    for i in 0..count {
        if i > 0 { out.push_str(" | "); }
        let header = &t.headers[i];
        let len = header.chars().count();
        let left = widths[i].saturating_sub(len) / 2;
        let right = widths[i].saturating_sub(len) - left;
        out.push_str(&" ".repeat(left));
        out.push_str(header);
        out.push_str(&" ".repeat(right));
    }
    out.push('\n');
    for i in 0..count {
        if i > 0 { out.push('+'); }
        out.push_str(&"-".repeat(widths[i] + 2));
    }
    for row in &t.rows {
        out.push('\n');
        out.push(' ');
        for i in 0..count {
            if i > 0 { out.push_str(" | "); }
            let value = row.get(i).and_then(Option::as_deref).unwrap_or("");
            let len = value.chars().count();
            let pad = widths[i].saturating_sub(len);
            if t.right_align.get(i).copied().unwrap_or(false) {
                out.push_str(&" ".repeat(pad));
                out.push_str(value);
            } else {
                out.push_str(value);
                if i + 1 < count { out.push_str(&" ".repeat(pad)); }
            }
        }
    }
    if footer {
        out.push('\n');
        let n = t.rows.len();
        out.push_str(&format!("({n} row{})", if n == 1 { "" } else { "s" }));
    }
    Ok(out)
}

/// CSV output (module docs).
pub fn render_csv(t: &Table) -> Result<String, CliError> {
    let mut out = String::new();
    write_csv_record(&mut out, t.headers.iter().map(|s| Some(s.as_str())));
    for row in &t.rows {
        write_csv_record(&mut out, (0..t.headers.len()).map(|i| row.get(i).and_then(Option::as_deref)));
    }
    Ok(out)
}

/// Expanded output (module docs).
pub fn render_expanded(t: &Table) -> Result<String, CliError> {
    let mut out = String::new();
    let name_width = t.headers.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    for (row_index, row) in t.rows.iter().enumerate() {
        if row_index > 0 { out.push('\n'); }
        let value_width = (0..t.headers.len()).map(|i| row.get(i).and_then(Option::as_deref).unwrap_or("").chars().count()).max().unwrap_or(0);
        let title = format!("-[ RECORD {} ]", row_index + 1);
        out.push_str(&title);
        let target = name_width.saturating_add(3).saturating_add(value_width);
        out.push_str(&"-".repeat(target.max(title.chars().count() + 5).saturating_sub(title.chars().count())));
        for (i, name) in t.headers.iter().enumerate() {
            out.push('\n');
            out.push_str(name);
            out.push_str(&" ".repeat(name_width.saturating_sub(name.chars().count())));
            out.push_str(" | ");
            out.push_str(row.get(i).and_then(Option::as_deref).unwrap_or(""));
        }
    }
    Ok(out)
}

fn write_csv_record<'a>(out: &mut String, fields: impl Iterator<Item = Option<&'a str>>) {
    for (i, field) in fields.enumerate() {
        if i > 0 { out.push(','); }
        match field {
            None => {},
            Some(value) => {
                let quote = value.is_empty() || value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r');
                if quote { out.push('"'); }
                for ch in value.chars() {
                    if ch == '"' { out.push_str("\"\""); } else { out.push(ch); }
                }
                if quote { out.push('"'); }
            }
        }
    }
    out.push('\n');
}

/// True for PG numeric type OIDs that psql right-aligns: int2 21, int4 23, int8 20, oid 26, float4 700,
/// float8 701, numeric 1700, money 790.
pub fn is_numeric_oid(oid: u32) -> bool {
    matches!(oid, 20 | 21 | 23 | 26 | 700 | 701 | 790 | 1700)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Table {
        Table { headers: vec!["id".into(), "name".into(), "price".into()],
            rows: vec![vec![Some("1".into()), Some("apple".into()), Some("1.50".into())],
                       vec![Some("22".into()), Some("pear".into()), None]],
            right_align: vec![true, false, true] }
    }

    #[test]
    fn aligned_example_and_footer() {
        assert_eq!(render_aligned(&example(), true).expect("render"),
            " id | name  | price\n----+-------+-------\n  1 | apple |  1.50\n 22 | pear  |      \n(2 rows)");
    }

    #[test]
    fn aligned_zero_and_one_row_footers() {
        let mut t = Table { headers: vec!["x".into()], rows: vec![], right_align: vec![] };
        assert_eq!(render_aligned(&t, true).expect("render"), " x\n---\n(0 rows)");
        t.rows.push(vec![None]);
        assert_eq!(render_aligned(&t, true).expect("render"), " x\n---\n \n(1 row)");
    }

    #[test]
    fn aligned_numeric_cells_and_nulls() {
        let t = Table {
            headers: vec!["n".into(), "v".into()],
            rows: vec![vec![Some("123".into()), None], vec![Some("4".into()), Some("x".into())]],
            right_align: vec![true, false],
        };
        assert_eq!(render_aligned(&t, false).expect("render"),
            "  n  | v\n-----+---\n 123 | \n   4 | x");
    }

    #[test]
    fn csv_quotes_special_fields_and_distinguishes_null_empty() {
        let t = Table { headers: vec!["a,b".into(), "q".into(), "n".into()],
            rows: vec![vec![Some("a\"b".into()), Some("line\nnext".into()), Some(String::new())], vec![None, None, None]],
            right_align: vec![] };
        assert_eq!(render_csv(&t).expect("render"), "\"a,b\",q,n\n\"a\"\"b\",\"line\nnext\",\"\"\n,,\n");
    }

    #[test]
    fn csv_quotes_carriage_returns() {
        let t = Table {
            headers: vec!["value".into()],
            rows: vec![vec![Some("a\rb".into())]],
            right_align: vec![],
        };
        assert_eq!(render_csv(&t).expect("render"), "value\n\"a\rb\"\n");
    }

    #[test]
    fn expanded_two_records() {
        let t = Table { headers: vec!["id".into(), "name".into()],
            rows: vec![vec![Some("1".into()), Some("apple".into())], vec![Some("2".into()), Some("pear".into())]],
            right_align: vec![] };
        assert_eq!(render_expanded(&t).expect("render"), "-[ RECORD 1 ]-----\nid   | 1\nname | apple\n-[ RECORD 2 ]-----\nid   | 2\nname | pear");
    }
}
