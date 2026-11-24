//! State records: what makes a record state, the hash it is known by and the slot it fills.
//!
//! Track 0 (the definitions) and every track whose policy is [TrackRepeat::State](crate::TrackRepeat::State) hold
//! state, not events: a reader at any place wants the item in force there, the nearest record of its **slot** behind
//! that place. [Slot::of] is the one place that decides a record's slot:
//!
//! - a record of a state track fills the track's slot, [Slot::State];
//! - a definition fills the slot of what it defines: the producer, the layout, a track or a clock by number, a map by
//!   its two clocks;
//! - a bundle, a schema, a typed item, or a definition of a kind this crate doesn't know is a slot of its own, by its hash;
//! - a record of a keyed state track ([TrackRepeat::Keyed](crate::TrackRepeat::Keyed)) fills the slot of its key,
//!   [Slot::Keyed].
//!
//! An item is known by its [ItemHash]: BLAKE3 of its bytes, cut to 128 bits. A writer puts an item again only when
//! the newest record of its slot isn't the same item close behind ([TrackRepeat::State](crate::TrackRepeat::State)'s
//! `near`); a reader holding an item of that hash takes the record as nothing new. Two items colliding in 128 bits
//! (about 2^64 items before one pair is likely) would make a writer skip writing the second one where the first is
//! close behind in the same slot: the slot then reads as the first item.
//!
//! The index lists every state record by its slot and hash ([crate::index]), so a reader finds the item in force at a
//! place, or an item by its hash, without a scan.
//!
//! **Keyed state.** A keyed state track holds many slots, one per key: bytes of at most [STATE_KEY_LEN_MAX]. Its
//! records sit in blocks of their own, marked [FLAG_KEYED](crate::FLAG_KEYED), and each item is a [KeyedItem]: the
//! key, then the value, or no value for a deletion, which ends the key:
//!
//! ```text
//! uvar head: key length << 1 | 1 for a deletion; the length at most STATE_KEY_LEN_MAX (1024)
//! key bytes
//! value bytes, the rest of the item: none in a deletion
//! ```
//!
//! The index lists a keyed record by its
//! [KeyId]: the key's first [KEY_HEAD_LEN] bytes, its length and a 64-bit hash of the whole key. Ordered by the
//! head first, the index lists a key range in order, and a key of at most [KEY_HEAD_LEN] bytes is whole in it.
//!
//! Two different keys of the same head, length and 64-bit hash share a slot. That happens by chance only about once
//! in 2^64 pairs, but a key set crafted for it costs about 2^32 BLAKE3 hashes. A lookup of one key
//! ([TapeReader::key_at](crate::reader::TapeReader::key_at)) reads the full key of each record and stays exact; a
//! listing ([TapeReader::keys_at](crate::reader::TapeReader::keys_at)) reads the newest record of each slot and so
//! shows only the newer of two keys that share one.

use shrink_wrap::prelude::*;

use crate::varint;
use crate::{BoundError, ClockNum, Describe, FromFile, RecordTrack, TrackNum};

/// Longest key of a keyed state track, in bytes: a store's path limit.
pub const STATE_KEY_LEN_MAX: usize = 1024;
/// Bytes of a key's head ([KeyId::head]) an index entry holds.
pub const KEY_HEAD_LEN: usize = 8;

/// How the index knows a key of a keyed state track: its head, its length and a hash of the whole key.
///
/// Ordered by head, then length, then hash: keys of at most [KEY_HEAD_LEN] bytes in byte order, longer keys grouped
/// by their head.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct KeyId {
    /// The key's first [KEY_HEAD_LEN] bytes, zero past its end.
    pub head: [u8; KEY_HEAD_LEN],
    /// The key's length in bytes, at most [STATE_KEY_LEN_MAX].
    pub len: u32,
    /// The first 8 bytes of BLAKE3 of the key, little endian.
    pub hash: u64,
}

impl KeyId {
    /// The id of `key`; None for a key longer than [STATE_KEY_LEN_MAX].
    pub fn of(key: &[u8]) -> Option<Self> {
        if key.len() > STATE_KEY_LEN_MAX {
            return None;
        }
        let mut head = [0u8; KEY_HEAD_LEN];
        for (d, s) in head.iter_mut().zip(key) {
            *d = *s;
        }
        let full = blake3::hash(key);
        let mut hash = [0u8; 8];
        for (d, s) in hash.iter_mut().zip(full.as_bytes()) {
            *d = *s;
        }
        Some(Self {
            head,
            len: u32::try_from(key.len()).ok()?,
            hash: u64::from_le_bytes(hash),
        })
    }

    /// The whole key, when it is at most [KEY_HEAD_LEN] bytes long: the index alone holds it.
    pub fn key(&self) -> Option<&[u8]> {
        self.head.get(..crate::from_file::u32_to_usize(self.len))
    }

    /// Whether a key of this id starts with `prefix`: Some when the id decides it, None when only the key can (a
    /// prefix longer than [KEY_HEAD_LEN] whose head matches).
    pub fn has_prefix(&self, prefix: &[u8]) -> Option<bool> {
        if crate::from_file::u32_to_usize(self.len) < prefix.len() {
            return Some(false);
        }
        let n = prefix.len().min(KEY_HEAD_LEN);
        let (known, rest) = prefix.split_at(n);
        if self.head.get(..n) != Some(known) {
            return Some(false);
        }
        if rest.is_empty() { Some(true) } else { None }
    }

    /// Whether the head is one a key of this length has: zero past the key's end, the length within
    /// [STATE_KEY_LEN_MAX].
    pub fn is_valid(&self) -> bool {
        let len = crate::from_file::u32_to_usize(self.len);
        len <= STATE_KEY_LEN_MAX && self.head.iter().skip(len).all(|b| *b == 0)
    }
}

/// Whether a record of `track` holding `item` may sit in a block marked [FLAG_KEYED](crate::FLAG_KEYED): a data
/// track's, its item a [KeyedItem].
pub(crate) fn keyed_record(track: RecordTrack, item: &[u8]) -> bool {
    matches!(track, RecordTrack::Data(_)) && KeyedItem::read(item).is_ok()
}

/// A keyed state record's item read: the key, then the value; no value for a deletion, which ends the key. The
/// bytes are in the [module docs](self).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeyedItem<'a> {
    pub key: &'a [u8],
    /// The value, None for a deletion.
    pub value: Option<&'a [u8]>,
}

/// Why a keyed state record's item doesn't read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyedError {
    /// The head doesn't read.
    Head,
    /// The key is longer than [STATE_KEY_LEN_MAX].
    KeyLength(BoundError),
    /// The key runs past the item.
    Short,
    /// A deletion with value bytes after its key.
    DeletionValue,
}

impl core::fmt::Display for KeyedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Head => f.write_str("a keyed item's head doesn't read"),
            Self::KeyLength(e) => write!(f, "a keyed item's key length: {e}"),
            Self::Short => f.write_str("a keyed item's key runs past the item"),
            Self::DeletionValue => f.write_str("a deletion carries a value"),
        }
    }
}

impl core::error::Error for KeyedError {}

impl<'a> KeyedItem<'a> {
    /// Read the keyed item `item`.
    pub fn read(item: &'a [u8]) -> Result<Self, KeyedError> {
        let mut rd = varint::Reader(item);
        let head = rd.uvar().ok_or(KeyedError::Head)?;
        let deleted = head & 1 != 0;
        let len = FromFile::new(head >> 1)
            .at_most(STATE_KEY_LEN_MAX)
            .map_err(KeyedError::KeyLength)?;
        let key = rd.take(len).ok_or(KeyedError::Short)?;
        let value = rd.0;
        if deleted {
            if !value.is_empty() {
                return Err(KeyedError::DeletionValue);
            }
            return Ok(Self { key, value: None });
        }
        Ok(Self {
            key,
            value: Some(value),
        })
    }

    /// Bytes of the item: its head, key and value.
    pub fn len(&self) -> usize {
        let head = self.head();
        crate::uvar_len(head)
            .saturating_add(self.key.len())
            .saturating_add(self.value.map_or(0, <[u8]>::len))
    }

    /// Whether the item has no bytes: never, the head takes one at least.
    pub fn is_empty(&self) -> bool {
        false
    }

    fn head(&self) -> u64 {
        let len = u64::try_from(self.key.len()).unwrap_or(u64::MAX);
        len.saturating_mul(2) | u64::from(self.value.is_none())
    }

    /// The item's bytes, appended to `out`; None when the key is longer than [STATE_KEY_LEN_MAX].
    #[cfg(feature = "alloc")]
    pub fn encode_into(&self, out: &mut alloc::vec::Vec<u8>) -> Option<()> {
        if self.key.len() > STATE_KEY_LEN_MAX {
            return None;
        }
        let mut head = [0u8; 10];
        let n = varint::put_uvar(&mut head, self.head())?;
        out.extend_from_slice(head.get(..n)?);
        out.extend_from_slice(self.key);
        out.extend_from_slice(self.value.unwrap_or_default());
        Some(())
    }

    /// The id of its key; None for a key longer than [STATE_KEY_LEN_MAX], which no item read has.
    pub fn id(&self) -> Option<KeyId> {
        KeyId::of(self.key)
    }
}

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
    /// A [Schema](crate::Schema) of an item encoding.
    Schema,
    /// A [Describe] variant this crate doesn't know, by its code: its discriminant plus 1, at most 15. A newer
    /// writer's code for it is the same, so a reader that replays the index agrees with the writer's entries.
    Unknown(u8),
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
            Describe::Schema(_) => Self::Schema,
            Describe::Unknown(d) => Self::Unknown(match u8::try_from(*d) {
                Ok(d) if d < 15 => d.saturating_add(1),
                _ => 15,
            }),
        }
    }

    /// Its code in an index entry: 1 to 15, a [Describe] variant's discriminant plus 1.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Producer => 1,
            Self::Track => 2,
            Self::Bundle => 3,
            Self::Clock => 4,
            Self::Map => 5,
            Self::Item => 6,
            Self::Layout => 7,
            Self::Schema => 8,
            Self::Unknown(code) => code,
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
            8 => Self::Schema,
            9..=15 => Self::Unknown(v),
            _ => return None,
        })
    }
}

/// The state a record is the newest of, see the [module docs](self).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// The item of state track `track` (1 and up).
    State {
        track: TrackNum,
    },
    Producer,
    Layout,
    /// The definition of track `number`.
    Track {
        number: TrackNum,
    },
    /// The definition of clock `number`.
    Clock {
        number: ClockNum,
    },
    /// The map from clock `from` to clock `to`.
    Map {
        from: ClockNum,
        to: ClockNum,
    },
    /// The key `key` of keyed state track `track`.
    Keyed {
        track: TrackNum,
        key: KeyId,
    },
    /// A bundle, a schema, a typed item or a definition of a kind this crate doesn't know: each item a slot of its own.
    Own {
        kind: DefinitionKind,
        hash: ItemHash,
    },
}

impl Slot {
    /// The slot of record `item` on `track`, in a block marked [FLAG_KEYED](crate::FLAG_KEYED) with `keyed`: on
    /// track 0 by the definition it decodes to, on a keyed track by its key; None when it doesn't decode, and for a
    /// record of track 0 in a keyed block.
    pub fn of(track: RecordTrack, item: &[u8], keyed: bool) -> Option<Self> {
        match (track, keyed) {
            (RecordTrack::Data(track), false) => return Some(Self::State { track }),
            (RecordTrack::Data(track), true) => {
                let key = KeyedItem::read(item).ok()?.id()?;
                return Some(Self::Keyed { track, key });
            }
            (RecordTrack::Definitions, true) => return None,
            (RecordTrack::Definitions, false) => {}
        }
        let d = Describe::from_ww_bytes(item).ok()?;
        Some(Self::of_definition(&d, || ItemHash::of(item)))
    }

    /// The slot of definition `d`; `hash` gives its item's hash, asked only for a slot of its own.
    pub fn of_definition(d: &Describe<'_>, hash: impl FnOnce() -> ItemHash) -> Self {
        match d {
            Describe::Producer(_) => Self::Producer,
            Describe::Layout(_) => Self::Layout,
            Describe::Track(t) => Self::Track { number: t.number },
            Describe::Clock(c) => Self::Clock { number: c.number },
            Describe::Map(m) => Self::Map {
                from: m.from,
                to: m.to,
            },
            _ => Self::Own {
                kind: DefinitionKind::of(d),
                hash: hash(),
            },
        }
    }

    /// Whether the slot is a definition's (track 0).
    pub fn is_definition(&self) -> bool {
        !matches!(self, Self::State { .. } | Self::Keyed { .. })
    }

    /// The track its records are on: the definitions for a definition.
    pub fn track(&self) -> RecordTrack {
        match self {
            Self::State { track } | Self::Keyed { track, .. } => RecordTrack::Data(*track),
            _ => RecordTrack::Definitions,
        }
    }

    #[cfg(feature = "alloc")]
    /// How an index entry lists the slot: its level code, number and second number; a key's head and hash go
    /// elsewhere ([KeyId]).
    pub(crate) fn code(&self) -> (u8, u32, u32) {
        match *self {
            Self::State { track } => (0, track.get(), 0),
            Self::Keyed { track, key } => (0, track.get(), key.len),
            Self::Producer => (DefinitionKind::Producer.to_u8(), 0, 0),
            Self::Layout => (DefinitionKind::Layout.to_u8(), 0, 0),
            Self::Track { number } => (DefinitionKind::Track.to_u8(), number.get(), 0),
            Self::Clock { number } => (DefinitionKind::Clock.to_u8(), number.get(), 0),
            Self::Map { from, to } => (DefinitionKind::Map.to_u8(), from.get(), to.get()),
            Self::Own { kind, .. } => (kind.to_u8(), 0, 0),
        }
    }

    /// The slot an index entry lists by `code`, with the item's `hash` and a keyed record's `key`; None for a level
    /// no slot has, a key on a definition, or a track or clock numbered 0.
    pub(crate) fn from_code(
        level: u8,
        number: u32,
        second: u32,
        hash: ItemHash,
        key: Option<KeyId>,
    ) -> Option<Self> {
        if level == 0 {
            let track = TrackNum::try_new(number)?;
            return Some(match key {
                Some(key) => Self::Keyed { track, key },
                None => Self::State { track },
            });
        }
        if key.is_some() {
            return None;
        }
        Some(match DefinitionKind::from_u8(level)? {
            DefinitionKind::Producer => Self::Producer,
            DefinitionKind::Layout => Self::Layout,
            DefinitionKind::Track => Self::Track {
                number: TrackNum::try_new(number)?,
            },
            DefinitionKind::Clock => Self::Clock {
                number: ClockNum::try_new(number)?,
            },
            DefinitionKind::Map => Self::Map {
                from: ClockNum::try_new(number)?,
                to: ClockNum::try_new(second)?,
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
            Slot::State {
                track: TrackNum::new(9),
            },
            Slot::Producer,
            Slot::Layout,
            Slot::Track {
                number: TrackNum::new(3),
            },
            Slot::Clock {
                number: ClockNum::new(2),
            },
            Slot::Map {
                from: ClockNum::new(2),
                to: ClockNum::new(1),
            },
            Slot::Own {
                kind: DefinitionKind::Bundle,
                hash,
            },
            Slot::Own {
                kind: DefinitionKind::Schema,
                hash,
            },
            Slot::Own {
                kind: DefinitionKind::Unknown(14),
                hash,
            },
        ];
        for s in slots {
            let (level, n, second) = s.code();
            assert_eq!(Slot::from_code(level, n, second, hash, None), Some(s));
        }
        assert_eq!(Slot::from_code(16, 0, 0, hash, None), None);
        let key = KeyId::of(b"/a/b").unwrap();
        let keyed = Slot::Keyed {
            track: TrackNum::new(4),
            key,
        };
        let (level, n, second) = keyed.code();
        assert_eq!(second, 4);
        assert_eq!(
            Slot::from_code(level, n, second, hash, Some(key)),
            Some(keyed)
        );
        assert_eq!(Slot::from_code(2, 1, 0, hash, Some(key)), None);
    }

    #[test]
    fn a_keyed_item_reads_back() {
        for (key, value) in [
            (&b""[..], Some(&b""[..])),
            (b"/x", Some(b"value")),
            (b"/x", None),
            (&[7u8; STATE_KEY_LEN_MAX][..], Some(b"v")),
        ] {
            let it = KeyedItem { key, value };
            let mut out = alloc::vec::Vec::new();
            it.encode_into(&mut out).unwrap();
            assert_eq!(out.len(), it.len());
            assert_eq!(KeyedItem::read(&out), Ok(it));
        }
        let long = [0u8; STATE_KEY_LEN_MAX + 1];
        let it = KeyedItem {
            key: &long,
            value: None,
        };
        assert_eq!(it.encode_into(&mut alloc::vec::Vec::new()), None);
    }

    #[test]
    fn hostile_keyed_items_are_refused() {
        assert_eq!(KeyedItem::read(&[]), Err(KeyedError::Head));
        assert_eq!(KeyedItem::read(&[0x80]), Err(KeyedError::Head));
        // a key of 1025 bytes: head 2050
        assert!(matches!(
            KeyedItem::read(&[0x82, 0x10]),
            Err(KeyedError::KeyLength(_))
        ));
        assert_eq!(KeyedItem::read(&[6, b'a']), Err(KeyedError::Short));
        assert_eq!(
            KeyedItem::read(&[3, b'a', b'v']),
            Err(KeyedError::DeletionValue)
        );
        let max = [0xff; 10];
        assert!(KeyedItem::read(&max).is_err());
    }

    #[test]
    fn key_ids_order_short_keys_as_bytes_and_decide_prefixes() {
        let ids: alloc::vec::Vec<KeyId> = [&b""[..], b"a", b"a\0", b"ab", b"b"]
            .iter()
            .map(|k| KeyId::of(k).unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ids[3].key(), Some(&b"ab"[..]));
        let long = KeyId::of(b"/eventfull/layouts/a").unwrap();
        assert_eq!(long.key(), None);
        assert_eq!(long.has_prefix(b"/eve"), Some(true));
        assert_eq!(long.has_prefix(b"/x"), Some(false));
        assert_eq!(long.has_prefix(b"/eventfull/"), None);
        assert_eq!(long.has_prefix(b"/eventfX/"), Some(false));
        assert_eq!(ids[1].has_prefix(b"ab"), Some(false));
        assert!(long.is_valid());
        let mut bad = ids[1];
        bad.head[3] = 1;
        assert!(!bad.is_valid());
        assert_eq!(KeyId::of(&[0; STATE_KEY_LEN_MAX + 1]), None);
    }

    #[test]
    fn the_hash_is_blake3_cut_to_128_bits() {
        let h = ItemHash::of(b"abc");
        assert_eq!(&h.0[..], &blake3::hash(b"abc").as_bytes()[..16]);
    }
}
