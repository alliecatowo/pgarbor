//! OID-like types: oid, xid, cid, xid8, pg_lsn, reg* aliases, int2vector, oidvector
//! (TYPE-01, TYPE-02, TYPE-15). Lease TYP-08.
//!
//! PG18 reference: `utils/adt/oid.c`, `xid.c`, `regproc.c`, `int.c` (int2vector), `pg_lsn.c`, `numutils.c`
//! (`uint32in_subr`, `uint64in_subr`).
//!
//! Datum mapping: oid/xid/cid/reg* → `Datum::Oid(u32)`; xid8 and pg_lsn → `Datum::Int8` (u64 bits);
//! int2vector → 1-D `Array{elem: INT2, lbound 0}`; oidvector → 1-D `Array{elem: OID, lbound 0}`.
//!
//! Text I/O:
//! - `oid`: unsigned 32-bit decimal; input also accepts a NEGATIVE value >= -2147483648 and wraps it
//!   (`'-1'::oid` = 4294967295) like `strtoul`; trailing junk → 22P02 `invalid input syntax for type oid: "x"`;
//!   out of range → 22003 `value "4294967296" is out of range for type oid`. Output: unsigned decimal.
//! - `xid`, `cid`: like oid (messages say `xid`/`cid`). `xid8`: unsigned 64-bit decimal.
//! - `pg_lsn`: `XXXXXXXX/XXXXXXXX` uppercase hex halves (`0/16B6C50`); invalid → 22P02
//!   `invalid input syntax for type pg_lsn: "x"`.
//! - `regclass`, `regtype`, `regproc`, `regprocedure`, `regoper`, `regoperator`, `regnamespace`,
//!   `regrole`, `regcollation`, `regconfig`, `regdictionary`:
//!   input: all-digits string → that OID (no lookup); `-` → 0; otherwise name lookup via
//!   `ctx.env.reg_lookup(kind, text)` (for `regtype`, first try `names::parse_type_string`);
//!   not found → regclass 42P01 `relation "x" does not exist`, regtype 42704 `type "x" does not exist`,
//!   regproc 42883 `function "x" does not exist`, regnamespace 3F000 `schema "x" does not exist`,
//!   regrole 42704 `role "x" does not exist`, regoper 42883 `operator does not exist: x`.
//!   output: 0 → `-`; else `ctx.env.reg_name(kind, oid)`; for regtype fall back to
//!   `names::format_type(env, oid, -1)`; if the object does not exist print the bare number.
//! - `int2vector` / `oidvector`: space-separated decimal list (`"1 2 3"`), output same; empty → `""`.
//!
//! Binary: oid/xid/cid/reg* = u32 big-endian (4 bytes); xid8/pg_lsn = u64 big-endian;
//! int2vector/oidvector = the PG array binary format (delegate to `array::io` with lbound 0).
//!
//! Functions owned (ImplModule::OidTypes): `oideq oidne oidlt oidle oidgt oidge btoidcmp oidlarger oidsmaller
//! hashoid(+extended) oidtoi8 i8tooid` (i8tooid: out of [0, 2^32) → 22003 `OID out of range`),
//! `oidvector*` comparisons (element-wise then length), `xideq xidneq cideq xid8eq/ne/lt/gt/le/ge xid8cmp
//! xid8_larger xid8_smaller xid8toxid hashxid hashxid8 hashcid (+extended)`.
use crate::catalog_data::BuiltinProc;
use crate::context::{IoCtx, RegKind};
use crate::datum::{Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{Oid, PgError, Result};

/// The reg* kind for a reg type OID (regclass → `RegKind::Class`, ...), or None for plain oid/xid/cid.
pub fn reg_kind_of(type_oid: Oid) -> Option<RegKind> {
    use crate::oids;
    match type_oid {
        oids::REGCLASS => Some(RegKind::Class),
        oids::REGTYPE => Some(RegKind::Type),
        oids::REGPROC => Some(RegKind::Proc),
        oids::REGPROCEDURE => Some(RegKind::Procedure),
        oids::REGOPER => Some(RegKind::Oper),
        oids::REGOPERATOR => Some(RegKind::Operator),
        oids::REGNAMESPACE => Some(RegKind::Namespace),
        oids::REGROLE => Some(RegKind::Role),
        oids::REGCOLLATION => Some(RegKind::Collation),
        oids::REGCONFIG => Some(RegKind::Config),
        oids::REGDICTIONARY => Some(RegKind::Dictionary),
        _ => None,
    }
}

/// Parse an unsigned 32-bit OID-style value (PG `uint32in_subr`); `type_name` for messages.
// CONTRACT
#[allow(unused_variables)]
pub fn parse_oid(s: &str, type_name: &str) -> Result<u32> {
    Err(PgError::unimplemented("TYP-08", "oid input"))
}

/// Text input for every type in this family (see module docs).
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-08", "oid-like type input"))
}

/// Text output for every type in this family.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    Err(PgError::unimplemented("TYP-08", "oid-like type output"))
}

/// Binary input.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    Err(PgError::unimplemented("TYP-08", "oid-like type binary input"))
}

/// Binary output.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    Err(PgError::unimplemented("TYP-08", "oid-like type binary output"))
}

/// Function registry hook (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    None
}

/// 0A000 for an OidTypes-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-08", format!("function {}", proc.name))
}
