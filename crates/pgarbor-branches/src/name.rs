//! Branch name and snapshot label validation (ARCHITECTURE §6). CONTRACT. Fully implemented.
//!
//! Rule (both names and labels): 1..=200 bytes from `[A-Za-z0-9._/-]`, first character not `-`
//! or `/`. Names are case-sensitive and compared bytewise (`Main` != `main`). Nothing else is
//! forbidden in v1 (e.g. `a//b` and `x/` are valid); tools that map names to file names must
//! escape `/` themselves.
use crate::error;
use pgarbor_common::Result;

/// Maximum branch name / snapshot label length in bytes.
pub const MAX_NAME_LEN: usize = 200;

/// Human-readable statement of the rule, used as the error detail.
pub const NAME_RULE: &str =
    "Names must be 1 to 200 characters from [A-Za-z0-9._/-] and must not start with \"-\" or \"/\".";

/// `None` if valid, else the reason.
fn check(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Some(NAME_RULE);
    }
    if name.starts_with('-') || name.starts_with('/') {
        return Some(NAME_RULE);
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-')) {
        return Some(NAME_RULE);
    }
    None
}

/// Validate a branch name. Error: 42602 `invalid branch name "<name>"` with [`NAME_RULE`] detail.
///
/// Examples: `main`, `feature/auth`, `agent-42`, `v1.2_rc` are valid; ``, `-x`, `/x`, `a b`,
/// `ünï` and a 201-character name are not.
pub fn validate_branch_name(name: &str) -> Result<()> {
    match check(name) {
        None => Ok(()),
        Some(reason) => Err(error::invalid_branch_name(name, reason)),
    }
}

/// Validate a named-snapshot label (same rule). Error: 42602 `invalid snapshot name "<label>"`.
pub fn validate_snapshot_label(label: &str) -> Result<()> {
    match check(label) {
        None => Ok(()),
        Some(reason) => Err(error::invalid_snapshot_label(label, reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names() {
        for ok in ["main", "feature/auth", "agent-42", "v1.2_rc", "a//b"] {
            assert!(validate_branch_name(ok).is_ok(), "{ok}");
        }
        let long = "a".repeat(201);
        for bad in ["", "-x", "/x", "a b", "ünï", long.as_str()] {
            assert!(validate_branch_name(bad).is_err(), "{bad}");
        }
        assert!(validate_branch_name(&"a".repeat(200)).is_ok());
    }
}
