//! State records: what makes a record state, the hash it is known by and the slot it fills.
//!
//! Track 0 (the definitions) and every track whose policy is [TrackRepeat::State](crate::TrackRepeat::State) hold
//! state, not events: a reader at any place wants the item in force there, the nearest record of its **slot** behind
//! that place. [Slot::of] is the one place that decides a record's slot:
//!
//! - a record of a state track fills the track's slot, [Slot::State];
//! - a definition fills the slot of what it defines: the producer, the layout, a track or a clock by number, a map by
//!   its two clocks;
//! - a bundle, a typed item, or a definition of a kind this crate doesn't know is a slot of its own, by its hash.
//!
//! An item is known by its [ItemHash]: BLAKE3 of its bytes, cut to 128 bits. A writer puts an item again only when
//! the newest record of its slot isn't the same item close behind ([TrackRepeat::State](crate::TrackRepeat::State)'s
//! `near`); a reader holding an item of that hash takes the record as nothing new. Two items colliding in 128 bits
//! (about 2^64 items before one pair is likely) would make a writer skip writing the second one where the first is
//! close behind in the same slot: the slot then reads as the first item.
//!
//! The index lists every state record by its slot and hash ([crate::index]), so a reader finds the item in force at a
//! place, or an item by its hash, without a scan.

use shrink_wrap::prelude::*;

use crate::{DESCRIBE_TRACK, Describe};

/// Bytes of an [ItemHash].
pub const ITEM_HASH_LEN: usize = 16;

/// The hash an item is known by: BLAKE3 of its bytes, the first [ITEM_HASH_LEN] bytes.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ItemHash(pub [u8; ITEM_HASH_LEN]);

impl ItemHash {
    /// The hash of `item`'s bytes.
    pub fn of(item: &[u8]) -> Self {
        let full = blake3::hash(item);
        let mut out = [0u8; ITEM_HASH_LEN];
        for (d, s) in out.iter_mut().zip(full.as_bytes()) {
            *d = *s;
        }
        Self(out)
    }
}

impl core::fmt::Debug for ItemHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// What a definition defines: the code an index entry carries in its level byte ([crate::IndexEntry::level]), 0
/// standing for a record of a state track.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DefinitionKind {
    Producer,
    Track,
    Bundle,
    Clock,
    Map,
    Item,
    Layout,
    /// A [Describe] variant this crate doesn't know.
    Unknown,
}

impl DefinitionKind {
    /// The kind of `d`.
    pub fn of(d: &Describe<'_>) -> Self {
        match d {
            Describe::Producer(_) => Self::Producer,
            Describe::Track(_) => Self::Track,
            Describe::Bundle { .. } => Self::Bundle,
            Describe::Clock(_) => Self::Clock,
            Describe::Map(_) => Self::Map,
            Describe::Item(_) => Self::Item,
            Describe::Layout(_) => Self::Layout,
            Describe::Unknown(_) => Self::Unknown,
        }
    }

    /// Its code in an index entry: 1 and up.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Producer => 1,
            Self::Track => 2,
            Self::Bundle => 3,
            Self::Clock => 4,
            Self::Map => 5,
            Self::Item => 6,
            Self::Layout => 7,
            Self::Unknown => 15,
        }
    }

    /// The kind of code `v`; None for 0 (a state track's record) and codes no kind has.
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Producer,
            2 => Self::Track,
            3 => Self::Bundle,
            4 => Self::Clock,
            5 => Self::Map,
            6 => Self::Item,
            7 => Self::Layout,
            15 => Self::Unknown,
            _ => return None,
        })
    }
}

/// The state a record is the newest of, see the [module docs](self).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// The item of state track `track` (1 and up).
    State {
        track: u32,
    },
    Producer,
    Layout,
    /// The definition of track `number`.
    Track {
        number: u32,
    },
    /// The definition of clock `number`.
    Clock {
        number: u32,
    },
    /// The map from clock `from` to clock `to`.
    Map {
        from: u32,
        to: u32,
    },
    /// A bundle, a typed item or a definition of a kind this crate doesn't know: each item a slot of its own.
    Own {
        kind: DefinitionKind,
        hash: ItemHash,
    },
}

impl Slot {
    /// The slot of record `item` on `track`: on track 0 by the definition it decodes to, None when it doesn't decode.
    pub fn of(track: u32, item: &[u8]) -> Option<Self> {
        if track != DESCRIBE_TRACK {
            return Some(Self::State { track });
        }
        let d = Describe::from_ww_bytes(item).ok()?;
        Some(Self::of_definition(&d, || ItemHash::of(item)))
    }

    /// The slot of definition `d`; `hash` gives its item's hash, asked only for a slot of its own.
    pub fn of_definition(d: &Describe<'_>, hash: impl FnOnce() -> ItemHash) -> Self {
        match d {
            Describe::Producer(_) => Self::Producer,
            Describe::Layout(_) => Self::Layout,
            Describe::Track(t) => Self::Track { number: t.number.0 },
            Describe::Clock(c) => Self::Clock { number: c.number.0 },
            Describe::Map(m) => Self::Map {
                from: m.from.0,
                to: m.to.0,
            },
            _ => Self::Own {
                kind: DefinitionKind::of(d),
                hash: hash(),
            },
        }
    }

    /// Whether the slot is a definition's (track 0).
    pub fn is_definition(&self) -> bool {
        !matches!(self, Self::State { .. })
    }

    /// The track its records are on: 0 for a definition.
    pub fn track(&self) -> u32 {
        match self {
            Self::State { track } => *track,
            _ => DESCRIBE_TRACK,
        }
    }

    #[cfg(feature = "alloc")]
    /// How an index entry lists the slot: its level code, number and second number.
    pub(crate) fn code(&self) -> (u8, u32, u32) {
        match *self {
            Self::State { track } => (0, track, 0),
            Self::Producer => (DefinitionKind::Producer.to_u8(), 0, 0),
            Self::Layout => (DefinitionKind::Layout.to_u8(), 0, 0),
            Self::Track { number } => (DefinitionKind::Track.to_u8(), number, 0),
            Self::Clock { number } => (DefinitionKind::Clock.to_u8(), number, 0),
            Self::Map { from, to } => (DefinitionKind::Map.to_u8(), from, to),
            Self::Own { kind, .. } => (kind.to_u8(), 0, 0),
        }
    }

    /// The slot an index entry lists by `code`, with the item's `hash`; None for a level no slot has.
    pub(crate) fn from_code(level: u8, number: u32, second: u32, hash: ItemHash) -> Option<Self> {
        if level == 0 {
            return Some(Self::State { track: number });
        }
        Some(match DefinitionKind::from_u8(level)? {
            DefinitionKind::Producer => Self::Producer,
            DefinitionKind::Layout => Self::Layout,
            DefinitionKind::Track => Self::Track { number },
            DefinitionKind::Clock => Self::Clock { number },
            DefinitionKind::Map => Self::Map {
                from: number,
                to: second,
            },
            kind => Self::Own { kind, hash },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_reads_back_from_its_index_code() {
        let hash = ItemHash::of(b"bundle");
        let slots = [
            Slot::State { track: 9 },
            Slot::Producer,
            Slot::Layout,
            Slot::Track { number: 3 },
            Slot::Clock { number: 2 },
            Slot::Map { from: 2, to: 1 },
            Slot::Own {
                kind: DefinitionKind::Bundle,
                hash,
            },
        ];
        for s in slots {
            let (level, n, second) = s.code();
            assert_eq!(Slot::from_code(level, n, second, hash), Some(s));
        }
        assert_eq!(Slot::from_code(9, 0, 0, hash), None);
    }

    #[test]
    fn the_hash_is_blake3_cut_to_128_bits() {
        let h = ItemHash::of(b"abc");
        assert_eq!(&h.0[..], &blake3::hash(b"abc").as_bytes()[..16]);
    }
}
