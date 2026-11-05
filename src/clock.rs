//! Time sources for expiry.
//!
//! Caches never call [`std::time::Instant::now`] directly. They ask a
//! [`Clock`], which makes expiry testable: production code uses
//! [`SystemClock`], tests use [`ManualClock`] and move time by hand.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A monotonic time source.
///
/// `now` returns the time elapsed since an arbitrary fixed origin chosen by
/// the clock. Only differences between two readings of the same clock are
/// meaningful, and readings must never go backwards.
pub trait Clock: Send + Sync {
    /// Time elapsed since the clock's origin.
    fn now(&self) -> Duration;
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> Duration {
        (**self).now()
    }
}

/// A clock backed by [`Instant`]; its origin is the moment it was created.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Creates a clock that starts counting now.
    pub fn new() -> Self {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that only moves when told to.
///
/// Clones share the same time, so a test can keep one handle and give
/// another to the cache:
///
/// ```
/// use std::time::Duration;
/// use cachekit::clock::{Clock, ManualClock};
///
/// let clock = ManualClock::new();
/// let handle = clock.clone();
/// clock.advance(Duration::from_secs(5));
/// assert_eq!(handle.now(), Duration::from_secs(5));
/// ```
#[derive(Clone, Default)]
pub struct ManualClock {
    nanos: Arc<AtomicU64>,
}

impl ManualClock {
    /// Creates a clock reading zero.
    pub fn new() -> Self {
        ManualClock::default()
    }

    /// Creates a clock that already reads `start`.
    pub fn starting_at(start: Duration) -> Self {
        let clock = ManualClock::new();
        clock.set(start);
        clock
    }

    /// Moves the clock forward by `step`. The reading saturates at the
    /// largest representable time instead of wrapping.
    pub fn advance(&self, step: Duration) {
        let step = duration_to_nanos(step);
        // fetch_update retries on contention; the closure never returns None.
        let _ = self
            .nanos
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                Some(now.saturating_add(step))
            });
    }

    /// Sets the clock to `time`.
    ///
    /// # Panics
    ///
    /// Panics if `time` is earlier than the current reading, because clocks
    /// must be monotonic.
    pub fn set(&self, time: Duration) {
        let target = duration_to_nanos(time);
        let previous = self.nanos.fetch_max(target, Ordering::SeqCst);
        assert!(
            previous <= target,
            "a manual clock cannot be moved backwards"
        );
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

impl fmt::Debug for ManualClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualClock")
            .field("now", &self.now())
            .finish()
    }
}

fn duration_to_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_starts_at_zero_and_advances() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        clock.advance(Duration::from_millis(250));
        clock.advance(Duration::from_millis(750));
        assert_eq!(clock.now(), Duration::from_secs(1));
    }

    #[test]
    fn clones_share_time() {
        let clock = ManualClock::starting_at(Duration::from_secs(10));
        let other = clock.clone();
        other.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_secs(11));
    }

    #[test]
    fn set_moves_forward() {
        let clock = ManualClock::new();
        clock.set(Duration::from_secs(3));
        clock.set(Duration::from_secs(3));
        assert_eq!(clock.now(), Duration::from_secs(3));
    }

    #[test]
    #[should_panic(expected = "cannot be moved backwards")]
    fn set_refuses_to_go_back() {
        let clock = ManualClock::starting_at(Duration::from_secs(3));
        clock.set(Duration::from_secs(2));
    }

    #[test]
    fn advance_saturates() {
        let clock = ManualClock::starting_at(Duration::from_nanos(u64::MAX - 1));
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_nanos(u64::MAX));
    }

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }

    #[test]
    fn clock_works_behind_arc_dyn() {
        let manual = ManualClock::new();
        let shared: Arc<dyn Clock> = Arc::new(manual.clone());
        manual.advance(Duration::from_secs(2));
        assert_eq!(shared.now(), Duration::from_secs(2));
    }
}
