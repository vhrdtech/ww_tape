//! The writer's clock: when a sync, a checkpoint index or a cut of a chain is due by time.
//!
//! A [TapeWriter](crate::TapeWriter) reads time only through a [ClockRef], so it runs where there is no system clock
//! (firmware, WASM) and tests move time by hand. Time never reaches the bytes: it only decides when a writer syncs,
//! places a checkpoint or cuts ([SyncPolicy::Interval](crate::SyncPolicy::Interval),
//! [IndexPolicy::interval](crate::IndexPolicy::interval), [CutPolicy::interval](crate::CutPolicy::interval)).

use alloc::sync::Arc;

/// A source of milliseconds, from an origin the clock picks, that never decrease.
pub trait Clock: Send + Sync {
    /// Milliseconds since the clock's origin.
    fn now_ms(&self) -> u64;
}

/// The system's monotonic clock, from when it was made.
#[cfg(feature = "std")]
#[derive(Copy, Clone, Debug)]
pub struct SystemClock(std::time::Instant);

#[cfg(feature = "std")]
impl SystemClock {
    /// A clock starting now.
    pub fn new() -> Self {
        SystemClock(std::time::Instant::now())
    }
}

#[cfg(feature = "std")]
impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The clock a writer reads, or none: without a clock no interval is ever due, so only the block and byte rules of
/// the policies apply. Equal when both are none or both are the same clock.
#[derive(Clone)]
pub struct ClockRef(Option<Arc<dyn Clock>>);

impl ClockRef {
    /// No clock: interval rules are off.
    pub const NONE: ClockRef = ClockRef(None);

    /// `clock`, shared by every clone.
    pub fn new(clock: impl Clock + 'static) -> Self {
        ClockRef(Some(Arc::new(clock)))
    }

    /// Milliseconds now; None without a clock.
    pub fn now_ms(&self) -> Option<u64> {
        self.0.as_ref().map(|c| c.now_ms())
    }

    /// Whether `interval` has passed since `since` (a reading of this clock); false without a clock or a reading.
    #[cfg(feature = "std")]
    pub(crate) fn passed(&self, since: Option<u64>, interval: core::time::Duration) -> bool {
        match (self.now_ms(), since) {
            (Some(now), Some(since)) => {
                u128::from(now.saturating_sub(since)) >= interval.as_millis()
            }
            _ => false,
        }
    }
}

/// The system clock with `std`, else none.
impl Default for ClockRef {
    fn default() -> Self {
        #[cfg(feature = "std")]
        return ClockRef::new(SystemClock::new());
        #[cfg(not(feature = "std"))]
        return ClockRef::NONE;
    }
}

impl PartialEq for ClockRef {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for ClockRef {}

impl core::fmt::Debug for ClockRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(_) => f.write_str("ClockRef(clock)"),
            None => f.write_str("ClockRef::NONE"),
        }
    }
}
