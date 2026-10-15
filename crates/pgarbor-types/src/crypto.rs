//! Hash digests and checksums (TYPE-07, TYPE-13). Lease TYP-19.
//!
//! The crate has no crypto dependency (deps frozen), so this lease implements MD5 (RFC 1321),
//! SHA-224/256 and SHA-384/512 (FIPS 180-4), CRC-32 (IEEE, reflected, poly 0xEDB88320) and CRC-32C
//! (Castagnoli, reflected, poly 0x82F63B78) in plain Rust. Correctness is checked against the RFC/FIPS
//! test vectors in unit tests (e.g. md5("") = d41d8cd98f00b204e9800998ecf8427e,
//! sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad).
//!
//! Owned procs (ImplModule::Crypto): `md5_text` (md5(text) → lowercase hex text), `md5_bytea`,
//! `sha224_bytea sha256_bytea sha384_bytea sha512_bytea` (→ bytea digest), `crc32_bytea`,
//! `crc32c_bytea` (PG18: → int8, the unsigned 32-bit CRC). PG: `crc32('The quick brown fox jumps over
//! the lazy dog'::bytea)` = 1095738169.
use crate::catalog_data::BuiltinProc;
use crate::context::FuncCtx;
use crate::datum::Datum;
use crate::func::{FuncImpl, ScalarFn};
use pgarbor_common::{PgError, Result};

/// MD5 digest (16 bytes).
// CONTRACT
pub fn md5(data: &[u8]) -> Result<[u8; 16]> {
    let mut state = MD5_INIT;
    for_each_padded_block(data, 64, 8, false, |block| md5_compress(&mut state, block));
    let mut out = [0u8; 16];
    for (dst, word) in out.chunks_exact_mut(4).zip(state.iter()) {
        dst.copy_from_slice(&word.to_le_bytes());
    }
    Ok(out)
}

/// SHA-256 digest.
// CONTRACT
pub fn sha256(data: &[u8]) -> Result<[u8; 32]> {
    Ok(words32_be::<32>(&sha256_state(data, H256)))
}

/// SHA-224 digest.
// CONTRACT
pub fn sha224(data: &[u8]) -> Result<[u8; 28]> {
    Ok(words32_be::<28>(&sha256_state(data, H224)))
}

/// SHA-512 digest.
// CONTRACT
pub fn sha512(data: &[u8]) -> Result<[u8; 64]> {
    Ok(words64_be::<64>(&sha512_state(data, H512)))
}

/// SHA-384 digest.
// CONTRACT
pub fn sha384(data: &[u8]) -> Result<[u8; 48]> {
    Ok(words64_be::<48>(&sha512_state(data, H384)))
}

/// Function registry hook (see `crate::func`).
pub(crate) fn lookup(proc: &BuiltinProc) -> Option<FuncImpl> {
    let f: ScalarFn = match proc.prosrc {
        "md5_text" => md5_text_fn,
        "md5_bytea" => md5_bytea_fn,
        "sha224_bytea" => sha224_bytea_fn,
        "sha256_bytea" => sha256_bytea_fn,
        "sha384_bytea" => sha384_bytea_fn,
        "sha512_bytea" => sha512_bytea_fn,
        "crc32_bytea" => crc32_bytea_fn,
        "crc32c_bytea" => crc32c_bytea_fn,
        _ => return None,
    };
    Some(FuncImpl::Scalar(f))
}

/// 0A000 for a Crypto-module proc without implementation.
pub(crate) fn unsupported(proc: &BuiltinProc) -> PgError {
    PgError::unimplemented("TYP-19", format!("function {}", proc.name))
}

// ---------------------------------------------------------------------------------------------
// Scalar entry points (SQL-visible functions)
// ---------------------------------------------------------------------------------------------

fn md5_text_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let digest = md5(arg_text(args)?.as_bytes())?;
    Ok(Datum::Text(hex_lower(&digest)))
}

fn md5_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    let digest = md5(arg_bytea(args)?)?;
    Ok(Datum::Text(hex_lower(&digest)))
}

fn sha224_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(sha224(arg_bytea(args)?)?.to_vec()))
}

fn sha256_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(sha256(arg_bytea(args)?)?.to_vec()))
}

fn sha384_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(sha384(arg_bytea(args)?)?.to_vec()))
}

fn sha512_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Bytea(sha512(arg_bytea(args)?)?.to_vec()))
}

fn crc32_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int8(i64::from(crc32_ieee(arg_bytea(args)?))))
}

fn crc32c_bytea_fn(args: &[Datum], _ctx: &FuncCtx) -> Result<Datum> {
    Ok(Datum::Int8(i64::from(crc32c(arg_bytea(args)?))))
}

/// First argument as text. Strict procs never pass NULL here; any other variant is an internal error.
fn arg_text(args: &[Datum]) -> Result<&str> {
    match args.first() {
        Some(Datum::Text(s)) => Ok(s.as_str()),
        other => Err(bad_arg("text", other)),
    }
}

/// First argument as bytea.
fn arg_bytea(args: &[Datum]) -> Result<&[u8]> {
    match args.first() {
        Some(Datum::Bytea(b)) => Ok(b.as_slice()),
        other => Err(bad_arg("bytea", other)),
    }
}

fn bad_arg(expected: &str, got: Option<&Datum>) -> PgError {
    PgError::internal(format!(
        "crypto function expected {expected} argument, got {}",
        got.map_or("no value", Datum::variant_name)
    ))
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Lowercase hexadecimal rendering (`md5` output format).
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0F) as usize] as char);
    }
    s
}

/// Feed the message to `f` one block at a time, including the Merkle-Damgard padding: a 0x80 byte,
/// zeros, then the bit length in the last `len_field` bytes (big or little endian). Only the final
/// one or two blocks are copied; the message itself is never copied.
fn for_each_padded_block(
    data: &[u8],
    block: usize,
    len_field: usize,
    big_endian: bool,
    mut f: impl FnMut(&[u8]),
) {
    let mut chunks = data.chunks_exact(block);
    for chunk in &mut chunks {
        f(chunk);
    }
    let rem = chunks.remainder();
    // Largest block size is 128 (SHA-512), so two blocks always fit in 256 bytes.
    let mut tail = [0u8; 256];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let total = if rem.len() + 1 + len_field <= block { block } else { 2 * block };
    let bits = (data.len() as u128) << 3;
    let field = &mut tail[total - len_field..total];
    if big_endian {
        field.copy_from_slice(&bits.to_be_bytes()[16 - len_field..]);
    } else {
        field.copy_from_slice(&bits.to_le_bytes()[..len_field]);
    }
    for chunk in tail[..total].chunks_exact(block) {
        f(chunk);
    }
}

fn words32_be<const N: usize>(h: &[u32; 8]) -> [u8; N] {
    let mut out = [0u8; N];
    for (dst, word) in out.chunks_exact_mut(4).zip(h.iter()) {
        dst.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn words64_be<const N: usize>(h: &[u64; 8]) -> [u8; N] {
    let mut out = [0u8; N];
    for (dst, word) in out.chunks_exact_mut(8).zip(h.iter()) {
        dst.copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// MD5 (RFC 1321)
// ---------------------------------------------------------------------------------------------

const MD5_INIT: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];

/// Per-round left-rotation amounts.
const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `K[i] = floor(|sin(i + 1)| * 2^32)`.
const MD5_K: [u32; 64] = [
    0xD76A_A478, 0xE8C7_B756, 0x2420_70DB, 0xC1BD_CEEE, 0xF57C_0FAF, 0x4787_C62A, 0xA830_4613, 0xFD46_9501,
    0x6980_98D8, 0x8B44_F7AF, 0xFFFF_5BB1, 0x895C_D7BE, 0x6B90_1122, 0xFD98_7193, 0xA679_438E, 0x49B4_0821,
    0xF61E_2562, 0xC040_B340, 0x265E_5A51, 0xE9B6_C7AA, 0xD62F_105D, 0x0244_1453, 0xD8A1_E681, 0xE7D3_FBC8,
    0x21E1_CDE6, 0xC337_07D6, 0xF4D5_0D87, 0x455A_14ED, 0xA9E3_E905, 0xFCEF_A3F8, 0x676F_02D9, 0x8D2A_4C8A,
    0xFFFA_3942, 0x8771_F681, 0x6D9D_6122, 0xFDE5_380C, 0xA4BE_EA44, 0x4BDE_CFA9, 0xF6BB_4B60, 0xBEBF_BC70,
    0x289B_7EC6, 0xEAA1_27FA, 0xD4EF_3085, 0x0488_1D05, 0xD9D4_D039, 0xE6DB_99E5, 0x1FA2_7CF8, 0xC4AC_5665,
    0xF429_2244, 0x432A_FF97, 0xAB94_23A7, 0xFC93_A039, 0x655B_59C3, 0x8F0C_CC92, 0xFFEF_F47D, 0x8584_5DD1,
    0x6FA8_7E4F, 0xFE2C_E6E0, 0xA301_4314, 0x4E08_11A1, 0xF753_7E82, 0xBD3A_F235, 0x2AD7_D2BB, 0xEB86_D391,
];

fn md5_compress(state: &mut [u32; 4], block: &[u8]) {
    let mut m = [0u32; 16];
    for (word, c) in m.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64usize {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let f = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(f.rotate_left(MD5_S[i]));
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

// ---------------------------------------------------------------------------------------------
// SHA-224 / SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------------------------

const K256: [u32; 64] = [
    0x428A_2F98, 0x7137_4491, 0xB5C0_FBCF, 0xE9B5_DBA5, 0x3956_C25B, 0x59F1_11F1, 0x923F_82A4, 0xAB1C_5ED5,
    0xD807_AA98, 0x1283_5B01, 0x2431_85BE, 0x550C_7DC3, 0x72BE_5D74, 0x80DE_B1FE, 0x9BDC_06A7, 0xC19B_F174,
    0xE49B_69C1, 0xEFBE_4786, 0x0FC1_9DC6, 0x240C_A1CC, 0x2DE9_2C6F, 0x4A74_84AA, 0x5CB0_A9DC, 0x76F9_88DA,
    0x983E_5152, 0xA831_C66D, 0xB003_27C8, 0xBF59_7FC7, 0xC6E0_0BF3, 0xD5A7_9147, 0x06CA_6351, 0x1429_2967,
    0x27B7_0A85, 0x2E1B_2138, 0x4D2C_6DFC, 0x5338_0D13, 0x650A_7354, 0x766A_0ABB, 0x81C2_C92E, 0x9272_2C85,
    0xA2BF_E8A1, 0xA81A_664B, 0xC24B_8B70, 0xC76C_51A3, 0xD192_E819, 0xD699_0624, 0xF40E_3585, 0x106A_A070,
    0x19A4_C116, 0x1E37_6C08, 0x2748_774C, 0x34B0_BCB5, 0x391C_0CB3, 0x4ED8_AA4A, 0x5B9C_CA4F, 0x682E_6FF3,
    0x748F_82EE, 0x78A5_636F, 0x84C8_7814, 0x8CC7_0208, 0x90BE_FFFA, 0xA450_6CEB, 0xBEF9_A3F7, 0xC671_78F2,
];

/// SHA-256 initial hash value.
const H256: [u32; 8] = [
    0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A, 0x510E_527F, 0x9B05_688C, 0x1F83_D9AB, 0x5BE0_CD19,
];

/// SHA-224 initial hash value (FIPS 180-4 section 5.3.2).
const H224: [u32; 8] = [
    0xC105_9ED8, 0x367C_D507, 0x3070_DD17, 0xF70E_5939, 0xFFC0_0B31, 0x6858_1511, 0x64F9_8FA7, 0xBEFA_4FA4,
];

fn sha256_state(data: &[u8], init: [u32; 8]) -> [u32; 8] {
    let mut h = init;
    for_each_padded_block(data, 64, 8, true, |block| sha256_compress(&mut h, block));
    h
}

fn sha256_compress(h: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (word, c) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(big_s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(v);
    }
}

// ---------------------------------------------------------------------------------------------
// SHA-384 / SHA-512 (FIPS 180-4)
// ---------------------------------------------------------------------------------------------

const K512: [u64; 80] = [
    0x428A_2F98_D728_AE22, 0x7137_4491_23EF_65CD, 0xB5C0_FBCF_EC4D_3B2F, 0xE9B5_DBA5_8189_DBBC,
    0x3956_C25B_F348_B538, 0x59F1_11F1_B605_D019, 0x923F_82A4_AF19_4F9B, 0xAB1C_5ED5_DA6D_8118,
    0xD807_AA98_A303_0242, 0x1283_5B01_4570_6FBE, 0x2431_85BE_4EE4_B28C, 0x550C_7DC3_D5FF_B4E2,
    0x72BE_5D74_F27B_896F, 0x80DE_B1FE_3B16_96B1, 0x9BDC_06A7_25C7_1235, 0xC19B_F174_CF69_2694,
    0xE49B_69C1_9EF1_4AD2, 0xEFBE_4786_384F_25E3, 0x0FC1_9DC6_8B8C_D5B5, 0x240C_A1CC_77AC_9C65,
    0x2DE9_2C6F_592B_0275, 0x4A74_84AA_6EA6_E483, 0x5CB0_A9DC_BD41_FBD4, 0x76F9_88DA_8311_53B5,
    0x983E_5152_EE66_DFAB, 0xA831_C66D_2DB4_3210, 0xB003_27C8_98FB_213F, 0xBF59_7FC7_BEEF_0EE4,
    0xC6E0_0BF3_3DA8_8FC2, 0xD5A7_9147_930A_A725, 0x06CA_6351_E003_826F, 0x1429_2967_0A0E_6E70,
    0x27B7_0A85_46D2_2FFC, 0x2E1B_2138_5C26_C926, 0x4D2C_6DFC_5AC4_2AED, 0x5338_0D13_9D95_B3DF,
    0x650A_7354_8BAF_63DE, 0x766A_0ABB_3C77_B2A8, 0x81C2_C92E_47ED_AEE6, 0x9272_2C85_1482_353B,
    0xA2BF_E8A1_4CF1_0364, 0xA81A_664B_BC42_3001, 0xC24B_8B70_D0F8_9791, 0xC76C_51A3_0654_BE30,
    0xD192_E819_D6EF_5218, 0xD699_0624_5565_A910, 0xF40E_3585_5771_202A, 0x106A_A070_32BB_D1B8,
    0x19A4_C116_B8D2_D0C8, 0x1E37_6C08_5141_AB53, 0x2748_774C_DF8E_EB99, 0x34B0_BCB5_E19B_48A8,
    0x391C_0CB3_C5C9_5A63, 0x4ED8_AA4A_E341_8ACB, 0x5B9C_CA4F_7763_E373, 0x682E_6FF3_D6B2_B8A3,
    0x748F_82EE_5DEF_B2FC, 0x78A5_636F_4317_2F60, 0x84C8_7814_A1F0_AB72, 0x8CC7_0208_1A64_39EC,
    0x90BE_FFFA_2363_1E28, 0xA450_6CEB_DE82_BDE9, 0xBEF9_A3F7_B2C6_7915, 0xC671_78F2_E372_532B,
    0xCA27_3ECE_EA26_619C, 0xD186_B8C7_21C0_C207, 0xEADA_7DD6_CDE0_EB1E, 0xF57D_4F7F_EE6E_D178,
    0x06F0_67AA_7217_6FBA, 0x0A63_7DC5_A2C8_98A6, 0x113F_9804_BEF9_0DAE, 0x1B71_0B35_131C_471B,
    0x28DB_77F5_2304_7D84, 0x32CA_AB7B_40C7_2493, 0x3C9E_BE0A_15C9_BEBC, 0x431D_67C4_9C10_0D4C,
    0x4CC5_D4BE_CB3E_42B6, 0x597F_299C_FC65_7E2A, 0x5FCB_6FAB_3AD6_FAEC, 0x6C44_198C_4A47_5817,
];

/// SHA-512 initial hash value.
const H512: [u64; 8] = [
    0x6A09_E667_F3BC_C908, 0xBB67_AE85_84CA_A73B, 0x3C6E_F372_FE94_F82B, 0xA54F_F53A_5F1D_36F1,
    0x510E_527F_ADE6_82D1, 0x9B05_688C_2B3E_6C1F, 0x1F83_D9AB_FB41_BD6B, 0x5BE0_CD19_137E_2179,
];

/// SHA-384 initial hash value.
const H384: [u64; 8] = [
    0xCBBB_9D5D_C105_9ED8, 0x629A_292A_367C_D507, 0x9159_015A_3070_DD17, 0x152F_ECD8_F70E_5939,
    0x6733_2667_FFC0_0B31, 0x8EB4_4A87_6858_1511, 0xDB0C_2E0D_64F9_8FA7, 0x47B5_481D_BEFA_4FA4,
];

fn sha512_state(data: &[u8], init: [u64; 8]) -> [u64; 8] {
    let mut h = init;
    for_each_padded_block(data, 128, 16, true, |block| sha512_compress(&mut h, block));
    h
}

fn sha512_compress(h: &mut [u64; 8], block: &[u8]) {
    let mut w = [0u64; 80];
    for (word, c) in w.iter_mut().zip(block.chunks_exact(8)) {
        *word = u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..80 {
        let big_s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(big_s1).wrapping_add(ch).wrapping_add(K512[i]).wrapping_add(w[i]);
        let big_s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(v);
    }
}

// ---------------------------------------------------------------------------------------------
// CRC-32 (IEEE) and CRC-32C (Castagnoli), reflected, table driven
// ---------------------------------------------------------------------------------------------

const fn crc_table(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { poly ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CRC32_IEEE_TABLE: [u32; 256] = crc_table(0xEDB8_8320);
const CRC32C_TABLE: [u32; 256] = crc_table(0x82F6_3B78);

fn crc_with_table(table: &[u32; 256], data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn crc32_ieee(data: &[u8]) -> u32 {
    crc_with_table(&CRC32_IEEE_TABLE, data)
}

fn crc32c(data: &[u8]) -> u32 {
    crc_with_table(&CRC32C_TABLE, data)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_data::builtin_proc;
    use crate::context::{IoConfig, TestSession};
    use crate::datum::TypeId;
    use crate::registry::builtin_env;
    use pgarbor_common::Oid;

    /// Deterministic message of length `n`: byte i = (7i + 3) mod 256.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect()
    }

    fn hex(b: &[u8]) -> String {
        hex_lower(b)
    }

    /// Run a registry-resolved scalar implementation directly (strictness is the dispatcher's job).
    fn call(oid: Oid, args: &[Datum]) -> Result<Datum> {
        let proc = builtin_proc(oid).expect("proc exists in catalog_data");
        let f = match lookup(proc) {
            Some(FuncImpl::Scalar(f)) => f,
            other => panic!("expected scalar crypto implementation, got {other:?}"),
        };
        let arg_types: Vec<TypeId> = proc.arg_types.iter().map(|&o| TypeId::of(o)).collect();
        let io = IoConfig::default();
        let session = TestSession::default();
        let ctx = FuncCtx {
            env: builtin_env(),
            io: &io,
            session: &session,
            arg_types: &arg_types,
            result_type: TypeId::of(proc.ret),
            collation: 0,
        };
        f(args, &ctx)
    }

    #[test]
    fn md5_rfc1321_vectors() {
        assert_eq!(hex(&md5(b"").unwrap()), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"a").unwrap()), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(&md5(b"abc").unwrap()), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"message digest").unwrap()),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog").unwrap()),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn sha2_fips_vectors() {
        assert_eq!(
            hex(&sha256(b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"").unwrap()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha224(b"").unwrap()),
            "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"
        );
        assert_eq!(
            hex(&sha224(b"abc").unwrap()),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
        assert_eq!(
            hex(&sha384(b"").unwrap()),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"
        );
        assert_eq!(
            hex(&sha512(b"abc").unwrap()),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            hex(&sha512(b"").unwrap()),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    /// Reference digests for lengths around every padding boundary (0, 55/56, 63/64, 111/112, 127/128).
    /// Columns: length, md5, sha224, sha256, sha384, sha512, crc32 (IEEE), crc32c.
    #[test]
    fn padding_boundary_vectors() {
        let rows: &[(usize, &str, &str, &str, &str, &str, u32, u32)] = &[
            (0, "d41d8cd98f00b204e9800998ecf8427e", "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f",
             "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
             "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b",
             "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
             0, 0),
            (3, "c9aee4810523ef8658121b8d492c6b41", "956a7e9eeefc4f6175e8cd337cd28a80f6a0a988b3fe0ed647b4a953",
             "6ab0dba1f4f1dfbb37b4f9eeb092c09fca4900ad32bdcd147d8dde35d6c87c35",
             "de5060203c58b419f136c17666d339a05705575827e3600f0475f6fb3089870aede26c5a0903dea5938bcded4ee9e619",
             "8853167ab14c043919981a6409307ef68b5b726f4b80dcab259daa24b7cabcb8b4bbfd3e2e36ed98465059661f672e6529cdcc673e7eabfa78dd3186745b112a",
             1834528563, 3526284339),
            (55, "52c0e574e1198de5fe3f8f11440dcb1b", "703a7d6e61c83903b5db54ddf6dda2268211b47c90bbdac2ed306d8a",
             "e7313d333c272e639f790978283f9eb392e843d0f29b7016828bb1daa4aac70b",
             "fc2728f452750d08b5dbad1aea53202e7fc19c06894d13e36c433b0444a899277c49f16e0577c03db0f0605578836ef6",
             "14fd424b1fcadee624da946ab03f7e1def7c0d6e00f689594319881a26ff30b875ba4c622ac13100c8cc784c9c2eb23159aecbb4a02e3999062f551193e2b256",
             818496530, 2901753791),
            (56, "46c9907fc908ee68b1e7b8e71286a518", "850fef35478d7a94a417713dbded4a39c18be2b40e6f20f47066d306",
             "4324d65f3c103567f5589c710bc08f8523f929a9272e3af36fc968e52abc6c27",
             "6c969a865f38d2eaac60b7963a84c97ba27bcb76e589cbe1e2e89ead6aee4ad0337a4268f054be49c604ddd99692036d",
             "480fa85be41ef55a41208ca28ffc8743c91cf7d24758defe6f95bfb16de614fc86b701034896b047dd571de4318853d80e0809df162f1752cb26da6ddb94a0dd",
             3411939512, 4223354084),
            (63, "a62f6d59e837867693f042f5b8f5a236", "246a939bfdd1eedb1ad51b53a250d197b5752aab09580b7f1df50df7",
             "81c80242132f230c3bd41b3e63bbcff16107339549214a99614ff26664625055",
             "be67a9a1162c1d97f183980dc811f4f4f835b8b850eb32ecbf87397b26369bc6646a8f2234940dcff142b627c8e21ebb",
             "ecd42a703a4e93e163d60d55e3785b1a763838b0351bc2e6f7c94b4bfb24f9aa15da5d744ebcebe11f0fc4315d45ba3a047b6e60e07448357f2795bf34b73502",
             3073707050, 1084851031),
            (64, "7160b8fb5e9e4023d549c3971fbaeead", "e480c1c21ffd3f109fc0cde0daf967c748932b64f8e259d98db17420",
             "39e3d7b6b5d075d37d053ad89b24b41bef4f3c29760c84447cab3f3be1882241",
             "99d26430f561864c2f7d90b68650b960c86c85d6bf7f54ceeee404bdd59663a7f05588b5c750b862a2a24f6a9ad4c050",
             "8f3cc30b3fb5bf963688a46488249248ac2c67f0f85a145233c6c1e3c16dcd1df634c07d1d31da02576f65b9cf64e1c3fdb318b689b8a14e2e9552bcf30fb133",
             3420056816, 679803379),
            (111, "13f00cb15f0c3fe588d1c17cde14b38f", "5d5ba2469a7db3e3d5b622bdefbb8fef5582222b0f3dd41db4b62075",
             "67d9492e628fd376e0b2efec8ca2b99b123e202cf620deb270728df979b2f73e",
             "341388a9dc2275074e90cf394323761919761c805fd9e370977c9966a0e8c81a52135f02577670b0071638a4a26dbc31",
             "68cffa6d0d76f309c9ce0d35280939f8e25990c43b7b086ccdf709be35b07d4ddba599541ff2b1c19d34ea49aeafb9659adb7ac3c0b078bb30a22d57fc6687ef",
             1379087543, 4139396665),
            (112, "5fe6a9f76b541ec645559589da9a088a", "df564afcb13bf4a0b08d0d3132418ad63f523107e6003f3506167a5b",
             "96b928cff8528dbb99602c709a65b846cb6467acb8b722f0d758e4dc27bfc508",
             "619cc5d06138526d70659eccf602d197e63e1050e22039a7feb40a30a5b2b08fb03729e291df12f8c576e6f1cd8af22a",
             "d0865c524d1dddf7c23b799c413f5adcd7caefd3f66a9b49750ec81066012c25a8bcf94ddea6dc525691673097ca40e0101e897fc97218cfdb0704084e2bef4b",
             2397288125, 1469691090),
            (127, "acce2474d6cc8302120d09c818d17ef7", "f2d23206c9419be75e54c12e79b49903565a9240f09d0bf15c548ed4",
             "a8d23e75d936f303d248888d9b165ee543f4cbafcad3c9dd2a79bd84faa11d07",
             "f8234ab81f5f9641c656680825a9bbb3c2616bcc80b65d5e479a4d96742e1b8742330c9e8256ef5f03caa932487044ca",
             "e0b6a20f1c0c88970a9340152cd5a1c1ecf3d3b8de55102741879438079473540133b812706e5dbec322c8c9523b6fc8c6d16ee626e87ad5fe3d2916afedc369",
             4021710250, 36820307),
            (128, "10b2da1a82f16d99a81a7203fe9f02cb", "6df983e89c8c13cecdfb5f16fc67792ae3c4d062df69124d889a396f",
             "d2742f1f4ac6bb7ca2b239ee18402ba8b3f9f8e652d2a72973c2b9ba11c08cf6",
             "e8480e9c4dd90f88104a79cbaccec48edbd798a142b4f241d726dc252f1502350e824c7d18dadd59d7d716919fb8f9bf",
             "99b16f17aa0b969a5b8f08f367719d516e330ccd2660b6f0688ec031dbc783de50a1cd185a2568dba75070a2403d17d4741d163578515dfd2ff756ddfe4d47b1",
             3177000449, 751947902),
            (200, "4c79b81ac94bad7a875519ce6b964c66", "67e6d447872d6de5f0a53915ac4e88b60f08895e33f8453500d5bd30",
             "2c7e18c942ef065b526a2d4e5546283749cd3ddfb51d8fc71f42717363685f46",
             "8deb83535fa35d2f493c6c3695b1057b19232d2f531a0d398d2b958413855aa1594b6bcca0ddcbd24a981330a7ff1cc1",
             "cca3c0276046ef9f2897bdfc3ec330f77f4959914b1462bd581b232ddb3e9aa98acf5f5a2b21c7f49d2e43721daa61a2b5cee6af6052dfeb766e66ddb0d1719c",
             267479299, 184462485),
        ];
        for &(len, md5_hex, sha224_hex, sha256_hex, sha384_hex, sha512_hex, crc, crcc) in rows {
            let data = pattern(len);
            assert_eq!(hex(&md5(&data).unwrap()), md5_hex, "md5 len {len}");
            assert_eq!(hex(&sha224(&data).unwrap()), sha224_hex, "sha224 len {len}");
            assert_eq!(hex(&sha256(&data).unwrap()), sha256_hex, "sha256 len {len}");
            assert_eq!(hex(&sha384(&data).unwrap()), sha384_hex, "sha384 len {len}");
            assert_eq!(hex(&sha512(&data).unwrap()), sha512_hex, "sha512 len {len}");
            assert_eq!(crc32_ieee(&data), crc, "crc32 len {len}");
            assert_eq!(crc32c(&data), crcc, "crc32c len {len}");
        }
    }

    #[test]
    fn crc_vectors() {
        assert_eq!(crc32_ieee(b"The quick brown fox jumps over the lazy dog"), 1_095_738_169);
        assert_eq!(crc32_ieee(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn lookup_covers_every_crypto_proc() {
        for oid in [2311u32, 2321, 3419, 3420, 3421, 3422, 6364, 6365] {
            let proc = builtin_proc(oid).expect("catalog row");
            assert!(matches!(lookup(proc), Some(FuncImpl::Scalar(_))), "oid {oid} ({})", proc.name);
        }
        // Unknown prosrc (not a crypto symbol) resolves to nothing.
        let other = builtin_proc(2311).expect("catalog row");
        let mut unknown = *other;
        unknown.prosrc = "md5_nonexistent";
        assert!(lookup(&unknown).is_none());
    }

    #[test]
    fn sql_md5_text() {
        assert_eq!(
            call(2311, &[Datum::Text("abc".to_string())]).unwrap(),
            Datum::Text("900150983cd24fb0d6963f7d28e17f72".to_string())
        );
        assert_eq!(
            call(2311, &[Datum::Text(String::new())]).unwrap(),
            Datum::Text("d41d8cd98f00b204e9800998ecf8427e".to_string())
        );
    }

    #[test]
    fn sql_md5_bytea_and_sha() {
        assert_eq!(
            call(2321, &[Datum::Bytea(b"abc".to_vec())]).unwrap(),
            Datum::Text("900150983cd24fb0d6963f7d28e17f72".to_string())
        );
        match call(3420, &[Datum::Bytea(b"abc".to_vec())]).unwrap() {
            Datum::Bytea(b) => assert_eq!(
                hex(&b),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            ),
            other => panic!("sha256 returned {other:?}"),
        }
        match call(3419, &[Datum::Bytea(Vec::new())]).unwrap() {
            Datum::Bytea(b) => assert_eq!(b.len(), 28),
            other => panic!("sha224 returned {other:?}"),
        }
        match call(3422, &[Datum::Bytea(b"abc".to_vec())]).unwrap() {
            Datum::Bytea(b) => assert_eq!(b.len(), 64),
            other => panic!("sha512 returned {other:?}"),
        }
    }

    #[test]
    fn sql_crc_returns_unsigned_int8() {
        let fox = Datum::Bytea(b"The quick brown fox jumps over the lazy dog".to_vec());
        assert_eq!(call(6364, std::slice::from_ref(&fox)).unwrap(), Datum::Int8(1_095_738_169));
        assert_eq!(call(6365, &[Datum::Bytea(b"123456789".to_vec())]).unwrap(), Datum::Int8(0xE306_9283));
    }

    #[test]
    fn sql_wrong_variant_is_an_error_not_a_panic() {
        assert!(call(2311, &[Datum::Int4(1)]).is_err());
        assert!(call(3420, &[Datum::Text("abc".to_string())]).is_err());
        assert!(call(6364, &[]).is_err());
    }
}
