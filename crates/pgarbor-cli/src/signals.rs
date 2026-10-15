//! Process signals without the `libc` crate (amendment pgarbor-cli-01). Fully implemented, no lease.
//!
//! `signal(2)` is declared directly (std links libc). The handler only stores into atomics, so it
//! is async-signal-safe. glibc `signal()` installs handlers with SA_RESTART, which means a blocking
//! `accept()`/`read()` is NOT interrupted: every long-running loop in this program polls
//! [`shutdown_requested`] (the wire listener polls its stop flag every 50 ms; `serve` polls this flag
//! every 50 ms and then calls `Server::shutdown`).
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(unix)]
extern "C" fn on_signal(sig: i32) {
    LAST_SIGNAL.store(sig, Ordering::SeqCst);
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Install handlers for SIGINT, SIGTERM and SIGHUP that request a clean shutdown. Idempotent.
pub fn install() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let h = on_signal as extern "C" fn(i32) as usize;
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            // SAFETY: `signal` is the C library function; the handler only touches atomics.
            let prev = unsafe { signal(sig, h) };
            if prev == usize::MAX {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// True once SIGINT/SIGTERM/SIGHUP arrived (or [`request_shutdown`] was called).
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// The signal number that triggered shutdown (0 if none / manual).
pub fn last_signal() -> i32 {
    LAST_SIGNAL.load(Ordering::SeqCst)
}

/// Manual trigger (tests, internal errors).
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Clear the flag (tests).
pub fn reset() {
    SHUTDOWN.store(false, Ordering::SeqCst);
    LAST_SIGNAL.store(0, Ordering::SeqCst);
}

/// Send `sig` to process `pid` (used by `branch exec` to forward SIGTERM/SIGINT to the child).
/// Returns false when the process does not exist or on non-unix targets.
pub fn send_signal(pid: u32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: plain libc call with integer arguments.
        unsafe { kill(pid as i32, sig) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
        false
    }
}
