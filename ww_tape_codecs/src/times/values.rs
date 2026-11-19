//! A column of one u64 per item, read in place: a relation's values in a tape.
//!
//! Every value column starts with its item count (u32, little endian), which a reader checks against the count it
//! expects: the values never depend on anything outside the column alone. Two codecs follow. [Codec::STARTS] for
//! values that never decrease: the starts column's time mode, the first and the last value as order-keeping i64s,
//! little endian, then the starts column of those i64s, a line per group of 128 with residuals above it, or one
//! period for values a fixed step apart. [Codec::DURATIONS] for any values: per group the smallest (u64), a width and
//! an offset, then the values above the smallest.

use core::ops::Range;

use super::packed::Durations;
use super::starts::Starts;
use super::{ColumnError, TimeMode};
use crate::Codec;
use crate::bitpack::GROUP;

/// Bytes of the item count every value column starts with.
const COUNT_LEN: usize = 4;

/// Bytes of the head of a [Codec::STARTS] value column after its count: time mode, first, last.
const HEAD_LEN: usize = 17;

/// The order-keeping i64 of a u64: its sign bit flipped.
pub const fn ordered(v: u64) -> i64 {
    (v ^ (1 << 63)).cast_signed()
}

/// The u64 of an [ordered] i64.
pub const fn unordered(v: i64) -> u64 {
    v.cast_unsigned() ^ (1 << 63)
}

#[derive(Clone, Debug)]
enum Repr<'b> {
    /// Values that never decrease, as [ordered] starts.
    Monotone(Starts<'b>),
    /// Any values.
    Any(Durations<'b>),
}

/// One u64 per item, read in place. Reads never panic on any bytes; [Self::get] panics only for an item past the
/// end.
#[derive(Clone, Debug)]
pub struct Values<'b> {
    repr: Repr<'b>,
}

impl<'b> Values<'b> {
    /// Read a value column of `n` items in `codec`; `column` is its bytes, `tail` everything from its first byte on
    /// (the unpack kernels read past a group). Every group record is checked and a column in [Codec::STARTS] never
    /// decreases; with `salvage`, group records are checked one by one as read instead, and the items of a group
    /// that doesn't hold up are suspect ([Self::is_suspect]).
    pub fn parse(
        codec: Codec,
        n: usize,
        column: &'b [u8],
        tail: &'b [u8],
        salvage: bool,
    ) -> Result<Self, ColumnError> {
        let corrupt = ColumnError::Corrupt;
        let (count, bytes) = column
            .split_first_chunk::<COUNT_LEN>()
            .ok_or(corrupt("no count"))?;
        if usize::try_from(u32::from_le_bytes(*count)).ok() != Some(n) {
            return Err(corrupt("an item count other than the one asked for"));
        }
        let repr = match codec {
            Codec::STARTS => {
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
                let tail = tail
                    .get(COUNT_LEN + HEAD_LEN..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Monotone(Starts::parse(n, (first, last), mode, body, tail, salvage)?)
            }
            Codec::DURATIONS => {
                let tail = tail
                    .get(COUNT_LEN..)
                    .ok_or(corrupt("column past the payload"))?;
                Repr::Any(Durations::parse(n, u64::MAX, bytes, tail, salvage)?)
            }
            other => return Err(ColumnError::UnknownCodec(other)),
        };
        Ok(Values { repr })
    }

    /// Items in the column.
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
            Repr::Monotone(_) => Codec::STARTS,
            Repr::Any(_) => Codec::DURATIONS,
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

    /// The smallest and the largest value: the first and the last for a column in [Codec::STARTS], else from the
    /// values.
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
    /// [Codec::STARTS], whose values may decrease.
    pub fn lower_bound(&self, v: u64) -> Option<usize> {
        match &self.repr {
            Repr::Monotone(s) => Some(s.lower_bound(ordered(v))),
            Repr::Any(_) => None,
        }
    }
}

/// Encode `values`, one per item, in [Codec::STARTS] when they never decrease, else in [Codec::DURATIONS]: the codec
/// and the column's bytes. None for no values, more than a u32 counts, or a column past u32 offsets.
#[cfg(feature = "alloc")]
pub fn encode_values(values: &[u64]) -> Option<(Codec, alloc::vec::Vec<u8>)> {
    use alloc::vec::Vec;
    let count = u32::try_from(values.len()).ok()?;
    let (first, last) = (values.first()?, values.last()?);
    let ascending = values
        .iter()
        .zip(values.iter().skip(1))
        .all(|(a, b)| a <= b);
    if !ascending {
        let body = super::packed::encode_durations(values)?;
        let mut bytes = Vec::with_capacity(COUNT_LEN.checked_add(body.len())?);
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&body);
        return Some((Codec::DURATIONS, bytes));
    }
    let ticks: Vec<i64> = values.iter().map(|v| ordered(*v)).collect();
    let (mode, body) = super::starts::encode_starts(&ticks)?;
    let mut bytes = Vec::with_capacity((COUNT_LEN + HEAD_LEN).checked_add(body.len())?);
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.push(mode.to_u8());
    bytes.extend_from_slice(&ordered(*first).to_le_bytes());
    bytes.extend_from_slice(&ordered(*last).to_le_bytes());
    bytes.extend_from_slice(&body);
    Some((Codec::STARTS, bytes))
}
