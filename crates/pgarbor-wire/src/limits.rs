//! Connection admission counters (WIRE-10). Fully implemented, frozen (v1).
//!
//! Two gates protect the server:
//! * `threads`: every accepted socket that got a handler thread (including those still in startup).
//!   Capacity `max_connections * 2 + 16`; beyond it the accept loop closes the socket immediately
//!   without spawning anything (flood protection, no protocol reply).
//! * `sessions`: established sessions (after authentication). Capacity `max_connections`; beyond it the
//!   handler sends FATAL `53300 sorry, too many clients already` and closes. (The engine enforces
//!   `max_sessions` as well; its error is also sent as FATAL.)
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counting gate with RAII permits.
#[derive(Debug)]
pub struct ConnGate {
    max: usize,
    active: AtomicUsize,
}

/// A held permit; released on drop.
#[derive(Debug)]
pub struct ConnGuard {
    gate: Arc<ConnGate>,
}

impl ConnGate {
    pub fn new(max: usize) -> Arc<ConnGate> {
        Arc::new(ConnGate { max, active: AtomicUsize::new(0) })
    }
    /// Take a permit if fewer than `max` are held.
    pub fn try_acquire(self: &Arc<Self>) -> Option<ConnGuard> {
        let mut cur = self.active.load(Ordering::Acquire);
        loop {
            if cur >= self.max {
                return None;
            }
            match self.active.compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(ConnGuard { gate: Arc::clone(self) }),
                Err(actual) => cur = actual,
            }
        }
    }
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
    pub fn capacity(&self) -> usize {
        self.max
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_are_bounded_and_released() {
        let g = ConnGate::new(2);
        let a = g.try_acquire().unwrap();
        let _b = g.try_acquire().unwrap();
        assert!(g.try_acquire().is_none());
        drop(a);
        assert_eq!(g.active(), 1);
        assert!(g.try_acquire().is_some());
    }
}
