//! Polymorphic type resolution (TYPE-12). Lease TYP-57 (sonnet).
//!
//! PG18 reference: `src/backend/parser/parse_coerce.c` — `check_generic_type_consistency`,
//! `enforce_generic_type_consistency`, `resolve_polymorphic_argtypes`,
//! `check_valid_polymorphic_signature`; docs "38.2.5 Polymorphic Types".
//!
//! Families: (1) `anyelement`, `anyarray`, `anynonarray`, `anyenum`, `anyrange`, `anymultirange` must all
//! resolve to ONE element type T (anyarray = T[]); (2) `anycompatible`, `anycompatiblearray`,
//! `anycompatiblenonarray`, `anycompatiblerange`, `anycompatiblemultirange` resolve to the common type of
//! their inputs (`coerce::select_common_type`, context = function name), e.g. `array_append(array[1], 1.5)`
//! → numeric[] result `{1,1.5}` (oracle).
//! Rules: unknown inputs at polymorphic positions do not determine T (if nothing determines T for an
//! `anyelement` family, assume `text` when all such inputs are unknown — PG resolves unknown → text);
//! anynonarray rejects arrays; anyenum requires an enum; a domain over an array counts as its base array.
//! During function resolution an inconsistent candidate is simply NOT a candidate (`check_*` returns
//! false), so the user-visible error is the resolver's 42883 (oracle: `array_append(array[1], 'x'::text)`
//! → `function array_append(integer[], text) does not exist`; `array_cat(array[1], 2)` →
//! `function array_cat(integer[], integer) does not exist`). Unknown literals adopt the resolved type:
//! `array_append(array[1], 'x')` → 22P02 `invalid input syntax for type integer: "x"`; all-unknown
//! inputs resolve to text: `pg_typeof(array_append(null, null))` = `text[]`, `array_append(null, 1)` →
//! `integer[]`. `enforce_generic_type_consistency` (called after selection) raises 42804
//! `arguments declared "anyelement" are not all alike` with DETAIL `integer versus text`, or
//! `argument declared anyarray is not an array but type integer`, only for direct callers
//! (e.g. operator/aggregate paths that skip candidate filtering).
use crate::context::TypeEnv;
use pgarbor_common::{Oid, PgError, Result};

/// PG `check_generic_type_consistency`: can `actual` inputs satisfy the polymorphic `declared` args?
// CONTRACT
#[allow(unused_variables)]
pub fn check_generic_type_consistency(env: &dyn TypeEnv, actual: &[Oid], declared: &[Oid]) -> Result<bool> {
    Err(PgError::unimplemented("TYP-57", "polymorphic consistency check"))
}

/// PG `enforce_generic_type_consistency`: returns (declared args with polymorphics replaced by actual
/// types, resolved return type). `ret` may be non-polymorphic (returned unchanged).
// CONTRACT
#[allow(unused_variables)]
pub fn enforce_generic_type_consistency(
    env: &dyn TypeEnv,
    actual: &[Oid],
    declared: &[Oid],
    ret: Oid,
) -> Result<(Vec<Oid>, Oid)> {
    Err(PgError::unimplemented("TYP-57", "polymorphic type resolution"))
}
