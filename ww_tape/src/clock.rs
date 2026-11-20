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

/// The system's monotonic clock, from the first reading of any [SystemClock] in the process: every one reads the
/// same time.
#[cfg(feature = "std")]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemClock;

#[cfg(feature = "std")]
impl SystemClock {
    /// The system clock.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "std")]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        /// The origin every [SystemClock] counts from: its first reading.
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let origin = ORIGIN.get_or_init(std::time::Instant::now);
        u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// The clock a writer reads, or none: without a clock no interval is ever due, so only the block and byte rules of
/// the policies apply. Equal when both are none, both the system clock, or both the same shared clock.
#[derive(Clone)]
pub struct ClockRef(Source);

#[derive(Clone)]
enum Source {
    None,
    #[cfg(feature = "std")]
    System,
    Shared(Arc<dyn Clock>),
}

impl ClockRef {
    /// No clock: interval rules are off.
    pub const NONE: Self = Self(Source::None);

    /// The system clock ([SystemClock]), the default with `std`.
    #[cfg(feature = "std")]
    pub const SYSTEM: Self = Self(Source::System);

    /// `clock`, shared by every clone.
    pub fn new(clock: impl Clock + 'static) -> Self {
        Self(Source::Shared(Arc::new(clock)))
    }

    /// Milliseconds now; None without a clock.
    pub fn now_ms(&self) -> Option<u64> {
        match &self.0 {
            Source::None => None,
            #[cfg(feature = "std")]
            Source::System => Some(SystemClock.now_ms()),
            Source::Shared(c) => Some(c.now_ms()),
        }
    }

    /// Whether `interval` has passed since `since` (a reading of this clock); false without a clock or a reading.
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
        return Self::SYSTEM;
        #[cfg(not(feature = "std"))]
        return ClockRef::NONE;
    }
}

impl PartialEq for ClockRef {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Source::None, Source::None) => true,
            #[cfg(feature = "std")]
            (Source::System, Source::System) => true,
            (Source::Shared(a), Source::Shared(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for ClockRef {}

impl core::fmt::Debug for ClockRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self.0 {
            Source::None => "ClockRef::NONE",
            #[cfg(feature = "std")]
            Source::System => "ClockRef::SYSTEM",
            Source::Shared(_) => "ClockRef(clock)",
        })
    }
}
