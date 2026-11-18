//! A relation's values in an events chunk ([ColumnKind::Relation], its param the relation's index in
//! [TrackDef::relations](crate::TrackDef::relations)): one u64 per item, read in place.
//!
//! Every relation column starts with its item count (u32, little endian), which a reader checks against the chunk's:
//! the values never depend on the chunk head alone. Two codecs follow. [STARTS] for values that never decrease: the
//! starts column's time mode, the first and the last value as order-keeping i64s, little endian, then the starts
//! column of those i64s, a line per group of 128 with residuals above it, or one period for values a fixed step
//! apart. [DURATIONS] for any values: per group the smallest (u64), a width and an offset, then the values above the
//! smallest.
//!
//! A reader checks a chunk's relation columns against its track's definition ([StoredRelations]) before it reads one:
//! a column per relation that keeps its values in one, none for another, a relation that never decreases in
//! [STARTS]. A damaged column reads by salvage as the time columns do ([RelationColumn::salvage]).

use core::ops::Range;

use super::packed::Durations;
use super::starts::Starts;
use super::{DURATIONS, ITEMS_MAX, STARTS, TimeMode, TimesError};
use crate::FromFile;
use crate::bitpack::GROUP;
#[cfg(feature = "std")]
use crate::chunk::ColumnSpec;
use crate::chunk::{Chunk, Codec, Column, ColumnKind};
use crate::{RELATIONS_MAX, RelationValues, TrackDef};

/// Which relations of a track keep their values in a column of its chunks and which never decrease, bit `i` for
/// relation `i` of [TrackDef::relations]: what a reader checks an events chunk's relation columns against.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredRelations {
    /// Relations with [RelationValues::Stored].
    pub stored: u16,
    /// Relations whose values never decrease ([RelationDef::monotone](crate::RelationDef::monotone)).
    pub monotone: u16,
}

impl StoredRelations {
    /// The relations of `track`; one that doesn't decode keeps no column.
    pub fn of(track: &TrackDef<'_>) -> Self {
        let mut out = StoredRelations::default();
        // at most RELATIONS_MAX (16): each has a bit
        for (i, r) in track.relations.iter().enumerate().take(RELATIONS_MAX) {
            let (Ok(r), Some(bit)) = (r, u32::try_from(i).ok().and_then(|i| 1u16.checked_shl(i)))
            else {
                continue;
            };
            if r.values == RelationValues::Stored {
                out.stored |= bit;
            }
            if r.monotone {
                out.monotone |= bit;
            }
        }
        out
    }

    /// Check the relation columns of `chunk` against these relations: a column for each stored relation and none for
    /// another, a monotone relation's in [STARTS]; [TimesError::Relation] names the first that isn't.
    pub fn check(&self, chunk: &Chunk<'_>) -> Result<(), TimesError> {
        let mut seen = 0u16;
        for c in chunk.columns().filter(|c| c.kind == ColumnKind::Relation) {
            let bad = TimesError::Relation { relation: c.param };
            let bit = 1u16
                .checked_shl(u32::from(c.param))
                .filter(|b| self.stored & b != 0 && seen & b == 0)
                .ok_or(bad)?;
            if self.monotone & bit != 0 && c.codec != STARTS {
                return Err(bad);
            }
            seen |= bit;
        }
        match self.stored & !seen {
            0 => Ok(()),
            // a u16's trailing zeros are at most 16
            missing => Err(TimesError::Relation {
                relation: u16::try_from(missing.trailing_zeros()).unwrap_or(u16::MAX),
            }),
        }
    }
}

/// Bytes of the item count every relation column starts with.
const COUNT_LEN: usize = 4;

/// Bytes of the head of a [STARTS] relation column after its count: time mode, first, last.
const HEAD_LEN: usize = 17;

/// The order-keeping i64 of a u64: its sign bit flipped.
pub(crate) const fn ordered(v: u64) -> i64 {
    (v ^ (1 << 63)).cast_signed()
}

/// The u64 of an [ordered] i64.
pub(crate) const fn unordered(v: i64) -> u64 {
    v.cast_unsigned() ^ (1 << 63)
}

#[derive(Copy, Clone, Debug)]
enum Repr<'b> {
    /// Values that never decrease, as [ordered] starts.
    Monotone(Starts<'b>),
    /// Any values.
    Any(Durations<'b>),
}

/// One relation's values in an events chunk, read in place. Reads never panic on any bytes; [Self::get] panics only
/// for an item past the end.
#[derive(Copy, Clone, Debug)]
pub struct RelationColumn<'b> {
    repr: Repr<'b>,
}

impl<'b> RelationColumn<'b> {
    /// The values of relation `relation` in the intact events chunk `chunk` of a track with `track`'s relations,
    /// every group record checked and a column in [STARTS] never decreasing, the chunk's relation columns checked against `track` first
    /// ([StoredRelations::check]); None when the track keeps no column for it.
    pub fn read(
        chunk: &Chunk<'b>,
        track: StoredRelations,
        relation: u16,
    ) -> Result<Option<Self>, TimesError> {
        track.check(chunk)?;
        Self::open(chunk, relation, |_| false)
    }

    /// [Self::read] of a chunk whose payload may be damaged ([Chunk::bad_segments]), as
    /// [EventTimes::salvage](super::EventTimes::salvage) reads its times: a column with a byte in a bad segment is
    /// read group by group, and the items of a group that doesn't hold up are suspect ([Self::is_suspect]).
    pub fn salvage(
        chunk: &Chunk<'b>,
        track: StoredRelations,
        relation: u16,
    ) -> Result<Option<Self>, TimesError> {
        track.check(chunk)?;
        Self::open(chunk, relation, |c| chunk.column_damaged(c))
    }

    /// The values of relation `relation`, its column read in salvage where `damaged` says; columns unchecked against
    /// the track (a writer checked its own).
    pub(crate) fn open(
        chunk: &Chunk<'b>,
        relation: u16,
        damaged: impl Fn(&Column<'b>) -> bool,
    ) -> Result<Option<Self>, TimesError> {
        let head = chunk.head();
        if head.kind != crate::BlockKind::Events {
            return Err(TimesError::NotEvents);
        }
        let mut found = None;
        for c in chunk.columns() {
            if c.kind == ColumnKind::Relation && c.param == relation && found.replace(c).is_some() {
                return Err(TimesError::Unexpected(ColumnKind::Relation));
            }
        }
        let Some(c) = found else { return Ok(None) };
        let salvage = damaged(&c);
        let n = FromFile::new(head.count)
            .at_most(ITEMS_MAX)
            .map_err(TimesError::Bound)?;
        let corrupt = |what| TimesError::Corrupt {
            kind: ColumnKind::Relation,
            what,
        };
        let payload = chunk.payload();
        if c.codec != STARTS && c.codec != DURATIONS {
            return Err(TimesError::UnknownCodec(ColumnKind::Relation, c.codec));
        }
        let (count, bytes) = c
            .bytes
            .split_first_chunk::<COUNT_LEN>()
            .ok_or(corrupt("no count"))?;
        if u32::from_le_bytes(*count) != head.count {
            return Err(corrupt("an item count other than the chunk's"));
        }
        let offset = c.offset.saturating_add(COUNT_LEN);
        let repr = match c.codec {
            STARTS => {
                let (h, body) = bytes
                    .split_first_chunk::<HEAD_LEN>()
                    .ok_or(corrupt("no head"))?;
                let [
                    m,
                    f0,
                    f1,
                    f2,
                    f3,
                    f4,
                    f5,
                    f6,
                    f7,
                    l0,
                    l1,
                    l2,
                    l3,
                    l4,
                    l5,
                    l6,
                    l7,
                ] = *h;
                let mode = TimeMode::from_u8(m).ok_or(corrupt("unknown mode"))?;
                let first = i64::from_le_bytes([f0, f1, f2, f3, f4, f5, f6, f7]);
                let last = i64::from_le_bytes([l0, l1, l2, l3, l4, l5, l6, l7]);
                let tail = payload
                    .get(offset.saturating_add(HEAD_LEN)..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Monotone(Starts::parse(
                    ColumnKind::Relation,
                    n,
                    (first, last),
                    mode,
                    body,
                    tail,
                    salvage,
                )?)
            }
            DURATIONS => {
                let tail = payload
                    .get(offset..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Any(Durations::parse(
                    ColumnKind::Relation,
                    n,
                    u64::MAX,
                    bytes,
                    tail,
                    salvage,
                )?)
            }
            other => return Err(TimesError::UnknownCodec(ColumnKind::Relation, other)),
        };
        Ok(Some(RelationColumn { repr }))
    }

    /// Items in the chunk.
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Monotone(s) => s.len(),
            Repr::Any(d) => d.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The codec the column is in.
    pub fn codec(&self) -> Codec {
        match self.repr {
            Repr::Monotone(_) => STARTS,
            Repr::Any(_) => DURATIONS,
        }
    }

    /// The value of item `i`.
    ///
    /// # Panics
    ///
    /// When `i` is past the last item.
    pub fn get(&self, i: usize) -> u64 {
        match &self.repr {
            Repr::Monotone(s) => unordered(s.get(i)),
            Repr::Any(d) => d.get(i),
        }
    }

    /// Whether item `i` is in a group a salvage found damaged; its value is then a stand-in. False past the last
    /// item.
    pub fn is_suspect(&self, i: usize) -> bool {
        let g = i / GROUP;
        match &self.repr {
            Repr::Monotone(s) => s.is_suspect_group(g),
            Repr::Any(d) => d.is_suspect_group(g),
        }
    }

    /// The item ranges of the groups a salvage found damaged, ascending.
    pub fn suspect_items(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let n = self.len();
        (0..n.div_ceil(GROUP))
            .map(|g| g.saturating_mul(GROUP))
            .filter(|start| self.is_suspect(*start))
            .map(move |start| start..start.saturating_add(GROUP).min(n))
    }

    /// The smallest and the largest value: the first and the last for a column in [STARTS], else from the values.
    pub fn range(&self) -> (u64, u64) {
        match &self.repr {
            Repr::Monotone(s) => (unordered(s.first_tick()), unordered(s.last_tick())),
            Repr::Any(d) => (0..d.len()).fold((u64::MAX, 0), |(lo, hi), i| {
                let v = d.get(i);
                (lo.min(v), hi.max(v))
            }),
        }
    }

    /// The first item whose value is at least `v`, by search; [Self::len] when none is. None for a column not in
    /// [STARTS], whose values may decrease.
    pub fn lower_bound(&self, v: u64) -> Option<usize> {
        match &self.repr {
            Repr::Monotone(s) => Some(s.lower_bound(ordered(v))),
            Repr::Any(_) => None,
        }
    }
}

/// A relation's values encoded as a column of an events chunk.
#[cfg(feature = "std")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRelation {
    relation: u16,
    codec: Codec,
    bytes: std::vec::Vec<u8>,
}

#[cfg(feature = "std")]
impl EncodedRelation {
    /// Encode relation `relation`'s `values`, one per item: in [STARTS] when they never decrease, else in
    /// [DURATIONS]; a `monotone` relation whose values decrease is refused, naming the item.
    pub fn encode(
        relation: u16,
        values: &[u64],
        monotone: bool,
    ) -> Result<Self, super::EncodeError> {
        use super::EncodeError;
        if values.is_empty() {
            return Err(EncodeError::Empty);
        }
        if values.len() > ITEMS_MAX {
            return Err(EncodeError::TooMany);
        }
        // at most ITEMS_MAX, which a u32 holds
        let count = u32::try_from(values.len()).map_err(|_| EncodeError::TooMany)?;
        let decrease = values
            .iter()
            .zip(values.iter().skip(1))
            .position(|(a, b)| b < a)
            .map(|i| i.saturating_add(1));
        let (codec, bytes) = match decrease {
            Some(at) if monotone => return Err(EncodeError::Unsorted { at }),
            Some(_) => {
                let body = super::packed::encode_durations(values).ok_or(EncodeError::TooMany)?;
                let mut bytes = std::vec::Vec::with_capacity(COUNT_LEN.saturating_add(body.len()));
                bytes.extend_from_slice(&count.to_le_bytes());
                bytes.extend_from_slice(&body);
                (DURATIONS, bytes)
            }
            None => {
                let ticks: std::vec::Vec<i64> = values.iter().map(|v| ordered(*v)).collect();
                let (mode, body) = super::starts::encode(&ticks).ok_or(EncodeError::TooMany)?;
                let (Some(first), Some(last)) = (ticks.first(), ticks.last()) else {
                    return Err(EncodeError::Empty);
                };
                let mut bytes =
                    std::vec::Vec::with_capacity((COUNT_LEN + HEAD_LEN).saturating_add(body.len()));
                bytes.extend_from_slice(&count.to_le_bytes());
                bytes.push(mode.to_u8());
                bytes.extend_from_slice(&first.to_le_bytes());
                bytes.extend_from_slice(&last.to_le_bytes());
                bytes.extend_from_slice(&body);
                (STARTS, bytes)
            }
        };
        Ok(EncodedRelation {
            relation,
            codec,
            bytes,
        })
    }

    /// The column with its bytes, for [write_chunk](crate::chunk::write_chunk) and
    /// [TapeWriter::write_chunk](crate::TapeWriter::write_chunk).
    pub fn column(&self) -> (ColumnSpec, &[u8]) {
        let spec = ColumnSpec {
            kind: ColumnKind::Relation,
            codec: self.codec,
            param: self.relation,
            len: self.bytes.len(),
        };
        (spec, &self.bytes)
    }
}
