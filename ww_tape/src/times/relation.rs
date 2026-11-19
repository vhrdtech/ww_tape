//! A relation's values in an events chunk ([ColumnKind::Relation], its param the relation's index in
//! [TrackDef::relations](crate::TrackDef::relations)): one u64 per item, read in place.
//!
//! The column is a value column ([Values]): its item count, which a reader checks against the chunk's, then the
//! values in [STARTS] (never decreasing) or [DURATIONS] (any).
//!
//! A reader checks a chunk's relation columns against its track's definition ([StoredRelations]) before it reads one:
//! a column per relation that keeps its values in one, none for another, a relation that never decreases in
//! [STARTS]. A damaged column reads by salvage as the time columns do ([RelationColumn::salvage]).

use core::ops::Range;

use super::values::Values;
use super::{ColumnError, DURATIONS, ITEMS_MAX, STARTS, TimesError};
use crate::FromFile;
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

/// One relation's values in an events chunk, read in place. Reads never panic on any bytes; [Self::get] panics only
/// for an item past the end.
#[derive(Clone, Debug)]
pub struct RelationColumn<'b> {
    values: Values<'b>,
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
        let n = FromFile::new(head.count)
            .at_most(ITEMS_MAX)
            .map_err(TimesError::Bound)?;
        if c.codec != STARTS && c.codec != DURATIONS {
            return Err(TimesError::UnknownCodec(ColumnKind::Relation, c.codec));
        }
        let corrupt = |error| TimesError::Corrupt {
            kind: ColumnKind::Relation,
            error,
        };
        let tail = chunk
            .payload()
            .get(c.offset..)
            .ok_or(corrupt(ColumnError::Corrupt("column past the payload")))?;
        let values = Values::parse(c.codec, n, c.bytes, tail, damaged(&c)).map_err(corrupt)?;
        Ok(Some(RelationColumn { values }))
    }

    /// Items in the chunk.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The codec the column is in.
    pub fn codec(&self) -> Codec {
        self.values.codec()
    }

    /// The value of item `i`.
    ///
    /// # Panics
    ///
    /// When `i` is past the last item.
    pub fn get(&self, i: usize) -> u64 {
        self.values.get(i)
    }

    /// Whether item `i` is in a group a salvage found damaged; its value is then a stand-in. False past the last
    /// item.
    pub fn is_suspect(&self, i: usize) -> bool {
        self.values.is_suspect(i)
    }

    /// The item ranges of the groups a salvage found damaged, ascending.
    pub fn suspect_items(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        self.values.suspect_items()
    }

    /// The smallest and the largest value: the first and the last for a column in [STARTS], else from the values.
    pub fn range(&self) -> (u64, u64) {
        self.values.range()
    }

    /// The first item whose value is at least `v`, by search; [Self::len] when none is. None for a column not in
    /// [STARTS], whose values may decrease.
    pub fn lower_bound(&self, v: u64) -> Option<usize> {
        self.values.lower_bound(v)
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
        let decrease = values
            .iter()
            .zip(values.iter().skip(1))
            .position(|(a, b)| b < a)
            .map(|i| i.saturating_add(1));
        if let (Some(at), true) = (decrease, monotone) {
            return Err(EncodeError::Unsorted { at });
        }
        let (codec, bytes) = super::values::encode_values(values).ok_or(EncodeError::TooMany)?;
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
