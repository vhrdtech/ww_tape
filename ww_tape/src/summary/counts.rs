//! Flag counts of an event tree: per node, how many of its items have each of the low flag bits set
//! ([EventsDef::flag_counts](crate::EventsDef::flag_counts)).
//!
//! In a block, the counts of each counted bit are one column after the node columns: a width byte `w` (bits per
//! count, 0 when every count is 0), then the node count's values of `w` bits each, least significant bit first, packed
//! from the low bit of each byte, the last byte's spare bits zero. `w` is the fewest bits that hold the column's
//! largest count, and at most the bits of the node size (`node_shift + 1`), so the format has one encoding.

use crate::FLAG_COUNTS_MAX;

/// Counted bits at most, as an array length.
#[expect(
    clippy::as_conversions,
    reason = "a u8 constant widened to usize in a const; From is not const"
)]
pub(super) const BITS: usize = FLAG_COUNTS_MAX as usize;

/// Per counted flag bit, how many items have it set.
///
/// Bits `0..known` are counted; the others are unknown, never 0: a track that doesn't count a bit, or a node merged
/// with one that doesn't, says nothing about it. Counts merge by sum, so they are exact at every level.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FlagCounts {
    known: u8,
    n: [u64; BITS],
}

impl FlagCounts {
    /// No bit counted.
    pub const UNKNOWN: FlagCounts = FlagCounts {
        known: 0,
        n: [0; BITS],
    };

    /// The counts of one item with `flags`: every bit up to [FLAG_COUNTS_MAX] counted, 1 where set.
    pub fn of_item(flags: u32) -> Self {
        let mut n = [0; BITS];
        for (j, c) in (0u32..).zip(&mut n) {
            *c = u64::from(flags.checked_shr(j).unwrap_or(0) & 1);
        }
        FlagCounts {
            known: FLAG_COUNTS_MAX,
            n,
        }
    }

    /// Counts of the first `known` bits (at most [FLAG_COUNTS_MAX]), `n[j]` for bit `j`; None past the max or with a
    /// count given for a bit past `known`.
    pub fn new(known: u8, n: [u64; BITS]) -> Option<Self> {
        if known > FLAG_COUNTS_MAX || n.iter().skip(usize::from(known)).any(|c| *c != 0) {
            return None;
        }
        Some(FlagCounts { known, n })
    }

    /// Bits counted: `0..known`.
    pub fn known(&self) -> u8 {
        self.known
    }

    /// Items with flag bit `bit` set; None when the bit isn't counted.
    pub fn get(&self, bit: u8) -> Option<u64> {
        if bit >= self.known {
            return None;
        }
        self.n.get(usize::from(bit)).copied()
    }

    /// These counts with only the first `known` bits kept (fewer when fewer are known).
    pub fn limited(self, known: u8) -> Self {
        let known = known.min(self.known);
        let mut n = self.n;
        n.iter_mut().skip(usize::from(known)).for_each(|c| *c = 0);
        FlagCounts { known, n }
    }

    /// Add the counts of `next`: the bits both count, summed. A sum past u64, which counts of at most their nodes'
    /// items never reach, leaves no bit known rather than a wrong count.
    pub fn merge(&mut self, next: &FlagCounts) {
        let known = self.known.min(next.known);
        let mut n = [0u64; BITS];
        for ((out, a), b) in n
            .iter_mut()
            .zip(&self.n)
            .zip(&next.n)
            .take(usize::from(known))
        {
            match a.checked_add(*b) {
                Some(s) => *out = s,
                None => {
                    *self = FlagCounts::UNKNOWN;
                    return;
                }
            }
        }
        *self = FlagCounts { known, n };
    }
}

/// Fewest bits that hold `v`.
pub(super) fn width_of(v: u64) -> u8 {
    // 64 - leading zeros: at most 64, so it fits a u8
    u8::try_from(u64::BITS.saturating_sub(v.leading_zeros())).unwrap_or(u8::MAX)
}

/// Bytes of `nodes` values of `width` bits; None past a usize.
pub(super) fn packed_len(nodes: usize, width: u8) -> Option<usize> {
    nodes
        .checked_mul(usize::from(width))?
        .checked_add(7)?
        .checked_div(8)
}

/// Bytes of one count column: its width byte and its values.
pub(super) fn column_len(nodes: usize, width: u8) -> Option<usize> {
    packed_len(nodes, width)?.checked_add(1)
}

/// Pack `values` at `width` bits each into `out` (exactly [packed_len] bytes, zero before); None when a value
/// needs more bits or `out` is short.
pub(super) fn pack(out: &mut [u8], values: impl Iterator<Item = u64>, width: u8) -> Option<()> {
    let mut at = 0usize;
    for v in values {
        if width_of(v) > width {
            return None;
        }
        for b in 0..u32::from(width) {
            if v.checked_shr(b)? & 1 == 1 {
                let byte = out.get_mut(at.checked_div(8)?)?;
                *byte |= 1u8.checked_shl(u32::try_from(at.checked_rem(8)?).ok()?)?;
            }
            at = at.checked_add(1)?;
        }
    }
    Some(())
}

/// Value `i` of `width` bits in `packed`; None past its end.
pub(super) fn unpack(packed: &[u8], width: u8, i: usize) -> Option<u64> {
    let w = usize::from(width);
    let from = i.checked_mul(w)?;
    let mut v = 0u64;
    for b in 0..w {
        let at = from.checked_add(b)?;
        let byte = packed.get(at.checked_div(8)?)?;
        let bit = byte.checked_shr(u32::try_from(at.checked_rem(8)?).ok()?)? & 1;
        v |= u64::from(bit).checked_shl(u32::try_from(b).ok()?)?;
    }
    Some(v)
}

/// Whether the bits of `packed` past `nodes` values of `width` bits are zero.
pub(super) fn padding_zero(packed: &[u8], nodes: usize, width: u8) -> bool {
    let used = nodes.saturating_mul(usize::from(width));
    let Some(last) = packed.last() else {
        return true;
    };
    match used.checked_rem(8) {
        Some(0) | None => true,
        Some(r) => last.checked_shr(u32::try_from(r).unwrap_or(8)).unwrap_or(0) == 0,
    }
}
