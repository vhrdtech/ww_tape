//! A relation's values in an events chunk ([ColumnKind::Relation], its param the relation's index in
//! [TrackDef::relations](crate::TrackDef::relations)): one u64 per item, read in place.
//!
//! Two codecs. [STARTS] for values that never decrease: a head of 17 bytes (the starts column's time mode, then the
//! first and the last value as order-keeping i64s, little endian), then the starts column of those i64s, a line per
//! group of 128 with residuals above it, or one period for values a fixed step apart. [DURATIONS] for any values:
//! per group the smallest (u64), a width and an offset, then the values above the smallest.

use super::packed::Durations;
use super::starts::Starts;
use super::{DURATIONS, ITEMS_MAX, STARTS, TimeMode, TimesError};
use crate::FromFile;
#[cfg(feature = "std")]
use crate::chunk::ColumnSpec;
use crate::chunk::{Chunk, Codec, ColumnKind};

/// Bytes of the head of a [STARTS] relation column: time mode, first, last.
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
    /// The values of relation `relation` in the events chunk `chunk`, every group record checked; None when the
    /// chunk has no column for it.
    pub fn read(chunk: &Chunk<'b>, relation: u16) -> Result<Option<Self>, TimesError> {
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
        let corrupt = |what| TimesError::Corrupt {
            kind: ColumnKind::Relation,
            what,
        };
        let payload = chunk.payload();
        let repr = match c.codec {
            STARTS => {
                let (h, body) = c
                    .bytes
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
                    .get(c.offset.saturating_add(HEAD_LEN)..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Monotone(Starts::parse(
                    ColumnKind::Relation,
                    n,
                    (first, last),
                    mode,
                    body,
                    tail,
                    false,
                )?)
            }
            DURATIONS => {
                let tail = payload
                    .get(c.offset..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Any(Durations::parse(
                    ColumnKind::Relation,
                    n,
                    u64::MAX,
                    c.bytes,
                    tail,
                    false,
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
        let decrease = values
            .iter()
            .zip(values.iter().skip(1))
            .position(|(a, b)| b < a)
            .map(|i| i.saturating_add(1));
        let (codec, bytes) = match decrease {
            Some(at) if monotone => return Err(EncodeError::Unsorted { at }),
            Some(_) => (
                DURATIONS,
                super::packed::encode_durations(values).ok_or(EncodeError::TooMany)?,
            ),
            None => {
                let ticks: std::vec::Vec<i64> = values.iter().map(|v| ordered(*v)).collect();
                let (mode, body) = super::starts::encode(&ticks).ok_or(EncodeError::TooMany)?;
                let (Some(first), Some(last)) = (ticks.first(), ticks.last()) else {
                    return Err(EncodeError::Empty);
                };
                let mut bytes = std::vec::Vec::with_capacity(HEAD_LEN.saturating_add(body.len()));
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
