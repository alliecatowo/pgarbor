//! Type resolution for expressions (TYPE-03, TYPE-12): implicit coercion rules, common-type
//! selection (UNION/CASE/ARRAY/VALUES/COALESCE), function and operator overload resolution, and
//! polymorphic type resolution. Mirrors PG18 `parser/parse_coerce.c`, `parse_func.c`, `parse_oper.c`.
pub mod coerce;
pub mod func;
pub mod oper;
pub mod polymorphic;
