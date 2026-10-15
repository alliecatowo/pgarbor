//! Schema diff between two catalog snapshots (branch compare / merge preview). Lease CAT-27.
//! This is a PGArbor feature, not a PG one: `pgarbor branch diff a b` and the embedded
//! `Database::compare_branches` call it with the snapshots of two branch heads.
//!
//! Matching is by NAME, not OID (OIDs are per-branch counters): objects are identified by
//! `(kind, schema name, object name[, parent])`:
//! schemas by name; relations/sequences/indexes by `(schema, name)`; columns by `(relation, name)`;
//! constraints by `(table, name)`; types by `(schema, name)`; functions by `(schema, name,
//! IN-arg types as type NAMES)`; roles by name.
//!
//! The result is a flat, deterministic list ordered by (kind rank, qualified name) where kind rank
//! follows creation order (schema, type, sequence, table, column, index, constraint, view, function,
//! role) so a consumer can print or replay it:
//! - `Added` / `Removed`: the object exists only in `b` / only in `a`;
//! - `Modified`: same identity in both with differences; `details` lists them in a stable order, e.g.
//!   `type integer -> bigint`, `NOT NULL added`, `default (none) -> 0`, `owner postgres -> app`,
//!   `renamed`-style changes are NOT detected (a rename is Removed + Added).
//! Comparison inputs per kind: Relation (kind, persistence, owner, options, view query text,
//! columns via the column rules, `acl`), Column (type + typmod via `format_type`, not-null, default
//! text, identity/generated kind, collation name), Constraint (kind, columns by NAME, check text,
//! fk target by name and actions, deferrability), Index (method, uniqueness, key texts, predicate,
//! options), Sequence (all parameters), Type (enum labels in order / domain base+checks), Function
//! (everything but the OID), Role (attributes except password hash: only whether a password is set),
//! Schema (owner). Descriptions (comments) are compared as `comment` details.
use crate::model::*;
use crate::snapshot::CatalogSnapshot;
use pgarbor_common::{PgError, Result};

/// Kind of difference.
// CONTRACT
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// One difference.
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaChange {
    pub kind: ChangeKind,
    /// `schema`, `table`, `view`, `materialized view`, `sequence`, `index`, `column`, `constraint`,
    /// `type`, `domain`, `function`, `procedure`, `role` (words as `describe::type_description`).
    pub object_type: &'static str,
    /// Qualified identity: `public.t`, `public.t.col`, `public.t.t_pkey`, `public.f(integer)`.
    pub name: String,
    /// Human-readable details (module docs); empty for Added/Removed.
    pub details: Vec<String>,
    /// The address in snapshot `b` (Added/Modified) or `a` (Removed).
    pub address: ObjectAddress,
}

/// Differences turning snapshot `a` into snapshot `b` (module docs). Builtin objects are ignored.
// CONTRACT
#[allow(unused_variables)]
pub fn diff_schemas(a: &CatalogSnapshot, b: &CatalogSnapshot) -> Result<Vec<SchemaChange>> {
    Err(PgError::unimplemented("CAT-27", "schema diff"))
}

/// One line per change (`+ table public.t`, `- column public.t.x`, `~ column public.t.y: type integer
/// -> bigint`) joined with `\n`; empty string when identical.
pub fn render_diff(changes: &[SchemaChange]) -> String {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        let sigil = match c.kind {
            ChangeKind::Added => '+',
            ChangeKind::Removed => '-',
            ChangeKind::Modified => '~',
        };
        let mut line = format!("{sigil} {} {}", c.object_type, c.name);
        if !c.details.is_empty() {
            line.push_str(": ");
            line.push_str(&c.details.join("; "));
        }
        out.push(line);
    }
    out.join("\n")
}
