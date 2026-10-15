//! State shared by the listener threads and every connection thread. Fully implemented, frozen (v1).
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pgarbor::Database;

use crate::config::ServerConfig;
use crate::limits::ConnGate;
use crate::registry::ConnRegistry;

/// Everything a connection needs (module docs). Created once per [`crate::server::Server`].
pub struct Shared {
    pub db: Database,
    pub config: ServerConfig,
    /// Set by `Server::shutdown`; connection loops check it on every idle poll.
    pub stop: AtomicBool,
    /// Handler threads (pre-auth + established); capacity `max_connections * 2 + 16`.
    pub threads: Arc<ConnGate>,
    /// Established sessions; capacity `max_connections`.
    pub sessions: Arc<ConnGate>,
    pub registry: Arc<ConnRegistry>,
    next_conn_id: AtomicU64,
    /// Random per-process secret used to derive the salt of mock SCRAM exchanges for unknown roles.
    pub mock_secret: [u8; 32],
}

impl Shared {
    pub fn new(db: Database, config: ServerConfig) -> Arc<Shared> {
        let mut mock_secret = [0u8; 32];
        rand::fill(&mut mock_secret);
        let max = config.max_connections;
        Arc::new(Shared {
            db,
            config,
            stop: AtomicBool::new(false),
            threads: ConnGate::new(max.saturating_mul(2).saturating_add(16)),
            sessions: ConnGate::new(max),
            registry: ConnRegistry::new(),
            next_conn_id: AtomicU64::new(1),
            mock_secret,
        })
    }
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    /// Next connection id (starts at 1).
    pub fn next_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }
}
