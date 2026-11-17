//! The ARMv8 CRC extension kernel: the `crc32cx` instruction, three independent accumulators going at once, the
//! same structure as [super::hw_x86_64] (see its docs for the domain the combine math runs in).

#![expect(
    unsafe_code,
    reason = "core::arch calls into a target_feature function, the same shape as crate::bitpack's NEON kernel"
)]
#![warn(clippy::undocumented_unsafe_blocks)]

use core::arch::aarch64::{__crc32cb, __crc32cd};

use super::{combine_shift, combine_with_shift};

/// [super::hw_x86_64::MIN_LANE_WORDS]'s value, not (yet) its own measurement: no ARM CPU with the CRC extension
/// at hand to bench on. Worth a check on one; the combine cost is the same GF(2) step count on any CPU, but the
/// `crc32c*` instruction's own latency and throughput may not match SSE4.2's.
const MIN_LANE_WORDS: usize = 96;

/// As [super::hw_x86_64::Token], for the CRC extension.
#[derive(Copy, Clone)]
pub(super) struct Token(());

impl Token {
    /// Detects the CRC extension once; `None` when this CPU lacks it.
    pub(super) fn detect() -> Option<Self> {
        if super::has_crc32() {
            Some(Self(()))
        } else {
            None
        }
    }
}

/// `crc32c_append(crc, data)` on this CPU's CRC extension.
pub(super) fn append(_token: Token, crc: u32, data: &[u8]) -> u32 {
    let lane_words = data.len() / 8 / 3;
    if lane_words < MIN_LANE_WORDS {
        // SAFETY: _token proves the CPU has the CRC extension.
        return unsafe { append_serial(crc, data) };
    }
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "lane_words <= data.len() / 8: lane_bytes <= data.len(), 3 times over"
    )]
    let lane_bytes = lane_words * 8;
    let Some((a, rest)) = data.split_at_checked(lane_bytes) else {
        // SAFETY: _token proves it, as above.
        return unsafe { append_serial(crc, data) };
    };
    let Some((b, rest)) = rest.split_at_checked(lane_bytes) else {
        // SAFETY: _token proves it, as above.
        return unsafe { append_serial(crc, data) };
    };
    let Some((c, tail)) = rest.split_at_checked(lane_bytes) else {
        // SAFETY: _token proves it, as above.
        return unsafe { append_serial(crc, data) };
    };
    // SAFETY: _token proves the CPU has the CRC extension.
    let (lane0, lane1, lane2) = unsafe { interleave3(crc, a, b, c) };
    #[expect(
        clippy::as_conversions,
        reason = "lane_bytes is at most data.len(), which fits a u64 (a block is at most MAX_BLOCK_LEN)"
    )]
    let len_b = lane_bytes as u64;
    let shift = combine_shift(len_b);
    let combined = combine_with_shift(shift, combine_with_shift(shift, lane0, lane1), lane2);
    // SAFETY: _token proves the CPU has the CRC extension.
    unsafe { append_serial(combined, tail) }
}

/// As [super::hw_x86_64::append_serial].
///
/// # Safety
///
/// The CPU must have the CRC extension.
#[target_feature(enable = "crc")]
unsafe fn append_serial(crc: u32, data: &[u8]) -> u32 {
    let mut r = !crc;
    let (words, rem) = data.as_chunks::<8>();
    // __crc32cd/__crc32cb need no unsafe block here: both are `crc`-gated, which this function already is.
    for &w in words {
        r = __crc32cd(r, u64::from_le_bytes(w));
    }
    for &b in rem {
        r = __crc32cb(r, b);
    }
    !r
}

/// As [super::hw_x86_64::interleave3].
///
/// # Safety
///
/// The CPU must have the CRC extension.
#[target_feature(enable = "crc")]
unsafe fn interleave3(crc: u32, a: &[u8], b: &[u8], c: &[u8]) -> (u32, u32, u32) {
    let (mut r0, mut r1, mut r2) = (!crc, !0u32, !0u32);
    let (wa, _) = a.as_chunks::<8>();
    let (wb, _) = b.as_chunks::<8>();
    let (wc, _) = c.as_chunks::<8>();
    for ((&wa, &wb), &wc) in wa.iter().zip(wb).zip(wc) {
        // No unsafe block needed here either, as in append_serial.
        r0 = __crc32cd(r0, u64::from_le_bytes(wa));
        r1 = __crc32cd(r1, u64::from_le_bytes(wb));
        r2 = __crc32cd(r2, u64::from_le_bytes(wc));
    }
    (!r0, !r1, !r2)
}
