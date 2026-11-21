//! Track and clock numbers, keys and ids: what records, chunks and definitions name a track or a clock by. Also
//! [RelationSet], a set of a track's relations by index, and [ItemNum], an item's number on its track.
//!
//! A number is small and local to a producer's definitions; a key or an id is a 64-bit hash, the same across files and
//! runs. Each has its own type, so one can't pass where another belongs; on the wire each is exactly the plain number
//! it wraps.
//!
//! Numbers count from 1. Track 0 holds the definitions and has no clock ([RecordTrack::Definitions]), so neither type
//! holds 0: a clock number can't pass for a track number, and the definitions track can't pass for a data track. On
//! the wire each is exactly a `UNib32`, as it was before the types; a definition naming 0 doesn't decode.

use core::fmt;
use core::ops::{BitAnd, BitOr};
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

/// A track's stable key: the hash of its name path ([track_key](crate::track_key)), the same across files and
/// producer runs ([TrackDef::key](crate::TrackDef::key)).
///
/// Any value is a key. A track without dimensions has its key for its id ([Self::id]).
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)
)]
pub struct TrackKey(u64);

/// A track's identity: the hash of its key and dimensions ([track_id](crate::track_id),
/// [TrackDef::id](crate::TrackDef::id)), what chunks, summaries and the index place a track by.
///
/// Any value is an id.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)
)]
pub struct TrackId(u64);

/// A clock's stable key: the hash of the name path of what counts and its epoch ([clock_key](crate::clock_key)),
/// the same across files and runs ([ClockDef::key](crate::ClockDef::key)).
///
/// Any value is a key.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)
)]
pub struct ClockKey(u64);

/// The constructor, the accessor, Debug, Display (hex) and the conversion to `u64` of a 64-bit hash type.
macro_rules! hash_impls {
    ($name:ident, $what:literal) => {
        impl $name {
            #[doc = concat!("The ", $what, " `v`; any value is one.")]
            #[must_use]
            pub const fn new(v: u64) -> Self {
                Self(v)
            }

            /// The hash.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:#018x})"), self.0)
            }
        }

        /// In hex, 18 characters: `0x0158_4c57_693f_d3f2` shows as `0x01584c57693fd3f2`.
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#018x}", self.0)
            }
        }

        impl From<$name> for u64 {
            fn from(v: $name) -> Self {
                v.get()
            }
        }
    };
}

hash_impls!(TrackKey, "track key");
hash_impls!(TrackId, "track id");
hash_impls!(ClockKey, "clock key");

impl TrackKey {
    /// The id of the track of this key without dimensions: the key itself.
    #[must_use]
    pub const fn id(self) -> TrackId {
        TrackId(self.0)
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

/// A set of a track's relations, by index: relation `i` of [TrackDef::relations](crate::TrackDef::relations), `i`
/// below [RELATIONS_MAX](crate::RELATIONS_MAX).
///
/// Bit `i` of [Self::bits] is relation `i`. A relation index past the cap is in no set, and can't be inserted: a
/// column's `param` read from a file is checked with [Self::contains] before it means a relation.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
pub struct RelationSet(u16);

impl RelationSet {
    /// No relation.
    pub const EMPTY: Self = Self(0);
    /// Every relation index below the cap.
    pub const ALL: Self = Self(u16::MAX);

    /// The set of bit `i` for relation `i`.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Bit `i` for relation `i`.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// The set of relation `i` alone.
    ///
    /// # Panics
    ///
    /// For `i` at or past [RELATIONS_MAX](crate::RELATIONS_MAX); [Self::try_single] takes an index that may be past
    /// it.
    #[must_use]
    pub fn single(i: u16) -> Self {
        assert!(
            usize::from(i) < crate::RELATIONS_MAX,
            "relation index past RELATIONS_MAX"
        );
        Self(1u16.wrapping_shl(u32::from(i)))
    }

    /// The set of relation `i` alone; None for `i` at or past [RELATIONS_MAX](crate::RELATIONS_MAX).
    #[must_use]
    pub fn try_single(i: u16) -> Option<Self> {
        // RELATIONS_MAX is 16, a u16's bits: the shift is in range iff the index is below the cap
        const {
            assert!(
                crate::RELATIONS_MAX == 16,
                "a RelationSet has a bit per relation index below the cap"
            )
        }
        1u16.checked_shl(u32::from(i)).map(Self)
    }

    /// Whether relation `i` is in the set; false for an index past the cap.
    #[must_use]
    pub fn contains(self, i: u16) -> bool {
        Self::try_single(i).is_some_and(|s| self.0 & s.0 != 0)
    }

    /// The set with relation `i`.
    ///
    /// # Panics
    ///
    /// For `i` at or past [RELATIONS_MAX](crate::RELATIONS_MAX).
    #[must_use]
    pub fn with(self, i: u16) -> Self {
        Self(self.0 | Self::single(i).0)
    }

    /// Adds relation `i`.
    ///
    /// # Panics
    ///
    /// For `i` at or past [RELATIONS_MAX](crate::RELATIONS_MAX).
    pub fn insert(&mut self, i: u16) {
        *self = self.with(i);
    }

    /// Whether no relation is in the set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many relations are in the set.
    #[must_use]
    pub fn len(self) -> usize {
        // at most 16 ones: fits any usize
        usize::try_from(self.0.count_ones()).unwrap_or(crate::RELATIONS_MAX)
    }

    /// The relations in this set and not in `other`.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The lowest relation index in the set; None when it is empty.
    #[must_use]
    pub fn first(self) -> Option<u16> {
        // a u16's trailing zeros are at most 15 when a bit is set
        (self.0 != 0)
            .then(|| u16::try_from(self.0.trailing_zeros()).ok())
            .flatten()
    }

    /// The relation indexes in the set, lowest first.
    pub fn iter(self) -> RelationIter {
        self.into_iter()
    }
}

impl BitAnd for RelationSet {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitOr for RelationSet {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl IntoIterator for RelationSet {
    type Item = u16;
    type IntoIter = RelationIter;

    fn into_iter(self) -> Self::IntoIter {
        RelationIter { set: self, next: 0 }
    }
}

/// The relation indexes of a [RelationSet], lowest first.
#[derive(Clone, Debug)]
pub struct RelationIter {
    set: RelationSet,
    next: u16,
}

impl Iterator for RelationIter {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        while usize::from(self.next) < crate::RELATIONS_MAX {
            let i = self.next;
            self.next = i.saturating_add(1);
            if self.set.contains(i) {
                return Some(i);
            }
        }
        None
    }
}

impl fmt::Debug for RelationSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelationSet{self}")
    }
}

/// The indexes in braces: `{0, 2}`.
impl fmt::Display for RelationSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (n, i) in self.iter().enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{i}")?;
        }
        f.write_str("}")
    }
}

/// An item's number on its track: the first item is 0, each next one more, across chunks and pieces.
///
/// Any value is a number. A count of items stays a plain `u64`: a number plus a count is a number
/// ([Self::checked_add]), a number less a number is a count ([Self::offset_to]). On the wire exactly a `u64`.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    transparent,
    derive(Copy, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)
)]
pub struct ItemNum(u64);

impl ItemNum {
    /// Item 0, the first of a track.
    pub const ZERO: Self = Self(0);
    /// The highest, `u64::MAX`.
    pub const MAX: Self = Self(u64::MAX);

    /// Item `n`.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The item `count` items on; None past `u64::MAX`.
    #[must_use]
    pub const fn checked_add(self, count: u64) -> Option<Self> {
        match self.0.checked_add(count) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The item `count` items on, [Self::MAX] past `u64::MAX`.
    #[must_use]
    pub const fn saturating_add(self, count: u64) -> Self {
        Self(self.0.saturating_add(count))
    }

    /// The item `count` items back; None before 0.
    #[must_use]
    pub const fn checked_sub(self, count: u64) -> Option<Self> {
        match self.0.checked_sub(count) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The item `count` items back, [Self::ZERO] before 0.
    #[must_use]
    pub const fn saturating_sub(self, count: u64) -> Self {
        Self(self.0.saturating_sub(count))
    }

    /// The items from this one up to `end`: None when `end` is before it.
    #[must_use]
    pub const fn offset_to(self, end: Self) -> Option<u64> {
        end.0.checked_sub(self.0)
    }

    /// The items from this one up to `end`, 0 when `end` is before it.
    #[must_use]
    pub const fn saturating_offset_to(self, end: Self) -> u64 {
        end.0.saturating_sub(self.0)
    }

    /// This item and the `count - 1` after it, in order; fewer where they would number past `u64::MAX`.
    pub fn items(self, count: u64) -> impl Iterator<Item = Self> {
        (0..count).map_while(move |k| self.checked_add(k))
    }
}

impl fmt::Debug for ItemNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ItemNum({})", self.0)
    }
}

impl fmt::Display for ItemNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<ItemNum> for u64 {
    fn from(i: ItemNum) -> Self {
        i.get()
    }
}

impl From<u64> for ItemNum {
    fn from(n: u64) -> Self {
        Self(n)
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
    fn hashes_are_a_u64() {
        for v in [0, 1, 0x0158_4c57_693f_d3f2, u64::MAX] {
            let plain = v.to_ww_bytes_owned_vec();
            let k = TrackKey::new(v).to_ww_bytes_owned_vec();
            let i = TrackId::new(v).to_ww_bytes_owned_vec();
            let c = ClockKey::new(v).to_ww_bytes_owned_vec();
            assert_eq!(k, plain);
            assert_eq!(i, plain);
            assert_eq!(c, plain);
            assert_eq!(TrackKey::from_ww_bytes(&k).unwrap(), TrackKey::new(v));
            assert_eq!(TrackId::from_ww_bytes(&i).unwrap(), TrackId::new(v));
            assert_eq!(ClockKey::from_ww_bytes(&c).unwrap(), ClockKey::new(v));
            assert_eq!(TrackKey::new(v).id(), TrackId::new(v));
        }
        let k = TrackKey::new(0x0158_4c57_693f_d3f2);
        assert_eq!(k.to_string(), "0x01584c57693fd3f2");
        assert_eq!(format!("{k:?}"), "TrackKey(0x01584c57693fd3f2)");
        assert_eq!(
            format!("{:?}", TrackId::new(1)),
            "TrackId(0x0000000000000001)"
        );
        assert_eq!(ClockKey::new(1).to_string(), "0x0000000000000001");
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

    #[test]
    fn relation_sets() {
        let s = RelationSet::from_bits(0b101);
        assert!(s.contains(0) && s.contains(2) && !s.contains(1) && !s.contains(16));
        assert_eq!(s.iter().collect::<Vec<_>>(), [0, 2]);
        assert_eq!(s.len(), 2);
        assert_eq!(s.first(), Some(0));
        assert_eq!(RelationSet::EMPTY.first(), None);
        assert_eq!(s.to_string(), "{0, 2}");
        assert_eq!(format!("{s:?}"), "RelationSet{0, 2}");
        assert_eq!(RelationSet::EMPTY.with(3).with(0).bits(), 0b1001);
        assert_eq!(s.without(RelationSet::single(0)), RelationSet::single(2));
        assert_eq!((s & RelationSet::from_bits(0b110)).bits(), 0b100);
        assert_eq!((s | RelationSet::from_bits(0b110)).bits(), 0b111);
        assert_eq!(
            RelationSet::try_single(15),
            Some(RelationSet::from_bits(1 << 15))
        );
        assert_eq!(RelationSet::try_single(16), None);
        assert_eq!(RelationSet::ALL.len(), crate::RELATIONS_MAX);
    }

    #[test]
    fn item_numbers() {
        let i = ItemNum::new(10);
        assert_eq!(i.checked_add(5), Some(ItemNum::new(15)));
        assert_eq!(ItemNum::MAX.checked_add(1), None);
        assert_eq!(ItemNum::MAX.saturating_add(1), ItemNum::MAX);
        assert_eq!(i.checked_sub(11), None);
        assert_eq!(i.saturating_sub(11), ItemNum::ZERO);
        assert_eq!(i.offset_to(ItemNum::new(15)), Some(5));
        assert_eq!(i.offset_to(ItemNum::new(5)), None);
        assert_eq!(i.saturating_offset_to(ItemNum::new(5)), 0);
        assert_eq!(
            i.items(3).collect::<Vec<_>>(),
            [ItemNum::new(10), ItemNum::new(11), ItemNum::new(12)]
        );
        assert_eq!(ItemNum::MAX.items(5).count(), 1);
        assert_eq!(i.to_string(), "10");
        assert_eq!(format!("{i:?}"), "ItemNum(10)");
        for v in [0, 1, u64::MAX] {
            let b = ItemNum::new(v).to_ww_bytes_owned_vec();
            assert_eq!(b, v.to_ww_bytes_owned_vec());
            assert_eq!(ItemNum::from_ww_bytes(&b).unwrap(), ItemNum::new(v));
        }
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
