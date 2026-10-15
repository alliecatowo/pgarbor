//! Asynchronous notification delivery (lease WIR-15; WIRE-11). PG: `commands/async.c`
//! (`ProcessNotifyInterrupt`, `NotifyMyFrontEnd`), `protocol.sgml` "NotificationResponse".
//!
//! Mechanism: the engine's `NotifyHub` appends committed notifications to a per-session mailbox
//! (`Session::take_notifications`). The connection thread never blocks forever in a read: it polls
//! with a short read timeout ([`poll_interval`]) and, whenever the session is idle (status `I`, no
//! open transaction block, not skipping until Sync), drains the mailbox and queues one
//! `NotificationResponse{pid, channel, payload}` per notification, then flushes. Notifications
//! arriving while a transaction block is open stay queued and are delivered after it ends (PG parity:
//! delivery only outside transactions). The hub waker is not used (a blocked `read` cannot be woken
//! portably without extra descriptors); latency is bounded by the poll interval.
use std::time::Duration;

use pgarbor::Session;
use pgarbor_common::PgError;

use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::shared::Shared;

/// Read timeout for idle polling: `ServerConfig::idle_poll`, but at most 25 ms while the session has
/// LISTEN channels (`shared.db.notify_hub().listening(session.pid())` non-empty), at least 1 ms.
#[allow(unused_variables)]
pub fn poll_interval(shared: &Shared, session: &Session) -> Duration {
    // Default is the configured poll; the LISTEN speed-up is the worker's job.
    shared.config.idle_poll
}

/// Deliver pending notifications if the session is idle (module docs). Returns how many were sent.
/// The pid in each message is the NOTIFYING backend's pid (`Notification::pid`).
#[allow(unused_variables)]
pub fn deliver(io: &mut ConnIo, session: &mut Session) -> WireResult<usize> {
    Err(WireError::Pg(PgError::unimplemented("WIR-15", "notification delivery (notify_out::deliver)")))
}
