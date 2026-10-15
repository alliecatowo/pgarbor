//! ParameterStatus reporting (PG 14+ `ReportChangedGUCOptions`). Fully implemented, frozen (v1).
//!
//! PG does NOT send ParameterStatus right after a `SET`. It remembers which GUC_REPORT parameters
//! changed and sends one ParameterStatus per changed parameter immediately before ReadyForQuery
//! (`tcop/postgres.c` ~ line 4653), skipping parameters whose value equals the last value reported.
//! `Session` returns the changes per statement (`StatementOutput::param_status`); the wire layer
//! queues them here and drains at ReadyForQuery.
use std::collections::HashMap;

use crate::backend::Backend;

/// Tracks last-reported values and the pending changes of the current command cycle.
#[derive(Debug, Default)]
pub struct ParamReporter {
    last: HashMap<String, String>,
    pending: Vec<(String, String)>,
}

impl ParamReporter {
    /// Start with the values already sent during startup.
    pub fn new(initial: &[(String, String)]) -> ParamReporter {
        ParamReporter { last: initial.iter().cloned().collect(), pending: Vec::new() }
    }

    /// Record a changed value (a later change of the same name replaces the earlier pending one).
    pub fn note(&mut self, name: &str, value: &str) {
        if let Some(p) = self.pending.iter_mut().find(|(n, _)| n == name) {
            p.1 = value.to_string();
        } else {
            self.pending.push((name.to_string(), value.to_string()));
        }
    }

    /// Record a batch (`StatementOutput::param_status`).
    pub fn note_all(&mut self, changes: &[(String, String)]) {
        for (n, v) in changes {
            self.note(n, v);
        }
    }

    /// ParameterStatus messages to send before ReadyForQuery: pending entries whose value differs
    /// from the last reported one, in the order they changed. Updates the last-reported map and
    /// clears the pending list.
    pub fn drain(&mut self) -> Vec<Backend> {
        let mut out = Vec::new();
        for (n, v) in std::mem::take(&mut self.pending) {
            if self.last.get(&n) != Some(&v) {
                self.last.insert(n.clone(), v.clone());
                out.push(Backend::ParameterStatus { name: n, value: v });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_against_last_reported() {
        let mut r = ParamReporter::new(&[("TimeZone".into(), "UTC".into())]);
        r.note("TimeZone", "UTC");
        assert!(r.drain().is_empty());
        r.note("TimeZone", "Europe/Paris");
        r.note("TimeZone", "UTC");
        assert!(r.drain().is_empty());
        r.note("application_name", "x");
        assert_eq!(r.drain().len(), 1);
        assert!(r.drain().is_empty());
    }
}
