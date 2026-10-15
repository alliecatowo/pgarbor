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
use crate::catalog_data::ProcKind;
use crate::context::TypeEnv;
use crate::datum::TypeId;
use pgarbor_common::{Oid, PgError, Result};

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
#[allow(unused_variables)]
pub fn builtin_candidates(name: &str) -> Result<Vec<FuncCandidate>> {
    Err(PgError::unimplemented("TYP-55", "builtin function candidates"))
}

/// Resolve `call` among `candidates` (see module docs for algorithm and errors).
// CONTRACT
#[allow(unused_variables)]
pub fn resolve_function(env: &dyn TypeEnv, call: &FuncCall, candidates: &[FuncCandidate]) -> Result<ResolvedFunc> {
    Err(PgError::unimplemented("TYP-55", "function resolution"))
}

/// PG `func_select_candidate` over already-coercible candidates (exposed for operator resolution).
/// Returns indexes of surviving candidates (exactly one on success).
// CONTRACT
#[allow(unused_variables)]
pub fn select_candidate(env: &dyn TypeEnv, input_types: &[Oid], candidate_arg_types: &[Vec<Oid>]) -> Result<Vec<usize>> {
    Err(PgError::unimplemented("TYP-55", "best candidate selection"))
}

/// Display signature `foo(integer, unknown)` for error messages.
// CONTRACT
#[allow(unused_variables)]
pub fn signature_string(env: &dyn TypeEnv, name: &str, arg_types: &[Oid]) -> Result<String> {
    Err(PgError::unimplemented("TYP-55", "function signature formatting"))
}
