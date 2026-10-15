//! Character types (text, varchar, bpchar, name, "char") and string functions (TYPE-13).
//!
//! Collation: v1 implements only the default collation of the pinned oracle
//! (`C.UTF-8`, builtin provider) = Unicode code point order = UTF-8 byte order. `lower`/`upper`/
//! `initcap`/ILIKE use the builtin provider's simple case mapping (`char::to_lowercase`/`to_uppercase`
//! when the mapping is a single char; PG18 builtin C.UTF-8 also maps non-ASCII).
pub mod format;
pub mod funcs;
pub mod io;
pub mod like;
pub mod regex;
