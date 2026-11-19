//! The payload lengths column: a record per group of 128 items (base, minimum, width, offset), then each group's
//! lengths less its minimum, packed at the group's width.
//!
//! [Lengths] reads a column in place over its bytes, an item count and the payload bytes' total; [encode_lengths]
//! writes one. A group's lengths are in one form only (minimum the shortest, width the bits the largest packed value
//! needs, spare bits 0), and a reader takes no other.

use crate::FromFile;
use crate::bitpack::{self, GROUP, Width};
use crate::from_file::u32_to_usize;
use crate::times::ColumnError;
#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::ops::Range;

/// Bytes of a group record of the lengths column.
pub const RECORD_LEN: usize = 13;

/// Widest packed length, in bits: lengths are u32.
const WIDTH_MAX: u8 = 32;

/// A group record of the lengths column, its values bounded: width at most 32, offset inside the packed lengths,
/// base and minimum inside the payload bytes.
#[derive(Copy, Clone, Debug)]
struct Record {
    base: usize,
    min: usize,
    width: Width,
    offset: usize,
}

/// Where a group's payloads start and its shortest payload, as [Lengths::unpack] gives them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Where the group's first payload starts in the payload bytes.
    pub base: usize,
    /// The group's shortest payload, added to each unpacked length.
    pub min: usize,
}

/// The lengths column: records and packed lengths, read in place.
#[derive(Copy, Clone, Debug)]
pub struct Lengths<'b> {
    /// Items in the column.
    n: usize,
    /// One record per group.
    records: &'b [[u8; RECORD_LEN]],
    /// The packed lengths and everything after them in the payload, for the unpack kernels' reach.
    data: &'b [u8],
    /// Bytes of packed lengths in the column.
    data_len: usize,
    /// Bytes of payloads; None when they are not known (an lz4 column that doesn't decode).
    total: Option<usize>,
}

impl<'b> Lengths<'b> {
    /// Split `column` (its bytes; `tail` the bytes from the column's start on) into records and packed lengths of
    /// `n` items, the payload bytes' total not yet known ([Self::with_total]).
    pub fn split(n: usize, column: &'b [u8], tail: &'b [u8]) -> Result<Self, ColumnError> {
        let corrupt = ColumnError::Corrupt("shorter than its group records");
        let (all, _) = column.as_chunks::<RECORD_LEN>();
        let records = all.get(..n.div_ceil(GROUP)).ok_or(corrupt)?;
        let head = records.as_flattened().len();
        let data = tail.get(head..).ok_or(corrupt)?;
        let data_len = column.len().checked_sub(head).ok_or(corrupt)?;
        Ok(Lengths {
            n,
            records,
            data,
            data_len,
            total: None,
        })
    }

    /// The column for `total` bytes of payloads; None: not known, and no group holds up.
    #[must_use]
    pub fn with_total(self, total: Option<usize>) -> Self {
        Lengths { total, ..self }
    }

    /// Items in the column.
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether the column has no items.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Groups in the column.
    pub fn count(&self) -> usize {
        self.records.len()
    }

    /// Items in group `g`; 0 past the last group.
    fn group_len(&self, g: usize) -> usize {
        let first = g.saturating_mul(GROUP);
        self.n.saturating_sub(first).min(GROUP)
    }

    /// Group `g`'s record; what is out of bounds when it isn't one.
    fn checked_record(&self, g: usize) -> Result<Record, &'static str> {
        let r = self
            .records
            .get(g)
            .ok_or("shorter than its group records")?;
        let [b0, b1, b2, b3, m0, m1, m2, m3, w, o0, o1, o2, o3] = *r;
        let width = FromFile::new(w)
            .bounded(WIDTH_MAX)
            .ok()
            .and_then(|w| Width::new(u32::from(w)).ok())
            .ok_or("width over 32")?;
        let offset = FromFile::new(u32::from_le_bytes([o0, o1, o2, o3]))
            .within(self.data_len)
            .or(Err("packed lengths past the column"))?;
        let total = self.total.ok_or("payloads that don't decode")?;
        let past = "group base or minimum past the payload bytes";
        let base = FromFile::new(u32::from_le_bytes([b0, b1, b2, b3]))
            .within(total)
            .or(Err(past))?;
        let min = FromFile::new(u32::from_le_bytes([m0, m1, m2, m3]))
            .within(total)
            .or(Err(past))?;
        Ok(Record {
            base,
            min,
            width,
            offset,
        })
    }

    fn record(&self, g: usize) -> Option<Record> {
        self.checked_record(g).ok()
    }

    /// Where group `g`'s packed lengths end; None for a record out of bounds or an end past the column.
    fn packed_end(&self, g: usize) -> Option<usize> {
        let r = self.record(g)?;
        let end = r
            .offset
            .checked_add(bitpack::packed_len(self.group_len(g), r.width.bits()))?;
        (end <= self.data_len).then_some(end)
    }

    /// Unpack group `g`'s lengths less its minimum into `out`; None when its record doesn't fit the column.
    pub fn unpack(&self, g: usize, out: &mut [u32; GROUP]) -> Option<Group> {
        self.unpack_record(g, out).map(|r| Group {
            base: r.base,
            min: r.min,
        })
    }

    /// [Self::unpack], with the whole record.
    fn unpack_record(&self, g: usize, out: &mut [u32; GROUP]) -> Option<Record> {
        self.packed_end(g)?;
        let r = self.record(g)?;
        if r.width == Width::ZERO {
            out.fill(0);
        } else {
            // checked_record keeps widths of at most 32, the kernels' widest
            r.width.unpack_u32(self.data.get(r.offset..)?, out).ok()?;
        }
        Some(r)
    }

    /// Bytes of group `g`'s payloads; None when its record doesn't fit the column or its lengths are not in the
    /// encoder's form.
    fn group_bytes(&self, g: usize) -> Option<usize> {
        let mut v = [0u32; GROUP];
        let r = self.unpack_record(g, &mut v)?;
        let len = self.group_len(g);
        let vals = v.get(..len)?;
        if r.width != Width::ZERO {
            let min = vals.iter().copied().min()?;
            let max = vals.iter().copied().max()?;
            let packed = bitpack::packed_len(len, r.width.bits());
            let spare = packed
                .checked_mul(8)?
                .checked_sub(len.checked_mul(u32_to_usize(r.width.bits()))?)?;
            let last = *self
                .data
                .get(r.offset.checked_add(packed)?.checked_sub(1)?)?;
            // the bits after the last value are 0
            let tail_zero =
                spare == 0 || last.checked_shr(8u32.checked_sub(spare_bits(spare)?)?)? == 0;
            if min != 0 || Width::of(u64::from(max)) != r.width || !tail_zero {
                return None;
            }
        }
        let packed = sum(vals)?;
        packed.checked_add(r.min.checked_mul(len)?)
    }

    /// Whether group `g`'s lengths end where group `g + 1`'s start and its payloads where group `g + 1`'s base says,
    /// the last group's at the column's end and at the payload bytes' end.
    fn ends_at_next(&self, g: usize) -> bool {
        let next = if g.saturating_add(1) < self.count() {
            self.record(g.saturating_add(1)).map(|r| (r.offset, r.base))
        } else {
            self.total.map(|t| (self.data_len, t))
        };
        let (Some((offset, base)), Some(end), Some(bytes), Some(r)) = (
            next,
            self.packed_end(g),
            self.group_bytes(g),
            self.record(g),
        ) else {
            return false;
        };
        end == offset && r.base.checked_add(bytes) == Some(base)
    }

    /// Whether group `g` starts where group `g - 1` ends, group 0 at 0.
    fn starts_at_previous(&self, g: usize) -> bool {
        match g.checked_sub(1) {
            None => self.record(0).is_some_and(|r| r.offset == 0 && r.base == 0),
            Some(p) => self.ends_at_next(p),
        }
    }

    /// Whether group `g`'s record fits the column and its neighbours, blaming a broken link on the group whose
    /// record broke it (as the time columns' groups do).
    pub fn group_fits(&self, g: usize) -> bool {
        if self.packed_end(g).is_none() {
            return false;
        }
        let next = g.saturating_add(1);
        self.ends_at_next(g)
            || (self.starts_at_previous(g) && next < self.count() && !self.ends_at_next(next))
    }

    /// Check every record against the column and the payload bytes.
    pub fn check(&self) -> Result<(), ColumnError> {
        if !self.starts_at_previous(0) {
            return Err(ColumnError::Corrupt("group 0 doesn't start at 0"));
        }
        for g in 0..self.count() {
            self.checked_record(g).map_err(ColumnError::Corrupt)?;
            if self.packed_end(g).is_none() {
                return Err(ColumnError::Corrupt("packed lengths past the column"));
            }
            if !self.ends_at_next(g) {
                return Err(ColumnError::Corrupt(
                    "a group doesn't end where the next starts, or the last at the column's end",
                ));
            }
        }
        Ok(())
    }

    /// Where item `i`'s payload lies in the payload bytes; None when its group's record doesn't fit the column.
    pub fn range(&self, i: usize) -> Option<Range<usize>> {
        let (g, j) = (i / GROUP, i % GROUP);
        let mut v = [0u32; GROUP];
        let r = self.unpack(g, &mut v)?;
        let before = sum(v.get(..j)?)?;
        let start = r
            .base
            .checked_add(r.min.checked_mul(j)?)?
            .checked_add(before)?;
        let len = r.min.checked_add(u32_to_usize(*v.get(j)?))?;
        Some(start..start.checked_add(len)?)
    }
}

/// Spare bits of a group's last byte: 1 to 7.
fn spare_bits(spare: usize) -> Option<u32> {
    u32::try_from(spare).ok().filter(|s| *s < 8)
}

/// The sum of packed lengths; None past usize.
fn sum(vals: &[u32]) -> Option<usize> {
    vals.iter()
        .try_fold(0usize, |a, x| a.checked_add(u32_to_usize(*x)))
}

/// The lengths column of `lens`; None when the payloads or the packed lengths don't fit u32 offsets.
#[cfg(feature = "alloc")]
pub fn encode_lengths(lens: &[u32]) -> Option<Vec<u8>> {
    let mut base = 0u32;
    crate::times::encode_groups(lens, |g, offset| {
        let min = g.iter().copied().fold(u32::MAX, u32::min);
        let max = g.iter().copied().fold(0, u32::max);
        let width = bitpack::width_of(u64::from(max.abs_diff(min)));
        let mut rec = [0u8; RECORD_LEN];
        crate::bytes::put_u32(&mut rec, 0, base)?;
        crate::bytes::put_u32(&mut rec, 4, min)?;
        crate::bytes::put(&mut rec, 8, [u8::try_from(width).ok()?])?;
        crate::bytes::put_u32(&mut rec, 9, offset)?;
        for l in g {
            base = base.checked_add(*l)?;
        }
        // every length is at least the minimum: the difference is exact
        let vals = g.iter().map(|l| u64::from(l.wrapping_sub(min))).collect();
        Some((rec, vals, width))
    })
}
