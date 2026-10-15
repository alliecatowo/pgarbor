//! LISTEN / NOTIFY hub (PG: `commands/async.c`). Fully implemented, frozen (v1) - small enough that
//! no lease is needed.
//!
//! Semantics implemented:
//! * `LISTEN ch` / `UNLISTEN ch` / `UNLISTEN *` take effect when the issuing transaction COMMITS
//!   (kept in [`ListenState`] until then; dropped on abort);
//! * `NOTIFY ch, 'payload'` / `pg_notify(ch, payload)` are queued per transaction (identical
//!   `(channel, payload)` pairs de-duplicated, order preserved) and delivered at COMMIT to every
//!   session listening on `ch` (including the sender), in commit order; dropped on abort;
//! * delivery = appending to the listener's mailbox; the session drains it with
//!   `Session::take_notifications` (the wire layer sends NotificationResponse); an optional waker
//!   callback registered by the wire layer is invoked on each delivery so an idle connection thread
//!   can wake up.
//! * channel names are identifiers (case folded by the binder); payload max 7999 bytes (PG:
//!   `payload string too long`, 22023).
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use pgarbor_common::{PgError, Result, SqlState};

use crate::types::Notification;

/// Wake-up callback (`Send + Sync`).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

struct Mailbox {
    channels: HashSet<String>,
    queue: VecDeque<Notification>,
    waker: Option<Waker>,
}

/// Process-wide (per `Database`) notification hub.
#[derive(Default)]
pub struct NotifyHub {
    boxes: Mutex<HashMap<i32, Mailbox>>,
}

impl std::fmt::Debug for NotifyHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NotifyHub")
    }
}

impl NotifyHub {
    pub fn new() -> NotifyHub {
        NotifyHub::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i32, Mailbox>> {
        self.boxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Create the mailbox of session `pid`.
    pub fn register(&self, pid: i32, waker: Option<Waker>) {
        self.lock().insert(pid, Mailbox { channels: HashSet::new(), queue: VecDeque::new(), waker });
    }

    /// Install/replace the wake-up callback of `pid`.
    pub fn set_waker(&self, pid: i32, waker: Option<Waker>) {
        if let Some(b) = self.lock().get_mut(&pid) {
            b.waker = waker;
        }
    }

    pub fn unregister(&self, pid: i32) {
        self.lock().remove(&pid);
    }

    pub fn listen(&self, pid: i32, channel: &str) {
        if let Some(b) = self.lock().get_mut(&pid) {
            b.channels.insert(channel.to_string());
        }
    }

    /// `None` = `UNLISTEN *`.
    pub fn unlisten(&self, pid: i32, channel: Option<&str>) {
        if let Some(b) = self.lock().get_mut(&pid) {
            match channel {
                Some(c) => {
                    b.channels.remove(c);
                }
                None => b.channels.clear(),
            }
        }
    }

    /// Channels `pid` listens on, sorted (`pg_listening_channels()`).
    pub fn listening(&self, pid: i32) -> Vec<String> {
        let mut v: Vec<String> = self.lock().get(&pid).map(|b| b.channels.iter().cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// Deliver a committed notification to every listener of `channel`.
    pub fn post(&self, from_pid: i32, channel: &str, payload: &str) {
        let mut wake: Vec<Waker> = Vec::new();
        {
            let mut g = self.lock();
            for b in g.values_mut() {
                if b.channels.contains(channel) {
                    b.queue.push_back(Notification {
                        channel: channel.to_string(),
                        payload: payload.to_string(),
                        pid: from_pid,
                    });
                    if let Some(w) = &b.waker {
                        wake.push(Arc::clone(w));
                    }
                }
            }
        }
        for w in wake {
            w();
        }
    }

    /// Drain `pid`'s mailbox.
    pub fn take(&self, pid: i32) -> Vec<Notification> {
        match self.lock().get_mut(&pid) {
            Some(b) => b.queue.drain(..).collect(),
            None => Vec::new(),
        }
    }
}

/// A LISTEN-state change waiting for commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenOp {
    Listen(String),
    Unlisten(String),
    UnlistenAll,
}

/// Per-session pending LISTEN/NOTIFY work of the current transaction.
#[derive(Debug, Default)]
pub struct ListenState {
    pub ops: Vec<ListenOp>,
    pub notifies: Vec<(String, String)>,
}

/// Maximum NOTIFY payload length in bytes (PG `NOTIFY_PAYLOAD_MAX_LENGTH` - 1).
pub const MAX_PAYLOAD: usize = 7999;

impl ListenState {
    /// Queue a NOTIFY (de-duplicated). Payload too long -> 22023 `payload string too long`; empty
    /// channel -> 22023 `channel name cannot be empty`.
    pub fn queue_notify(&mut self, channel: &str, payload: &str) -> Result<()> {
        if channel.is_empty() {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "channel name cannot be empty"));
        }
        if payload.len() > MAX_PAYLOAD {
            return Err(PgError::new(SqlState::INVALID_PARAMETER_VALUE, "payload string too long"));
        }
        let item = (channel.to_string(), payload.to_string());
        if !self.notifies.contains(&item) {
            self.notifies.push(item);
        }
        Ok(())
    }

    pub fn queue_op(&mut self, op: ListenOp) {
        self.ops.push(op);
    }

    /// Apply at commit: LISTEN changes first (PG applies them in `PreCommit_Notify`), then deliver
    /// the queued notifications.
    pub fn at_commit(&mut self, hub: &NotifyHub, pid: i32) {
        for op in self.ops.drain(..) {
            match op {
                ListenOp::Listen(c) => hub.listen(pid, &c),
                ListenOp::Unlisten(c) => hub.unlisten(pid, Some(&c)),
                ListenOp::UnlistenAll => hub.unlisten(pid, None),
            }
        }
        for (c, p) in self.notifies.drain(..) {
            hub.post(pid, &c, &p);
        }
    }

    /// Drop everything at abort.
    pub fn at_abort(&mut self) {
        self.ops.clear();
        self.notifies.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_at_commit_only() {
        let hub = NotifyHub::new();
        hub.register(1, None);
        hub.register(2, None);
        let mut st = ListenState::default();
        st.queue_op(ListenOp::Listen("ch".into()));
        st.queue_notify("ch", "hi").unwrap();
        st.queue_notify("ch", "hi").unwrap();
        assert!(hub.take(1).is_empty());
        st.at_commit(&hub, 1);
        let got = hub.take(1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, "hi");
        assert!(hub.take(2).is_empty());
    }

    #[test]
    fn payload_limits() {
        let mut st = ListenState::default();
        assert!(st.queue_notify("", "x").is_err());
        assert!(st.queue_notify("c", &"x".repeat(8000)).is_err());
    }
}
