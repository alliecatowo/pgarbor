//! `bool` I/O and boolean functions (TYPE-01, TYPE-04). Lease TYP-03.
//!
//! PG18 reference: `src/backend/utils/adt/bool.c`.
//!
//! Text input (`boolin`/`parse_bool_with_len`): leading/trailing whitespace (isspace: space,
//! \t, \n, \r, \v, \f) is trimmed; then case-insensitive match of any unambiguous prefix of
//! `true`/`false`/`yes`/`no` (`t`, `tr`, `tru`, `true`, `y`, `ye`, `yes`, `f`, `fa`..., `n`, `no`),
//! or `on`, `off` (at least 2 chars: `of`, `off`; `o` alone is ambiguous → error), or `1`/`0`.
//! Anything else → 22P02 `invalid input syntax for type boolean: "<input as given>"`.
//! Text output: `t` / `f`. Binary: 1 byte, 0x01 / 0x00 (recv: any non-zero byte is true;
//! wrong length → 22P03 via `io::invalid_binary`).
//!
//! Functions owned by this module (ImplModule::Bool, matched by `proc.prosrc`):
//! `booleq boolne boollt boolgt boolle boolge` (false < true), `btboolcmp` (int4 -1/0/1),
//! `booland_statefunc`/`boolor_statefunc` (plain AND/OR of two non-null bools),
//! `booltext` (bool → text 'true'/'false' — NOTE the cast to text yields the long words, unlike output),
//! `int4_bool` (0 → false, else true), `bool_int4` (false → 0, true → 1),
//! `hashbool`, `hashboolextended` (PG `hash_uint32` of 0/1, see `compare::pg_hash_uint32`).
use crate::catalog_data::BuiltinProc;
use crate::context::{FuncCtx, IoCtx};
use crate::datum::{type_mismatch, Datum, TypeId};
use crate::func::FuncImpl;
use pgarbor_common::{PgError, Result};

/// True for the bytes C `isspace` accepts in the C/POSIX locale (space, \t, \n, \v, \f, \r).
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Port of PG `strncasecmp(value, word, n)` == 0, where `value` is the trimmed input (its end is
/// an implicit NUL) and `word` is a NUL-terminated ASCII keyword. Comparison stops at `n` bytes or
/// at the first NUL, whichever comes first.
fn strncasecmp_eq(value: &[u8], word: &[u8], n: usize) -> bool {
    for i in 0..n {
        let a = value.get(i).copied().unwrap_or(0).to_ascii_lowercase();
        let b = word.get(i).copied().unwrap_or(0).to_ascii_lowercase();
        if a != b {
            return false;
        }
        if a == 0 {
            return true;
        }
    }
    true
}

/// Port of `parse_bool_with_len` for an already-trimmed, non-empty byte string.
fn parse_bool_bytes(v: &[u8]) -> Option<bool> {
    let len = v.len();
    let first = *v.first()?;
    match first.to_ascii_lowercase() {
        b't' if strncasecmp_eq(v, b"true", len) => Some(true),
        b'f' if strncasecmp_eq(v, b"false", len) => Some(false),
        b'y' if strncasecmp_eq(v, b"yes", len) => Some(true),
        b'n' if strncasecmp_eq(v, b"no", len) => Some(false),
        // 'o' is not unique enough: "on" and "off" need at least two characters ("of" = off).
        b'o' if strncasecmp_eq(v, b"on", len.max(2)) => Some(true),
        b'o' if strncasecmp_eq(v, b"off", len.max(2)) => Some(false),
        b'1' if len == 1 => Some(true),
        b'0' if len == 1 => Some(false),
        _ => None,
    }
}

/// Parse PG boolean text (see module docs). Shared by `boolin`, GUC parsing and casts from text.
/// Returns `None` for invalid input (callers build the error).
// CONTRACT
#[allow(unused_variables)]
pub fn parse_bool(s: &str) -> Result<Option<bool>> {
    let bytes = s.as_bytes();
    // A NUL byte cannot appear in PG text; PG's strlen-based parsing would stop there.
    if bytes.contains(&0) {
        return Ok(None);
    }
    let start = bytes.iter().position(|&b| !is_c_space(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|&b| !is_c_space(b)).map_or(start, |i| i + 1);
    Ok(parse_bool_bytes(&bytes[start..end]))
}

/// `boolin`. Example: `input(ctx, TypeId::BOOL, " Yes ")` → `Datum::Bool(true)`.
#[allow(unused_variables)]
pub fn input(ctx: &IoCtx, ty: TypeId, s: &str) -> Result<Datum> {
    match parse_bool(s)? {
        Some(b) => Ok(Datum::Bool(b)),
        None => Err(crate::io::invalid_input_syntax("boolean", s)),
    }
}

/// `boolout`: `t` / `f`.
#[allow(unused_variables)]
pub fn output(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<String> {
    match d {
        Datum::Bool(true) => Ok("t".to_string()),
        Datum::Bool(false) => Ok("f".to_string()),
        other => Err(type_mismatch("Bool", other)),
    }
}

/// `boolrecv`: exactly 1 byte.
#[allow(unused_variables)]
pub fn recv(ctx: &IoCtx, ty: TypeId, buf: &[u8]) -> Result<Datum> {
    match buf {
        [b] => Ok(Datum::Bool(*b != 0)),
        _ => Err(crate::io::invalid_binary("")),
    }
}

/// `boolsend`: `[1]` / `[0]`.
#[allow(unused_variables)]
pub fn send(ctx: &IoCtx, ty: TypeId, d: &Datum) -> Result<Vec<u8>> {
    match d {
        Datum::Bool(b) => Ok(vec![u8::from(*b)]),
        other => Err(type_mismatch("Bool", other)),
    }
}

/// Function registry hook: map `proc.prosrc` to an implementation (see `crate::func`).
#[allow(unused_variables)]
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    // Every Bool-module proc is an internal (lang 12) builtin, so matching on prosrc is sufficient.
    match proc.prosrc {
        "booleq" => Some(FuncImpl::Scalar(booleq)),
        "boolne" => Some(FuncImpl::Scalar(boolne)),
        "boollt" => Some(FuncImpl::Scalar(boollt)),
        "boolgt" => Some(FuncImpl::Scalar(boolgt)),
        "boolle" => Some(FuncImpl::Scalar(boolle)),
        "boolge" => Some(FuncImpl::Scalar(boolge)),
        "btboolcmp" => Some(FuncImpl::Scalar(btboolcmp)),
        "booland_statefunc" => Some(FuncImpl::Scalar(booland_statefunc)),
        "boolor_statefunc" => Some(FuncImpl::Scalar(boolor_statefunc)),
        "booltext" => Some(FuncImpl::Scalar(booltext)),
        "int4_bool" => Some(FuncImpl::Scalar(int4_bool)),
        "bool_int4" => Some(FuncImpl::Scalar(bool_int4)),
        "hashbool" => Some(FuncImpl::Scalar(hashbool)),
        "hashboolextended" => Some(FuncImpl::Scalar(hashboolextended)),
        _ => None,
    }
}

/// 0A000 for a Bool-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-03", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Builtin function bodies. Arguments arrive non-NULL (strictness is applied by the dispatcher).
// ---------------------------------------------------------------------------------------------

fn arg_bool(args: &[Datum], i: usize) -> Result<bool> {
    match args.get(i) {
        Some(Datum::Bool(b)) => Ok(*b),
        Some(other) => Err(type_mismatch("Bool", other)),
        None => Err(PgError::internal(format!("missing bool argument {i}"))),
    }
}

fn arg_i32(args: &[Datum], i: usize) -> Result<i32> {
    match args.get(i) {
        Some(Datum::Int4(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("Int4", other)),
        None => Err(PgError::internal(format!("missing int4 argument {i}"))),
    }
}

fn arg_i64(args: &[Datum], i: usize) -> Result<i64> {
    match args.get(i) {
        Some(Datum::Int8(v)) => Ok(*v),
        Some(other) => Err(type_mismatch("Int8", other)),
        None => Err(PgError::internal(format!("missing int8 argument {i}"))),
    }
}

fn bin_bool(args: &[Datum], f: impl Fn(bool, bool) -> bool) -> Result<Datum> {
    Ok(Datum::Bool(f(arg_bool(args, 0)?, arg_bool(args, 1)?)))
}

fn booleq(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a == b)
}

fn boolne(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a != b)
}

fn boollt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| !a && b)
}

fn boolgt(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a && !b)
}

fn boolle(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| !a || b)
}

fn boolge(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a || !b)
}

fn btboolcmp(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let a = arg_bool(args, 0)? as i32;
    let b = arg_bool(args, 1)? as i32;
    Ok(Datum::Int4(a - b))
}

fn booland_statefunc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a && b)
}

fn boolor_statefunc(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    bin_bool(args, |a, b| a || b)
}

fn booltext(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let b = arg_bool(args, 0)?;
    Ok(Datum::text(if b { "true" } else { "false" }))
}

fn int4_bool(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bool(arg_i32(args, 0)? != 0))
}

fn bool_int4(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int4(arg_bool(args, 0)? as i32))
}

fn hashbool(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let h = crate::compare::pg_hash_uint32(u32::from(arg_bool(args, 0)?))?;
    Ok(Datum::Int4(h as i32))
}

fn hashboolextended(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let v = u32::from(arg_bool(args, 0)?);
    let seed = arg_i64(args, 1)? as u64;
    let h = crate::compare::pg_hash_uint32_extended(v, seed)?;
    Ok(Datum::Int8(h as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{IoConfig, TestSession};
    use crate::func;
    use crate::registry;

    /// Run `f` with a default (oracle-configured) I/O context and a function context.
    fn with_ctx<R>(f: impl FnOnce(&IoCtx, &FuncCtx) -> R) -> R {
        let cfg = IoConfig::default();
        let env = registry::builtin_env();
        let ictx = IoCtx::new(env, &cfg);
        let session = TestSession::default();
        let tys = [TypeId::BOOL, TypeId::BOOL];
        let fctx = FuncCtx {
            env,
            io: &cfg,
            session: &session,
            arg_types: &tys,
            result_type: TypeId::BOOL,
            collation: 0,
        };
        f(&ictx, &fctx)
    }

    fn sqlstate_of<T: std::fmt::Debug>(r: Result<T>) -> [u8; 5] {
        r.unwrap_err().sqlstate.0
    }

    #[test]
    fn parse_accepts_unambiguous_prefixes() {
        for s in ["t", "tr", "tru", "true", "TRUE", "True", "y", "ye", "yes", "YES", "on", "ON", "1"] {
            assert_eq!(parse_bool(s).unwrap(), Some(true), "input {s:?}");
        }
        for s in ["f", "fa", "fals", "false", "FALSE", "n", "no", "No", "off", "OFF", "of", "Of", "0"] {
            assert_eq!(parse_bool(s).unwrap(), Some(false), "input {s:?}");
        }
    }

    #[test]
    fn parse_trims_c_whitespace() {
        assert_eq!(parse_bool(" Yes ").unwrap(), Some(true));
        assert_eq!(parse_bool("\t\n\x0b\x0c\rfalse \r\n").unwrap(), Some(false));
        assert_eq!(parse_bool("   ").unwrap(), None);
        assert_eq!(parse_bool("").unwrap(), None);
    }

    #[test]
    fn parse_rejects_invalid() {
        for s in ["o", "x", "truex", "trueX", "onx", "offx", "11", "00", "2", " 1 0", "t\0", "yess", "nope", "f1"] {
            assert_eq!(parse_bool(s).unwrap(), None, "input {s:?}");
        }
    }

    #[test]
    fn input_error_matches_pg() {
        with_ctx(|ctx, _| {
            let err = input(ctx, TypeId::BOOL, "o").unwrap_err();
            assert_eq!(err.sqlstate, crate::io::invalid_input_syntax("boolean", "o").sqlstate);
            assert_eq!(err.message, "invalid input syntax for type boolean: \"o\"");
            // The message quotes the input as given, not the trimmed text.
            let err = input(ctx, TypeId::BOOL, " x ").unwrap_err();
            assert_eq!(err.message, "invalid input syntax for type boolean: \" x \"");
            assert_eq!(input(ctx, TypeId::BOOL, " Yes ").unwrap(), Datum::Bool(true));
            assert_eq!(input(ctx, TypeId::BOOL, "of").unwrap(), Datum::Bool(false));
        });
    }

    #[test]
    fn output_and_send() {
        with_ctx(|ctx, _| {
            assert_eq!(output(ctx, TypeId::BOOL, &Datum::Bool(true)).unwrap(), "t");
            assert_eq!(output(ctx, TypeId::BOOL, &Datum::Bool(false)).unwrap(), "f");
            assert_eq!(send(ctx, TypeId::BOOL, &Datum::Bool(true)).unwrap(), vec![1u8]);
            assert_eq!(send(ctx, TypeId::BOOL, &Datum::Bool(false)).unwrap(), vec![0u8]);
        });
    }

    #[test]
    fn recv_rules() {
        with_ctx(|ctx, _| {
            assert_eq!(recv(ctx, TypeId::BOOL, &[2]).unwrap(), Datum::Bool(true));
            assert_eq!(recv(ctx, TypeId::BOOL, &[0]).unwrap(), Datum::Bool(false));
            assert_eq!(sqlstate_of(recv(ctx, TypeId::BOOL, &[])), *b"22P03");
            assert_eq!(sqlstate_of(recv(ctx, TypeId::BOOL, &[1, 0])), *b"22P03");
        });
    }

    #[test]
    fn booltext_uses_long_words() {
        with_ctx(|ctx, fctx| {
            assert_eq!(func::call(2971, &[Datum::Bool(true)], fctx).unwrap(), Datum::text("true"));
            assert_eq!(func::call(2971, &[Datum::Bool(false)], fctx).unwrap(), Datum::text("false"));
            let _ = ctx;
        });
    }

    #[test]
    fn comparison_and_logic_functions() {
        with_ctx(|_, fctx| {
            let t = Datum::Bool(true);
            let f = Datum::Bool(false);
            let call2 = |oid, a: &Datum, b: &Datum| func::call(oid, &[a.clone(), b.clone()], fctx).unwrap();
            // booleq 60, boolne 84, boollt 56, boolgt 57, boolle 1691, boolge 1692
            assert_eq!(call2(60, &t, &t), Datum::Bool(true));
            assert_eq!(call2(60, &f, &t), Datum::Bool(false));
            assert_eq!(call2(84, &f, &t), Datum::Bool(true));
            assert_eq!(call2(56, &f, &t), Datum::Bool(true));
            assert_eq!(call2(56, &t, &f), Datum::Bool(false));
            assert_eq!(call2(57, &t, &f), Datum::Bool(true));
            assert_eq!(call2(57, &f, &t), Datum::Bool(false));
            assert_eq!(call2(1691, &t, &f), Datum::Bool(false));
            assert_eq!(call2(1691, &f, &f), Datum::Bool(true));
            assert_eq!(call2(1692, &f, &t), Datum::Bool(false));
            assert_eq!(call2(1692, &t, &t), Datum::Bool(true));
            // btboolcmp 1693
            assert_eq!(call2(1693, &f, &t), Datum::Int4(-1));
            assert_eq!(call2(1693, &t, &f), Datum::Int4(1));
            assert_eq!(call2(1693, &t, &t), Datum::Int4(0));
            // booland_statefunc 2515, boolor_statefunc 2516
            assert_eq!(call2(2515, &t, &f), Datum::Bool(false));
            assert_eq!(call2(2515, &t, &t), Datum::Bool(true));
            assert_eq!(call2(2516, &f, &t), Datum::Bool(true));
            assert_eq!(call2(2516, &f, &f), Datum::Bool(false));
        });
    }

    #[test]
    fn int_bool_casts() {
        with_ctx(|_, fctx| {
            // int4 -> bool is oid 2557, bool -> int4 is oid 2558
            assert_eq!(func::call(2557, &[Datum::Int4(0)], fctx).unwrap(), Datum::Bool(false));
            assert_eq!(func::call(2557, &[Datum::Int4(-7)], fctx).unwrap(), Datum::Bool(true));
            assert_eq!(func::call(2558, &[Datum::Bool(true)], fctx).unwrap(), Datum::Int4(1));
            assert_eq!(func::call(2558, &[Datum::Bool(false)], fctx).unwrap(), Datum::Int4(0));
        });
    }

    #[test]
    fn every_bool_proc_has_an_implementation() {
        for oid in [56, 57, 60, 84, 1691, 1692, 1693, 2515, 2516, 2557, 2558, 2971, 6417, 6418] {
            assert!(func::implementation(oid).is_ok(), "proc {oid} has no implementation");
        }
    }

    #[test]
    fn strict_null_handled_by_dispatcher() {
        with_ctx(|_, fctx| {
            assert_eq!(func::call(60, &[Datum::Null, Datum::Bool(true)], fctx).unwrap(), Datum::Null);
        });
    }

    #[test]
    #[ignore = "needs TYP-49 (compare::pg_hash_uint32)"]
    fn hash_fixtures() {
        with_ctx(|_, fctx| {
            // hash_uint32 is PG's lookup3 port; values checked centrally against PG 18.6.
            let _ = func::call(6417, &[Datum::Bool(true)], fctx).unwrap();
            let _ = func::call(6418, &[Datum::Bool(false), Datum::Int8(0)], fctx).unwrap();
        });
    }
}
