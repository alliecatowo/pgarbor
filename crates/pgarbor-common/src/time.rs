//! Clock types used by the engine instead of `std::time::{Instant, SystemTime, UNIX_EPOCH}`.
//!
//! On every target except `wasm32` these are plain re-exports of the `std` items. `std`'s clocks panic on
//! `wasm32-unknown-unknown` ("time not implemented on this platform"), so there they are small shims over
//! JavaScript's `Date.now()` and `performance.now()` with the subset of the `std` API the engine uses.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

#[cfg(target_arch = "wasm32")]
pub use wasm::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::ops::{Add, AddAssign, Sub};
    use std::time::Duration;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = Date, js_name = now)]
        fn date_now() -> f64;
        #[wasm_bindgen(js_namespace = performance, js_name = now)]
        fn performance_now() -> f64;
    }

    fn ms(v: f64) -> Duration {
        if v.is_finite() && v > 0.0 { Duration::from_secs_f64(v / 1000.0) } else { Duration::ZERO }
    }

    /// Error returned when a `SystemTime` is earlier than the reference point.
    #[derive(Clone, Debug)]
    pub struct SystemTimeError(pub Duration);

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct SystemTime(Duration);

    pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::ZERO);

    impl SystemTime {
        pub fn now() -> SystemTime {
            SystemTime(ms(date_now()))
        }
        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            self.0.checked_sub(earlier.0).ok_or_else(|| SystemTimeError(earlier.0 - self.0))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(Duration);

    impl Instant {
        pub fn now() -> Instant {
            Instant(ms(performance_now()))
        }
        pub fn elapsed(&self) -> Duration {
            Instant::now().0.saturating_sub(self.0)
        }
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }
        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.duration_since(earlier)
        }
        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0)
        }
        pub fn checked_add(&self, d: Duration) -> Option<Instant> {
            self.0.checked_add(d).map(Instant)
        }
        pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
            self.0.checked_sub(d).map(Instant)
        }
    }
    impl Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            Instant(self.0 + d)
        }
    }
    impl AddAssign<Duration> for Instant {
        fn add_assign(&mut self, d: Duration) {
            self.0 += d;
        }
    }
    impl Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, o: Instant) -> Duration {
            self.duration_since(o)
        }
    }
    impl Sub<Duration> for Instant {
        type Output = Instant;
        fn sub(self, d: Duration) -> Instant {
            Instant(self.0 - d)
        }
    }
}

/// `std::process::id()`, or a constant on `wasm32` where `std` has no process model and panics.
pub fn process_id() -> u32 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::id()
    }
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
}
