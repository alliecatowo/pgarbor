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

use pgarbor::{Session, TxnStatus};

use crate::backend::Backend;
use crate::conn_io::ConnIo;
use crate::error::WireResult;
use crate::shared::Shared;

/// Read timeout for idle polling: `ServerConfig::idle_poll`, but at most 25 ms while the session has
/// LISTEN channels (`shared.db.notify_hub().listening(session.pid())` non-empty), at least 1 ms.
pub fn poll_interval(shared: &Shared, session: &Session) -> Duration {
    poll_interval_for(
        shared.config.idle_poll,
        !shared.db.notify_hub().listening(session.pid()).is_empty(),
    )
}

fn poll_interval_for(idle_poll: Duration, listening: bool) -> Duration {
    if listening {
        idle_poll
            .min(Duration::from_millis(25))
            .max(Duration::from_millis(1))
    } else {
        idle_poll.max(Duration::from_millis(1))
    }
}

/// Deliver pending notifications if the session is idle (module docs). Returns how many were sent.
/// The pid in each message is the NOTIFYING backend's pid (`Notification::pid`).
pub fn deliver(io: &mut ConnIo, session: &mut Session) -> WireResult<usize> {
    if session.txn_status() != TxnStatus::Idle {
        return Ok(0);
    }

    let notifications = session.take_notifications();
    let count = notifications.len();
    for notification in notifications {
        io.send(&Backend::NotificationResponse {
            pid: notification.pid,
            channel: notification.channel,
            payload: notification.payload,
        })?;
    }
    if count > 0 {
        io.flush()?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_has_one_millisecond_floor_and_listen_cap() {
        assert_eq!(poll_interval_for(Duration::ZERO, false), Duration::from_millis(1));
        assert_eq!(poll_interval_for(Duration::from_millis(100), false), Duration::from_millis(100));
        assert_eq!(poll_interval_for(Duration::from_millis(100), true), Duration::from_millis(25));
        assert_eq!(poll_interval_for(Duration::from_millis(10), true), Duration::from_millis(10));
        assert_eq!(poll_interval_for(Duration::ZERO, true), Duration::from_millis(1));
    }
}
