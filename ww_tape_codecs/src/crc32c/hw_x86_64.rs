//! The SSE4.2 kernel: the `crc32` instruction, three independent accumulators going at once so the CPU doesn't
//! wait on one accumulator's latency between words, recombined with [super::combine_with_shift] once the shift is
//! computed ([super::combine_shift]): no second, hardware-only table, the same GF(2) shift [super] already
//! carries for segment CRCs.

#![expect(
    unsafe_code,
    reason = "core::arch calls into a target_feature function, the same shape as crate::bitpack's AVX2 kernel"
)]
#![warn(clippy::undocumented_unsafe_blocks)]

use core::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

use super::{combine_shift, combine_with_shift};

/// Words (8 B) a lane must hold before splitting into three pays for interleaving's own cost (two
/// [combine_with_shift] calls, one [combine_shift]). Measured on this crate's reference x86-64 box (an AMD EPYC
/// Genoa, `force_serial` vs `force_interleaved` at 1024 B to 16 KiB, averaged over 20000 calls): the two cross
/// around 1.5 KiB, noisily, and interleaving wins with a clear, growing margin from 2304 B (96 words) on, where
/// readers' segments (down to 4 KiB) mostly land.
const MIN_LANE_WORDS: usize = 96;

/// Proof this CPU has SSE4.2, made only by [Token::detect]: a [super::Digest] on the SSE4.2 kernel holds one for
/// its whole life, so [append] takes it in place of checking the CPU again on every call.
#[derive(Copy, Clone)]
pub(super) struct Token(());

impl Token {
    /// Detects SSE4.2 once; `None` when this CPU lacks it.
    pub(super) fn detect() -> Option<Self> {
        if super::has_sse42() {
            Some(Self(()))
        } else {
            None
        }
    }
}

/// `crc32c_append(crc, data)` ([crc::Digest::update]'s running value) on this CPU's SSE4.2 instruction.
pub(super) fn append(_token: Token, crc: u32, data: &[u8]) -> u32 {
    let lane_words = data.len() / 8 / 3;
    if lane_words < MIN_LANE_WORDS {
        // SAFETY: _token proves the CPU has SSE4.2.
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
    // SAFETY: _token proves the CPU has SSE4.2.
    let (lane0, lane1, lane2) = unsafe { interleave3(crc, a, b, c) };
    #[expect(
        clippy::as_conversions,
        reason = "lane_bytes is at most data.len(), which fits a u64 (a block is at most MAX_BLOCK_LEN)"
    )]
    let len_b = lane_bytes as u64;
    let shift = combine_shift(len_b);
    let combined = combine_with_shift(shift, combine_with_shift(shift, lane0, lane1), lane2);
    // SAFETY: _token proves the CPU has SSE4.2.
    unsafe { append_serial(combined, tail) }
}

/// One accumulator continuing from the visible CRC-32C `crc`: `!crc` is its value in the LFSR's own domain
/// (zlib's convention, the same one [super::crc32c_combine]'s GF(2) math assumes), one `crc32` instruction per
/// 8-byte word then per leftover byte, complemented back on return.
///
/// # Safety
///
/// The CPU must have SSE4.2.
#[target_feature(enable = "sse4.2")]
unsafe fn append_serial(crc: u32, data: &[u8]) -> u32 {
    let mut r = !crc;
    let (words, rem) = data.as_chunks::<8>();
    // _mm_crc32_u64/_mm_crc32_u8 need no unsafe block here: both are `sse4.2`-gated, which this function already is.
    for &w in words {
        r = low32(_mm_crc32_u64(u64::from(r), u64::from_le_bytes(w)));
    }
    for &b in rem {
        r = _mm_crc32_u8(r, b);
    }
    !r
}

/// Three same-length (a multiple of 8), independent lanes run in lockstep so the CPU issues all three `crc32`
/// instructions of a step without waiting on each other's latency: lane 0 continues from the visible `crc`, lanes
/// 1 and 2 start fresh, each the CRC [super::crc32c_combine] expects of the pieces it joins.
///
/// # Safety
///
/// The CPU must have SSE4.2.
#[target_feature(enable = "sse4.2")]
unsafe fn interleave3(crc: u32, a: &[u8], b: &[u8], c: &[u8]) -> (u32, u32, u32) {
    let (mut r0, mut r1, mut r2) = (!crc, !0u32, !0u32);
    let (wa, _) = a.as_chunks::<8>();
    let (wb, _) = b.as_chunks::<8>();
    let (wc, _) = c.as_chunks::<8>();
    for ((&wa, &wb), &wc) in wa.iter().zip(wb).zip(wc) {
        // No unsafe block needed here either, as in append_serial.
        r0 = low32(_mm_crc32_u64(u64::from(r0), u64::from_le_bytes(wa)));
        r1 = low32(_mm_crc32_u64(u64::from(r1), u64::from_le_bytes(wb)));
        r2 = low32(_mm_crc32_u64(u64::from(r2), u64::from_le_bytes(wc)));
    }
    (!r0, !r1, !r2)
}

/// The low 32 bits of a `crc32` instruction's 64-bit result: its high bits are 0.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "_mm_crc32_u64 returns a 32-bit value zero-extended to 64"
)]
fn low32(x: u64) -> u32 {
    x as u32
}
