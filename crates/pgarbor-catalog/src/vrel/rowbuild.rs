//! Helpers for producers: typed `Datum` constructors and a by-name [`RowBuilder`] so a producer
//! never counts columns by hand. Fully implemented, frozen (v1).
//!
//! ```ignore
//! let schema = builtin::system_rel(relid::PG_NAMESPACE).unwrap();
//! let mut b = RowBuilder::new(schema);
//! b.set("oid", d::oid(2200)).set("nspname", d::name("public")).set("nspowner", d::oid(6171));
//! let row = b.finish()?; // unset columns are NULL
//! ```
use super::Row;
use crate::builtin::SystemRel;
use crate::model::{Acl, AclItem};
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{Oid, PgError, Result};
use pgarbor_types::array::Array;
use pgarbor_types::{oids, Datum};

/// Builds one row addressed by column name.
// CONTRACT
pub struct RowBuilder {
    rel: &'static SystemRel,
    values: Vec<Datum>,
    error: Option<PgError>,
}

impl RowBuilder {
    /// All columns NULL.
    pub fn new(rel: &'static SystemRel) -> RowBuilder {
        RowBuilder { rel, values: vec![Datum::Null; rel.columns.len()], error: None }
    }

    /// Set a column by name. An unknown name is remembered and reported by `finish` as XX000
    /// `<relation> has no column "x"` (a producer bug, never user input).
    pub fn set(&mut self, column: &str, value: Datum) -> &mut RowBuilder {
        match self.rel.columns.iter().position(|c| c.name == column) {
            Some(i) => self.values[i] = value,
            None => {
                if self.error.is_none() {
                    self.error = Some(PgError::internal(format!("{} has no column \"{column}\"", self.rel.name)));
                }
            }
        }
        self
    }

    /// The finished row.
    pub fn finish(self) -> Result<Row> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.values),
        }
    }
}

/// Datum constructors (`d::text("x")`).
pub mod d {
    use super::*;

    pub fn text(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }
    /// `name` columns are text too.
    pub fn name(s: &str) -> Datum {
        Datum::Text(s.to_string())
    }
    pub fn opt_text(s: Option<&str>) -> Datum {
        s.map(text).unwrap_or(Datum::Null)
    }
    pub fn oid(o: Oid) -> Datum {
        Datum::Oid(o)
    }
    pub fn bool_(b: bool) -> Datum {
        Datum::Bool(b)
    }
    pub fn int2(v: i16) -> Datum {
        Datum::Int2(v)
    }
    pub fn int4(v: i32) -> Datum {
        Datum::Int4(v)
    }
    pub fn int8(v: i64) -> Datum {
        Datum::Int8(v)
    }
    pub fn float4(v: f32) -> Datum {
        Datum::Float4(v)
    }
    /// `"char"` column.
    pub fn char_(c: u8) -> Datum {
        Datum::Char(c)
    }
    pub fn opt_int4(v: Option<i32>) -> Datum {
        v.map(Datum::Int4).unwrap_or(Datum::Null)
    }
    pub fn opt_int8(v: Option<i64>) -> Datum {
        v.map(Datum::Int8).unwrap_or(Datum::Null)
    }
    pub fn opt_oid(v: Option<Oid>) -> Datum {
        v.map(Datum::Oid).unwrap_or(Datum::Null)
    }
    pub fn opt_tstz(v: Option<i64>) -> Datum {
        v.map(Datum::TimestampTz).unwrap_or(Datum::Null)
    }
    /// `oid[]` (empty slice -> empty array, NOT NULL).
    pub fn oid_array(v: &[Oid]) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::OID, v.iter().map(|o| Datum::Oid(*o)).collect())))
    }
    /// `smallint[]`.
    pub fn int2_array(v: &[i16]) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::INT2, v.iter().map(|x| Datum::Int2(*x)).collect())))
    }
    /// `text[]`.
    pub fn text_array(v: &[String]) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::TEXT, v.iter().map(|s| Datum::Text(s.clone())).collect())))
    }
    /// `name[]`.
    pub fn name_array(v: &[String]) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::NAME, v.iter().map(|s| Datum::Text(s.clone())).collect())))
    }
    /// `"char"[]`.
    pub fn char_array(v: &[u8]) -> Datum {
        Datum::Array(Box::new(Array::from_vec(oids::CHAR, v.iter().map(|c| Datum::Char(*c)).collect())))
    }
    /// `int2vector` (lower bound 0; empty -> no dims).
    pub fn int2vector(v: &[i16]) -> Datum {
        let mut a = Array::from_vec(oids::INT2, v.iter().map(|x| Datum::Int2(*x)).collect());
        if !v.is_empty() {
            a.dims = vec![(v.len() as i32, 0)];
        }
        Datum::Array(Box::new(a))
    }
    /// `oidvector` (lower bound 0; empty -> no dims).
    pub fn oidvector(v: &[Oid]) -> Datum {
        let mut a = Array::from_vec(oids::OID, v.iter().map(|o| Datum::Oid(*o)).collect());
        if !v.is_empty() {
            a.dims = vec![(v.len() as i32, 0)];
        }
        Datum::Array(Box::new(a))
    }
    /// `aclitem[]`; `None` -> NULL (default privileges).
    pub fn acl_array(acl: Option<&Acl>, snapshot: &CatalogSnapshot) -> Datum {
        match acl {
            None => Datum::Null,
            Some(items) => {
                let texts: Vec<Datum> = items.iter().map(|i| Datum::Text(aclitem_text(i, snapshot))).collect();
                Datum::Array(Box::new(Array::from_vec(oids::ACLITEM, texts)))
            }
        }
    }
}

/// `aclitem` canonical text with role names quoted as PG `putid` does (quotes unless every byte is
/// `[A-Za-z0-9_]`); unknown roles print their numeric OID.
pub fn aclitem_text(item: &AclItem, snapshot: &CatalogSnapshot) -> String {
    let name = |o: Oid| -> String {
        match snapshot.role_name(o) {
            Some(n) => {
                if n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    n.to_string()
                } else {
                    format!("\"{}\"", n.replace('"', "\"\""))
                }
            }
            None => o.to_string(),
        }
    };
    item.to_text(&name)
}
