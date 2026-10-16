//! Catalog object model (CAT-01, CAT-03, CAT-04, CAT-10). Plain serde data; frozen (v1).
//!
//! | Object | Struct | Persisted kind byte (`persist::keys`) |
//! |---|---|---|
//! | schema | [`Namespace`] | `b'n'` |
//! | table / view / matview / composite type / partitioned table | [`Relation`] | `b'r'` |
//! | index | [`Index`] | `b'i'` |
//! | sequence | [`Sequence`] | `b'S'` |
//! | enum / domain | [`TypeObj`] | `b't'` |
//! | constraint | [`Constraint`] | `b'c'` |
//! | function / procedure | [`Function`] | `b'f'` |
//! | role | [`Role`] | `b'a'` |
//! | role membership | [`RoleMembership`] | `b'm'` |
//! | database | [`Database`] | `b'd'` |
//! | description (COMMENT ON) | `String` | `b'D'` (key = `DescriptionKey`) |
//! | operator / aggregate def / opfamily / opclass / cast / conversion / collation / extension | [`ExtObject`] | `b'x'` |
pub mod common;
pub mod constraint;
pub mod ext;
pub mod function;
pub mod index;
pub mod namespace;
pub mod object;
pub mod relation;
pub mod sequence;
pub mod types;

pub use common::{
    acl_mode, class_oid, Acl, AclItem, DepType, DescriptionKey, DropBehavior, ObjClass, ObjectAddress, StoredExpr,
    ACL_ALL_RIGHTS_STR,
};
pub use ext::{
    AggregateDef, AmOpEntry, AmProcEntry, CastDef, Collation, Conversion, ExtObject, Extension, OpClass, OpFamily,
    Operator,
};
pub use constraint::{Constraint, ConstraintKind, FkAction, FkMatch, ForeignKey};
pub use function::{ArgMode, FuncArg, FuncKind, FuncVolatility, Function};
pub use index::{Index, IndexKey, IndexMethod};
pub use namespace::{Database, Namespace, Role, RoleMembership};
pub use object::{CatalogObject, ObjKind};
pub use relation::{
    Column, ColumnDefault, GeneratedKind, Identity, IdentityKind, PartitionKey, Persistence, Policy, RelKind, Relation,
    ReplicaIdentity, Rule, ViewDef,
};
pub use sequence::Sequence;
pub use types::{EnumLabelDef, TypeDef, TypeObj};
