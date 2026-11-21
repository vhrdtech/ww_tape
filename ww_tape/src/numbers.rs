//! Track and clock numbers: the small numbers records, chunks and definitions name a track or a clock by.
//!
//! Both count from 1. Track 0 holds the definitions and has no clock ([RecordTrack::Definitions]), so neither type
//! holds 0: a clock number can't pass for a track number, and the definitions track can't pass for a data track. On
//! the wire each is exactly a `UNib32`, as it was before the types; a definition naming 0 doesn't decode.

use core::fmt;
use shrink_wrap::UNib32;
use shrink_wrap::prelude::*;

/// A data track's number: what records, chunks and summaries name it by, 1 and up ([TrackDef::number]).
///
/// Track 0 holds the definitions and is no data track: a record that may be on it names a [RecordTrack].
///
/// [TrackDef::number]: crate::TrackDef::number
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord),
    validate = TrackNum::validate
)]
pub struct TrackNum(UNib32);

impl TrackNum {
    /// The lowest, 1.
    pub const MIN: Self = Self(UNib32(1));
    /// The highest, `u32::MAX`.
    pub const MAX: Self = Self(UNib32(u32::MAX));

    /// Track `n`, for a constant: `const PHASE_A: TrackNum = TrackNum::new(4);`.
    ///
    /// # Panics
    ///
    /// For 0, the definitions track (a compile error in a constant); [Self::try_new] takes a number that may be 0.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        assert!(n != 0, "track 0 holds the definitions, no data track");
        Self(UNib32(n))
    }

    /// Track `n`; None for 0, the definitions track.
    #[must_use]
    pub const fn try_new(n: u32) -> Option<Self> {
        if n == 0 { None } else { Some(Self(UNib32(n))) }
    }

    /// The number, 1 and up.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.0
    }

    /// Refuses 0 read from the wire.
    fn validate(&self) -> Result<(), shrink_wrap::Error> {
        if self.0.0 == 0 {
            Err(shrink_wrap::Error::SubtypeOutOfRange)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for TrackNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackNum({})", self.0.0)
    }
}

impl fmt::Display for TrackNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.0, f)
    }
}

impl From<TrackNum> for u32 {
    fn from(t: TrackNum) -> Self {
        t.get()
    }
}

/// A clock's number among the definitions, 1 and up ([ClockDef::number]): what a track, a map and a block's clock
/// table name it by.
///
/// [ClockDef::number]: crate::ClockDef::number
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord),
    validate = ClockNum::validate
)]
pub struct ClockNum(UNib32);

impl ClockNum {
    /// The lowest, 1.
    pub const MIN: Self = Self(UNib32(1));
    /// The highest, `u32::MAX`.
    pub const MAX: Self = Self(UNib32(u32::MAX));

    /// Clock `n`, for a constant: `const HOST: ClockNum = ClockNum::new(1);`.
    ///
    /// # Panics
    ///
    /// For 0, which names no clock (a compile error in a constant); [Self::try_new] takes a number that may be 0.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        assert!(n != 0, "clock 0 names no clock");
        Self(UNib32(n))
    }

    /// Clock `n`; None for 0, which names no clock.
    #[must_use]
    pub const fn try_new(n: u32) -> Option<Self> {
        if n == 0 { None } else { Some(Self(UNib32(n))) }
    }

    /// The number, 1 and up.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.0
    }

    /// Refuses 0 read from the wire.
    fn validate(&self) -> Result<(), shrink_wrap::Error> {
        if self.0.0 == 0 {
            Err(shrink_wrap::Error::SubtypeOutOfRange)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for ClockNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClockNum({})", self.0.0)
    }
}

impl fmt::Display for ClockNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.0, f)
    }
}

impl From<ClockNum> for u32 {
    fn from(c: ClockNum) -> Self {
        c.get()
    }
}

/// The track a record is on: track 0, the definitions, or a data track.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordTrack {
    /// Track 0: one [Describe](crate::Describe) per record, no time.
    Definitions,
    /// A data track.
    Data(TrackNum),
}

impl RecordTrack {
    /// The track numbered `n` in a record head: 0 is the definitions.
    #[must_use]
    pub const fn from_u32(n: u32) -> Self {
        match TrackNum::try_new(n) {
            None => Self::Definitions,
            Some(t) => Self::Data(t),
        }
    }

    /// The number a record head carries: 0 for the definitions.
    #[must_use]
    pub const fn get(self) -> u32 {
        match self {
            Self::Definitions => 0,
            Self::Data(t) => t.get(),
        }
    }

    /// The data track; None for the definitions.
    #[must_use]
    pub const fn data(self) -> Option<TrackNum> {
        match self {
            Self::Definitions => None,
            Self::Data(t) => Some(t),
        }
    }

    /// Whether it is track 0, the definitions.
    #[must_use]
    pub const fn is_definitions(self) -> bool {
        matches!(self, Self::Definitions)
    }
}

impl From<TrackNum> for RecordTrack {
    fn from(t: TrackNum) -> Self {
        Self::Data(t)
    }
}

impl fmt::Display for RecordTrack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.get(), f)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_a_unib32() {
        for n in [1, 7, 8, 300, u32::MAX] {
            let t = TrackNum::new(n).to_ww_bytes_owned_vec();
            let c = ClockNum::new(n).to_ww_bytes_owned_vec();
            let plain = UNib32(n).to_ww_bytes_owned_vec();
            assert_eq!(t, plain);
            assert_eq!(c, plain);
            assert_eq!(TrackNum::from_ww_bytes(&t).unwrap(), TrackNum::new(n));
            assert_eq!(ClockNum::from_ww_bytes(&c).unwrap(), ClockNum::new(n));
        }
    }

    #[test]
    fn zero_is_refused() {
        let zero = UNib32(0).to_ww_bytes_owned_vec();
        assert!(TrackNum::from_ww_bytes(&zero).is_err());
        assert!(ClockNum::from_ww_bytes(&zero).is_err());
        assert_eq!(TrackNum::try_new(0), None);
        assert_eq!(ClockNum::try_new(0), None);
        assert_eq!(RecordTrack::from_u32(0), RecordTrack::Definitions);
        assert_eq!(
            RecordTrack::from_u32(5),
            RecordTrack::Data(TrackNum::new(5))
        );
        assert_eq!(RecordTrack::Data(TrackNum::new(5)).get(), 5);
    }

    trait Bytes {
        fn to_ww_bytes_owned_vec(&self) -> Vec<u8>;
    }

    impl<T: SerializeShrinkWrap> Bytes for T {
        fn to_ww_bytes_owned_vec(&self) -> Vec<u8> {
            let mut buf = [0u8; 16];
            self.to_ww_bytes(&mut buf).unwrap().to_vec()
        }
    }
}
