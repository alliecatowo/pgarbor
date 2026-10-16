//! Function overload resolution (TYPE-12). Lease TYP-55 (opus).
//!
//! PG18 reference: `src/backend/parser/parse_func.c` — `func_get_detail`, `func_match_argtypes`,
//! `func_select_candidate` (the 6-step heuristic), `make_fn_arguments`; `catalog/namespace.c`
//! `FuncnameGetCandidates` (search_path precedence, named/default/variadic expansion).
//! Docs: "10.3 Functions" (type conversion rules).
//!
//! Algorithm summary (`resolve_function`), port faithfully:
//! 1. Candidates: all procs with the name (any kind) visible in the search path (`FuncCandidate.path_pos`:
//!    lower wins when signatures are identical). Expand VARIADIC candidates (when the call does not use
//!    the VARIADIC keyword) into fixed-arity forms, and default-argument candidates into shorter forms.
//!    Named-notation calls (`f(a => 1)`) reorder arguments by `arg_names` and may fill defaults.
//! 2. Exact match on argument types (identical OIDs; an unknown arg never matches exactly) → done.
//! 3. Function-style casts (`int4(1.5)` → 2, `text(1)`): when step 2 fails, the call has one argument and
//!    the name is a type name, PG treats it as a cast. That is the BINDER's job (it checks
//!    `names::resolve_type_name` + `cast::find_cast` before/after calling `resolve_function`).
//! 4. `func_match_argtypes`: keep candidates whose args the inputs can be implicitly coerced to
//!    (`coerce::can_coerce_type(..., Implicit)`, polymorphic consistency via `polymorphic`).
//!    One → done. Zero → 42883 `function foo(integer) does not exist` + HINT
//!    `No function matches the given name and argument types. You might need to add explicit type casts.`
//! 5. `func_select_candidate`: (a) most exact matches (input == candidate arg, after domain→base);
//!    (b) most args where the candidate takes a PREFERRED type at a non-exact position; (c) for unknown
//!    inputs: per argument position collect the categories the candidates accept; if any candidate accepts
//!    'S' (string) choose 'S', else if all agree on one category choose it (prefer the preferred type of it),
//!    else fail; drop candidates not matching; (d) if exactly one remains → done; (e) the "all unknown
//!    inputs assumed same type as known inputs" rule (PG 10.3 step 4.f / `func_select_candidate` final
//!    stage): if some inputs are unknown and all known inputs share one type, assume unknowns are that type
//!    and look for a candidate accepting it everywhere.
//!    Ambiguity → 42725 `function foo(unknown) is not unique` + HINT
//!    `Could not choose a best candidate function. You might need to add explicit type casts.`
//! 6. Result: declared arg types (after polymorphic resolution), return type (polymorphic return resolved by
//!    `polymorphic::resolve_polymorphic_return`), set-returning flag, how many variadic args were folded,
//!    defaults to append.
//! Oracle fixtures: `abs('1')` and `round('2.5')` → the float8 overloads (category N, preferred type
//! float8); `upper('x')`, `'a' || 'b'` → text; `length(1)` → 42883 `function length(integer) does not exist`; `substr(1, 1)` → 42883 `function substr(integer, integer) does not exist`;
//! `array_cat(array[1], array['a'])` → 42883 `function array_cat(integer[], text[]) does not exist`;
//! `foo(1)` → 42883 `function foo(integer) does not exist`.
//! Signature strings use `names::format_type` for each arg (`unknown` for unknown literals).
use crate::catalog_data::{self, CastContext, ProcKind};
use crate::context::TypeEnv;
use crate::datum::TypeId;
use crate::names;
use crate::oids;
use crate::registry;
use crate::resolve::{coerce, polymorphic};
use pgarbor_common::{Oid, PgError, Result, SqlState};

/// PG `FUNC_MAX_ARGS`.
const FUNC_MAX_ARGS: usize = 100;
/// Marker in `ResolvedFunc::arg_order` for a declared parameter filled from its default.
const DEFAULT_SLOT: usize = usize::MAX;

/// A candidate function (builtin from `catalog_data::PROCS` or user-defined from the catalog).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuncCandidate {
    pub oid: Oid,
    pub name: String,
    /// Declared IN argument types (`proargtypes`).
    pub arg_types: Vec<Oid>,
    /// Parameter names parallel to `arg_types` (empty strings when unnamed).
    pub arg_names: Vec<String>,
    /// Element type of the VARIADIC parameter (last arg is its array type; 2276 for VARIADIC "any"), or 0.
    pub variadic: Oid,
    /// Number of trailing args with defaults.
    pub ndefaults: u16,
    pub ret: Oid,
    pub retset: bool,
    pub kind: ProcKind,
    /// Position of the candidate's namespace in the effective search path (pg_catalog first = 0).
    pub path_pos: u32,
}

/// The call being resolved.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuncCall {
    pub name: String,
    /// Actual argument types (705 = unknown, which is also the type of an untyped NULL literal).
    pub arg_types: Vec<Oid>,
    /// Named-notation names parallel to `arg_types` (`None` for positional args).
    pub arg_names: Vec<Option<String>>,
    /// The call used `VARIADIC` on its last argument.
    pub variadic_keyword: bool,
}

/// Successful resolution.
///
/// Layout notes (the contract text is loose, this is what `resolve_function` produces):
/// `coerce_to[j]` is the type call argument `j` must be coerced to (call order; for a named call the
/// call's own order, which is also how PG orders `true_typeids`). `arg_order[p]` is, for declared
/// parameter `p` (0-based, variadic expansion not counted), the index of the call argument that
/// supplies it, or `usize::MAX` when the parameter is filled from its default. For positional calls
/// `arg_order` is the identity over the supplied arguments and omitted defaults are the trailing
/// `ndefaults_used` parameters; for named calls `arg_order` has one entry per declared parameter.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFunc {
    pub oid: Oid,
    pub kind: ProcKind,
    /// Types each SUPPLIED argument must be coerced to (in the call's argument order after named-arg
    /// reordering; polymorphic types replaced by actual types).
    pub coerce_to: Vec<Oid>,
    /// Permutation: `arg_order[i]` = index into the call's args of the i-th declared parameter
    /// (identity for positional calls).
    pub arg_order: Vec<usize>,
    /// Number of trailing call args collected into the VARIADIC array (0 if none / VARIADIC "any").
    pub nvariadic: usize,
    /// Number of defaults the executor must append (from `BuiltinProc::arg_defaults`).
    pub ndefaults_used: usize,
    /// Resolved result type.
    pub ret: TypeId,
    pub retset: bool,
}

/// All builtin candidates with this name (from `catalog_data::PROCS`; `path_pos` 0).
// CONTRACT
pub fn builtin_candidates(name: &str) -> Result<Vec<FuncCandidate>> {
    let mut out = Vec::new();
    for p in catalog_data::procs_named(name) {
        let n = p.arg_types.len();
        let mut names: Vec<String> = Vec::with_capacity(n);
        if !p.arg_modes.is_empty() {
            // arg_names are parallel to all_arg_types; keep the IN-ish ones (i, b, v).
            for (k, m) in p.arg_modes.iter().enumerate() {
                if matches!(*m, b'i' | b'b' | b'v') {
                    names.push(p.arg_names.get(k).map(|s| s.to_string()).unwrap_or_default());
                }
            }
        } else {
            for k in 0..p.arg_names.len().min(n) {
                names.push(p.arg_names[k].to_string());
            }
        }
        names.resize(n, String::new());
        out.push(FuncCandidate {
            oid: p.oid,
            name: p.name.to_string(),
            arg_types: p.arg_types.to_vec(),
            arg_names: names,
            variadic: p.variadic,
            ndefaults: p.ndefaults,
            ret: p.ret,
            retset: p.retset,
            kind: p.kind,
            path_pos: 0,
        });
    }
    Ok(out)
}

/// A candidate laid out against a particular call (PG `FuncCandidateList` entry after
/// `FuncnameGetCandidates` expansion).
struct Eff {
    cand: usize,
    /// Declared types per call argument (call order, length = number of call args).
    args: Vec<Oid>,
    /// Declared parameter -> call arg index (or `DEFAULT_SLOT`).
    order: Vec<usize>,
    nvariadic: usize,
    ndef: usize,
}

fn call_arg_name(call: &FuncCall, i: usize) -> Option<&str> {
    call.arg_names.get(i).and_then(|n| n.as_deref())
}

/// Lay out `c` against the call, or None when the candidate cannot take this argument list at all.
fn layout_candidate(idx: usize, c: &FuncCandidate, call: &FuncCall) -> Option<Eff> {
    let nargs = call.arg_types.len();
    let pronargs = c.arg_types.len();
    let ndefaults = (c.ndefaults as usize).min(pronargs);
    let named = (0..nargs).any(|i| call_arg_name(call, i).is_some());
    if !named {
        if c.variadic != 0 && !call.variadic_keyword && pronargs >= 1 && nargs >= pronargs {
            let mut args = Vec::with_capacity(nargs);
            args.extend_from_slice(&c.arg_types[..pronargs - 1]);
            let nvar = nargs - (pronargs - 1);
            for _ in 0..nvar {
                args.push(c.variadic);
            }
            let nvariadic = if c.variadic == oids::ANY { 0 } else { nvar };
            return Some(Eff { cand: idx, args, order: (0..nargs).collect(), nvariadic, ndef: 0 });
        }
        if pronargs == nargs {
            return Some(Eff { cand: idx, args: c.arg_types.clone(), order: (0..nargs).collect(), nvariadic: 0, ndef: 0 });
        }
        if pronargs > nargs && pronargs - ndefaults <= nargs {
            return Some(Eff {
                cand: idx,
                args: c.arg_types[..nargs].to_vec(),
                order: (0..nargs).collect(),
                nvariadic: 0,
                ndef: pronargs - nargs,
            });
        }
        return None;
    }
    // Named / mixed notation: positional args first, then named ones.
    let npos = (0..nargs).take_while(|i| call_arg_name(call, *i).is_none()).count();
    if (npos..nargs).any(|i| call_arg_name(call, i).is_none()) || nargs > pronargs {
        return None;
    }
    let mut order = vec![DEFAULT_SLOT; pronargs];
    let mut args = Vec::with_capacity(nargs);
    for j in 0..npos {
        order[j] = j;
        args.push(c.arg_types[j]);
    }
    for j in npos..nargs {
        let want = call_arg_name(call, j)?;
        let p = c.arg_names.iter().position(|n| !n.is_empty() && n == want)?;
        if p < npos || order[p] != DEFAULT_SLOT {
            return None;
        }
        order[p] = j;
        args.push(c.arg_types[p]);
    }
    let first_default = pronargs - ndefaults;
    let mut ndef = 0;
    for (p, o) in order.iter().enumerate() {
        if *o == DEFAULT_SLOT {
            if p < first_default {
                return None;
            }
            ndef += 1;
        }
    }
    Some(Eff { cand: idx, args, order, nvariadic: 0, ndef })
}

fn call_signature(env: &dyn TypeEnv, call: &FuncCall) -> String {
    let mut s = String::new();
    s.push_str(&call.name);
    s.push('(');
    for (i, t) in call.arg_types.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        if let Some(n) = call_arg_name(call, i) {
            s.push_str(n);
            s.push_str(" => ");
        }
        s.push_str(&names::display_name(env, *t));
    }
    s.push(')');
    s
}

fn not_found_error(env: &dyn TypeEnv, call: &FuncCall) -> PgError {
    PgError::new(SqlState::UNDEFINED_FUNCTION, format!("function {} does not exist", call_signature(env, call)))
        .with_hint("No function matches the given name and argument types. You might need to add explicit type casts.")
}

fn ambiguous_error(env: &dyn TypeEnv, call: &FuncCall) -> PgError {
    PgError::new(SqlState::AMBIGUOUS_FUNCTION, format!("function {} is not unique", call_signature(env, call)))
        .with_hint("Could not choose a best candidate function. You might need to add explicit type casts.")
}

fn too_many_args() -> PgError {
    PgError::new(SqlState(*b"54023"), format!("cannot pass more than {FUNC_MAX_ARGS} arguments to a function"))
}

/// Resolve `call` among `candidates` (see module docs for algorithm and errors).
// CONTRACT
pub fn resolve_function(env: &dyn TypeEnv, call: &FuncCall, candidates: &[FuncCandidate]) -> Result<ResolvedFunc> {
    let nargs = call.arg_types.len();
    if nargs > FUNC_MAX_ARGS {
        return Err(too_many_args());
    }
    // 1. Candidate expansion.
    let mut effs: Vec<Eff> = candidates.iter().enumerate().filter_map(|(i, c)| layout_candidate(i, c, call)).collect();
    // Identical effective signatures: the one earlier in the search path hides the others.
    let hidden: Vec<bool> = (0..effs.len())
        .map(|i| {
            effs.iter().enumerate().any(|(j, o)| {
                j != i && o.args == effs[i].args && candidates[o.cand].path_pos < candidates[effs[i].cand].path_pos
            })
        })
        .collect();
    let mut k = 0;
    effs.retain(|_| {
        let keep = !hidden[k];
        k += 1;
        keep
    });
    if effs.is_empty() {
        return Err(not_found_error(env, call));
    }

    // 2. Exact match on argument types.
    let exact: Vec<usize> = (0..effs.len()).filter(|i| effs[*i].args == call.arg_types).collect();
    let best = if exact.len() == 1 {
        exact[0]
    } else if exact.len() > 1 {
        return Err(ambiguous_error(env, call));
    } else {
        // 4. func_match_argtypes
        let mut ok: Vec<usize> = Vec::new();
        for (i, e) in effs.iter().enumerate() {
            if coerce::can_coerce_type(env, &call.arg_types, &e.args, CastContext::Implicit)? {
                ok.push(i);
            }
        }
        match ok.len() {
            0 => return Err(not_found_error(env, call)),
            1 => ok[0],
            _ => {
                // 5. func_select_candidate
                let lists: Vec<Vec<Oid>> = ok.iter().map(|i| effs[*i].args.clone()).collect();
                let sel = select_candidate(env, &call.arg_types, &lists)?;
                if sel.len() == 1 {
                    ok[sel[0]]
                } else {
                    return Err(ambiguous_error(env, call));
                }
            }
        }
    };

    // 6. Result.
    let e = &effs[best];
    let c = &candidates[e.cand];
    if call.variadic_keyword && c.variadic == oids::ANY && nargs > 0 {
        let last = registry::base_type(env, call.arg_types[nargs - 1]);
        if registry::element_type_of(env, last).is_none() {
            return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "VARIADIC argument must be an array"));
        }
    }
    let needs_poly = registry::is_polymorphic(c.ret) || e.args.iter().any(|t| registry::is_polymorphic(*t));
    let (mut coerce_to, ret) = if needs_poly {
        polymorphic::enforce_generic_type_consistency(env, &call.arg_types, &e.args, c.ret)?
    } else {
        (e.args.clone(), c.ret)
    };
    // "any" parameters take the argument as is, unknown literals included (PG `coerce_type` returns
    // the node unchanged for `any`, so `pg_typeof('x')` reports `unknown`).
    for (j, t) in coerce_to.iter_mut().enumerate() {
        if *t == oids::ANY {
            *t = call.arg_types[j];
        }
    }
    Ok(ResolvedFunc {
        oid: c.oid,
        kind: c.kind,
        coerce_to,
        arg_order: e.order.clone(),
        nvariadic: e.nvariadic,
        ndefaults_used: e.ndef,
        ret: TypeId::of(ret),
        retset: c.retset,
    })
}

/// Keep only the candidates with the highest score; if every score is equal all are kept (PG's "keep
/// all if none match" falls out of seeding with the first candidate).
fn keep_best<F: Fn(&[Oid]) -> usize>(idx: &[usize], cands: &[Vec<Oid>], score: F) -> Vec<usize> {
    let mut best = 0usize;
    let mut kept: Vec<usize> = Vec::new();
    for &i in idx {
        let n = score(&cands[i]);
        if kept.is_empty() || n > best {
            best = n;
            kept.clear();
            kept.push(i);
        } else if n == best {
            kept.push(i);
        }
    }
    kept
}

/// PG `IsPreferredType(category, type)` (category 0 = TYPCATEGORY_INVALID matches any category).
fn is_preferred_in(env: &dyn TypeEnv, category: u8, t: Oid) -> bool {
    match env.type_info(t) {
        Some(ti) => (category == 0 || category == ti.category) && ti.preferred,
        None => false,
    }
}

fn category_preferred(env: &dyn TypeEnv, t: Oid) -> (u8, bool) {
    match env.type_info(t) {
        Some(ti) => (ti.category, ti.preferred),
        None => (b'X', false),
    }
}

/// PG `func_select_candidate` over already-coercible candidates (exposed for operator resolution).
/// Returns indexes of surviving candidates (exactly one on success; on failure the set that was left
/// when the heuristics ran out, which has more than one member).
// CONTRACT
pub fn select_candidate(env: &dyn TypeEnv, input_types: &[Oid], candidate_arg_types: &[Vec<Oid>]) -> Result<Vec<usize>> {
    let nargs = input_types.len();
    if nargs > FUNC_MAX_ARGS {
        return Err(too_many_args());
    }
    let at = |c: &[Oid], i: usize| c.get(i).copied().unwrap_or(0);
    // Domains are reduced to their base types.
    let mut base: Vec<Oid> = input_types
        .iter()
        .map(|t| if *t == oids::UNKNOWN { oids::UNKNOWN } else { registry::base_type(env, *t) })
        .collect();
    let nunknowns = base.iter().filter(|t| **t == oids::UNKNOWN).count();
    let mut cur: Vec<usize> = (0..candidate_arg_types.len()).collect();
    if cur.len() <= 1 {
        return Ok(cur);
    }

    // (a) most exact matches.
    cur = keep_best(&cur, candidate_arg_types, |c| {
        (0..nargs).filter(|i| base[*i] != oids::UNKNOWN && at(c, *i) == base[*i]).count()
    });
    if cur.len() == 1 {
        return Ok(cur);
    }

    // (b) exact matches or preferred types of the input's category.
    let cats: Vec<u8> = base.iter().map(|t| registry::type_category(env, *t)).collect();
    cur = keep_best(&cur, candidate_arg_types, |c| {
        (0..nargs)
            .filter(|i| base[*i] != oids::UNKNOWN && (at(c, *i) == base[*i] || is_preferred_in(env, cats[*i], at(c, *i))))
            .count()
    });
    if cur.len() == 1 {
        return Ok(cur);
    }

    if nunknowns == 0 {
        return Ok(cur);
    }

    // (c) assign categories to unknown inputs.
    let mut slot_category = vec![0u8; nargs];
    let mut slot_pref = vec![false; nargs];
    let mut resolved = true;
    for i in 0..nargs {
        if base[i] != oids::UNKNOWN {
            continue;
        }
        let mut have_conflict = false;
        for &ci in &cur {
            let (cat, pref) = category_preferred(env, at(&candidate_arg_types[ci], i));
            if slot_category[i] == 0 {
                slot_category[i] = cat;
                slot_pref[i] = pref;
            } else if cat == slot_category[i] {
                slot_pref[i] |= pref;
            } else if cat == b'S' {
                slot_category[i] = cat;
                slot_pref[i] = pref;
            } else {
                have_conflict = true;
            }
        }
        if have_conflict && slot_category[i] != b'S' {
            resolved = false;
            break;
        }
    }
    if resolved {
        let kept: Vec<usize> = cur
            .iter()
            .copied()
            .filter(|ci| {
                (0..nargs).all(|i| {
                    if base[i] != oids::UNKNOWN {
                        return true;
                    }
                    let (cat, pref) = category_preferred(env, at(&candidate_arg_types[*ci], i));
                    cat == slot_category[i] && !(slot_pref[i] && !pref)
                })
            })
            .collect();
        if !kept.is_empty() {
            cur = kept;
        }
        if cur.len() == 1 {
            return Ok(cur);
        }
    }

    // (e) last gasp: all known inputs share one type; assume the unknowns are that type too.
    if nunknowns < nargs {
        let mut known = oids::UNKNOWN;
        for t in &base {
            if *t == oids::UNKNOWN {
                continue;
            }
            if known == oids::UNKNOWN {
                known = *t;
            } else if known != *t {
                known = oids::UNKNOWN;
                break;
            }
        }
        if known != oids::UNKNOWN {
            for t in base.iter_mut() {
                *t = known;
            }
            let mut found: Vec<usize> = Vec::new();
            for &ci in &cur {
                if coerce::can_coerce_type(env, &base, &candidate_arg_types[ci], CastContext::Implicit)? {
                    found.push(ci);
                    if found.len() > 1 {
                        break;
                    }
                }
            }
            if found.len() == 1 {
                return Ok(found);
            }
        }
    }
    Ok(cur)
}

/// Display signature `foo(integer, unknown)` for error messages.
// CONTRACT
pub fn signature_string(env: &dyn TypeEnv, name: &str, arg_types: &[Oid]) -> Result<String> {
    let mut s = String::with_capacity(name.len() + 2 + arg_types.len() * 8);
    s.push_str(name);
    s.push('(');
    for (i, t) in arg_types.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&names::display_name(env, *t));
    }
    s.push(')');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &[Oid]) -> FuncCall {
        FuncCall { name: name.into(), arg_types: args.to_vec(), arg_names: vec![None; args.len()], variadic_keyword: false }
    }

    fn resolve(name: &str, args: &[Oid]) -> Result<ResolvedFunc> {
        let env = registry::builtin_env();
        let cands = builtin_candidates(name)?;
        resolve_function(env, &call(name, args), &cands)
    }

    #[test]
    fn abs_unknown_is_float8() {
        let r = resolve("abs", &[oids::UNKNOWN]).unwrap();
        assert_eq!(r.ret.oid, 701);
        assert_eq!(r.coerce_to, vec![701]);
    }

    #[test]
    fn round_unknown_is_float8() {
        let r = resolve("round", &[oids::UNKNOWN]).unwrap();
        assert_eq!(r.ret.oid, 701);
    }

    #[test]
    fn upper_unknown_is_text() {
        let r = resolve("upper", &[oids::UNKNOWN]).unwrap();
        assert_eq!(r.ret.oid, oids::TEXT);
    }

    #[test]
    fn length_int4_not_found() {
        let e = resolve("length", &[oids::INT4]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "function length(integer) does not exist");
        assert!(e.hint.as_deref().unwrap().starts_with("No function matches"));
    }

    #[test]
    fn unknown_function() {
        let e = resolve("foo", &[oids::INT4]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "function foo(integer) does not exist");
    }

    #[test]
    fn array_cat_mismatch() {
        let e = resolve("array_cat", &[1007, 1009]).unwrap_err();
        assert_eq!(e.sqlstate, SqlState::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "function array_cat(integer[], text[]) does not exist");
    }

    #[test]
    fn concat_variadic_any() {
        let r = resolve("concat", &[oids::INT4, oids::UNKNOWN]).unwrap();
        assert_eq!(r.ret.oid, oids::TEXT);
        assert_eq!(r.nvariadic, 0);
        assert_eq!(r.coerce_to, vec![oids::INT4, oids::UNKNOWN]);
    }

    #[test]
    fn any_parameter_keeps_unknown_argument() {
        // `pg_typeof('x')` sees the literal as `unknown` (PG: the `any` argument is not coerced).
        let r = resolve("pg_typeof", &[oids::UNKNOWN]).unwrap();
        assert_eq!(r.coerce_to, vec![oids::UNKNOWN]);
    }

    #[test]
    fn make_interval_named_with_defaults() {
        let env = registry::builtin_env();
        let cands = builtin_candidates("make_interval").unwrap();
        let c = FuncCall {
            name: "make_interval".into(),
            arg_types: vec![oids::INT4],
            arg_names: vec![Some("days".into())],
            variadic_keyword: false,
        };
        let r = resolve_function(env, &c, &cands).unwrap();
        assert_eq!(r.ret.oid, 1186);
        assert_eq!(r.ndefaults_used, 6);
        assert_eq!(r.arg_order[3], 0);
        assert_eq!(r.arg_order[0], DEFAULT_SLOT);
    }

    #[test]
    fn named_arg_unknown_name_fails() {
        let env = registry::builtin_env();
        let cands = builtin_candidates("make_interval").unwrap();
        let c = FuncCall {
            name: "make_interval".into(),
            arg_types: vec![oids::INT4],
            arg_names: vec![Some("nope".into())],
            variadic_keyword: false,
        };
        let e = resolve_function(env, &c, &cands).unwrap_err();
        assert_eq!(e.message, "function make_interval(nope => integer) does not exist");
    }

    #[test]
    fn signature_formatting() {
        let env = registry::builtin_env();
        assert_eq!(signature_string(env, "foo", &[oids::INT4, oids::UNKNOWN]).unwrap(), "foo(integer, unknown)");
    }
}
