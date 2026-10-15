//! Registry of live connections for graceful shutdown and diagnostics. Fully implemented, frozen (v1).
//!
//! `Server::shutdown` must unblock threads that sit in a socket read or inside a long statement:
//! 1. `terminate_all()`: `CancelToken::terminate()` on every registered session (executor loops see
//!    it and fail with FATAL 57P01);
//! 2. after the grace period `close_all_sockets()`: `Stream::shutdown_both()` on a clone of each
//!    socket (a blocked `read` returns EOF; nothing relies on signals).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pgarbor::CancelToken;

use crate::transport::Stream;

struct Entry {
    socket: Stream,
    token: Option<CancelToken>,
    user: String,
}

/// Live connections by connection id.
#[derive(Default)]
pub struct ConnRegistry {
    conns: Mutex<HashMap<u64, Entry>>,
}

/// Removes the entry on drop.
pub struct RegistryGuard {
    registry: Arc<ConnRegistry>,
    conn_id: u64,
}

impl ConnRegistry {
    pub fn new() -> Arc<ConnRegistry> {
        Arc::new(ConnRegistry::default())
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        self.conns.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Register `socket` (a `try_clone` of the connection's stream).
    pub fn register(self: &Arc<Self>, conn_id: u64, socket: Stream) -> RegistryGuard {
        self.lock().insert(conn_id, Entry { socket, token: None, user: String::new() });
        RegistryGuard { registry: Arc::clone(self), conn_id }
    }
    /// Attach the session's cancel token and user once the session exists.
    pub fn set_session(&self, conn_id: u64, token: CancelToken, user: &str) {
        if let Some(e) = self.lock().get_mut(&conn_id) {
            e.token = Some(token);
            e.user = user.to_string();
        }
    }
    pub fn len(&self) -> usize {
        self.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Ask every established session to terminate (FATAL 57P01).
    pub fn terminate_all(&self) {
        for e in self.lock().values() {
            if let Some(t) = &e.token {
                t.terminate();
            }
        }
    }
    /// Close every socket (unblocks reads).
    pub fn close_all_sockets(&self) {
        for e in self.lock().values() {
            let _ = e.socket.shutdown_both();
        }
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.conn_id);
    }
}
