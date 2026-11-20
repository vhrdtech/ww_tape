//! CRC-32C (`CRC_32_ISCSI`), the polynomial every block uses.
//!
//! A software fallback is good on every target: `TABLE`, the `crc` crate's `Table<16>`, measured about 5.1 GiB/s on an
//! x86-64 EPYC Genoa. Where the CPU runs the instruction, a hardware path (SSE4.2 on x86-64, the CRC extension on
//! aarch64) keeps three independent accumulators going at once so the instruction's own latency doesn't serialize the
//! stream. They are recombined with the GF(2) shift [crc32c_combine] already uses for segment CRCs: no second table for
//! the fast path.
//!
//! Picked over a crate: `crc32c` (SSE4.2 and ARMv8, software fallback) needs `std` throughout (its `io` wrappers,
//! `is_x86_feature_detected!`), so it can't build for wasm32 or a `no_std` MCU target as this crate must; `crc-fast`
//! supports `no_std`, but as a generic many-algorithm engine it pulls in `hashbrown` and `spin` for a cache this
//! crate has no use for, and ships its own flash-sized shift tables. The instruction needs no table at all, and the
//! shift math is already here, so a hand-written kernel (the same shape as [crate::bitpack]'s AVX2 and NEON ones)
//! costs nothing extra: no new dependency, no unsafe beyond that shape.
//!
//! Run-time detection follows [crate::bitpack]: with `std`, `is_x86_feature_detected!` / `is_aarch64_feature_detected!`;
//! without it (wasm32, a `no_std` MCU build), the compile-time `target_feature` only, so a kernel that isn't enabled
//! at build time is simply not compiled in and [Crc32c::digest] never picks it.

#[cfg(target_arch = "aarch64")]
mod hw_aarch64;
#[cfg(target_arch = "x86_64")]
mod hw_x86_64;
#[cfg(test)]
mod tests;

/// The polynomial of CRC-32C, reflected.
const POLY: u32 = 0x82f6_3b78;

/// `a · b` modulo the polynomial, both reflected (zlib's `multmodp`, over all 32 bits of `a`).
const fn multmodp(a: u32, mut b: u32) -> u32 {
    let mut p = 0u32;
    let mut m = 1u32 << 31;
    while m != 0 {
        if a & m != 0 {
            p ^= b;
        }
        m = m.wrapping_shr(1);
        b = if b & 1 != 0 {
            b.wrapping_shr(1) ^ POLY
        } else {
            b.wrapping_shr(1)
        };
    }
    p
}

/// `x^(2^k)` modulo the polynomial, k = 0..31: for this polynomial `x^(2^31) = x^(2^0)`, period 31, not 32 (checked
/// against plain square-and-multiply in `tests::x8nmodp_matches_square_and_multiply`, which is what caught the
/// previous period-32 table giving a wrong `x8nmodp` from `n = 2^29` on: at that bit, `k` reaches 32 and `k % 32`
/// pointed at `X2N[0]` instead of the correct `X2N[32 % 31] = X2N[1]`).
const X2N: [u32; 31] = {
    let mut t = [0u32; 31];
    let mut p = 1u32 << 30; // x^1
    let mut k = 0;
    while k < 31 {
        #[expect(clippy::indexing_slicing, reason = "k is under 31, the table's length")]
        let slot = &mut t[k];
        *slot = p;
        p = multmodp(p, p);
        k += 1;
    }
    t
};

/// `x^(8·n)` modulo the polynomial.
const fn x8nmodp(mut n: u64) -> u32 {
    let mut p = 1u32 << 31; // x^0
    // x^(2^k) repeats with period 31 in k (from k = 0): X2N is indexed modulo 31
    let mut k = 3usize;
    while n != 0 {
        if n & 1 != 0 {
            #[expect(
                clippy::indexing_slicing,
                reason = "k % 31 is under 31, the table's length"
            )]
            let x = X2N[k % 31];
            p = multmodp(x, p);
        }
        n = n.wrapping_shr(1);
        k = k.wrapping_add(1);
    }
    p
}

/// The shift [crc32c_combine] applies to `crc_a`, `len_b` bytes forward. The hardware kernels' three-way
/// interleave combines twice over the same `len_b` ([combine_with_shift]): computed once there instead of inside
/// each `crc32c_combine` call.
const fn combine_shift(len_b: u64) -> u32 {
    x8nmodp(len_b)
}

/// [crc32c_combine] with its shift already computed ([combine_shift]).
const fn combine_with_shift(shift: u32, crc_a: u32, crc_b: u32) -> u32 {
    multmodp(shift, crc_a) ^ crc_b
}

/// The CRC-32C of `A` then `B` from the CRC of `A`, the CRC of `B` (each computed from the start, as a fresh
/// `checksum` call does) and the length of `B`.
pub const fn crc32c_combine(crc_a: u32, crc_b: u32, len_b: u64) -> u32 {
    combine_with_shift(combine_shift(len_b), crc_a, crc_b)
}

/// The software fallback: the `crc` crate's 16-lane table, good on every target, the one implementation under
/// every block kind before a hardware kernel is available.
const TABLE: crc::Crc<u32, crc::Table<16>> =
    crc::Crc::<u32, crc::Table<16>>::new(&crc::CRC_32_ISCSI);

/// Which kernel a [Digest] ended up on: a tag the agreement test picks by hand ([digest_with]), never something
/// [Crc32c::digest] consults ahead of detecting.
///
/// A target with neither hardware kernel only ever makes [Kernel::Table].
#[cfg(test)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kernel {
    /// [TABLE], every target.
    Table,
    /// x86-64 SSE4.2 `crc32`.
    #[cfg(target_arch = "x86_64")]
    Sse42,
    /// aarch64 CRC extension `crc32c*`.
    #[cfg(target_arch = "aarch64")]
    Crc32,
}

#[cfg(all(target_arch = "x86_64", feature = "std"))]
fn has_sse42() -> bool {
    std::arch::is_x86_feature_detected!("sse4.2")
}

#[cfg(all(target_arch = "x86_64", not(feature = "std")))]
fn has_sse42() -> bool {
    cfg!(target_feature = "sse4.2")
}

#[cfg(all(target_arch = "aarch64", feature = "std"))]
fn has_crc32() -> bool {
    std::arch::is_aarch64_feature_detected!("crc")
}

#[cfg(all(target_arch = "aarch64", not(feature = "std")))]
fn has_crc32() -> bool {
    cfg!(target_feature = "crc")
}

/// A CRC-32C accumulator on the fastest kernel this CPU runs.
///
/// The kernel is detected once at [Crc32c::digest] and held for the accumulator's whole life as a `hw_x86_64::Token`
/// or `hw_aarch64::Token`, so [Digest::update] never checks the CPU again. It has the same `digest`/`checksum` shape
/// as [crc::Crc], so call sites take it in its place.
#[derive(Copy, Clone)]
pub struct Crc32c;

impl Crc32c {
    /// The CRC-32C of `bytes`.
    pub fn checksum(&self, bytes: &[u8]) -> u32 {
        let mut d = self.digest();
        d.update(bytes);
        d.finalize()
    }

    /// A fresh accumulator on the fastest kernel this CPU runs.
    pub fn digest(&self) -> Digest {
        #[cfg(target_arch = "x86_64")]
        if let Some(token) = hw_x86_64::Token::detect() {
            return Digest(State::Sse42(token, 0));
        }
        #[cfg(target_arch = "aarch64")]
        if let Some(token) = hw_aarch64::Token::detect() {
            return Digest(State::Crc32(token, 0));
        }
        Digest(State::Table(TABLE.digest()))
    }
}

/// [Crc32c::digest] on a given kernel, `None` when this CPU doesn't run it; the agreement test picks one by hand
/// the same way [Crc32c::digest] tries each in turn.
#[cfg(test)]
fn digest_with(kernel: Kernel) -> Option<Digest> {
    Some(match kernel {
        Kernel::Table => Digest(State::Table(TABLE.digest())),
        #[cfg(target_arch = "x86_64")]
        Kernel::Sse42 => Digest(State::Sse42(hw_x86_64::Token::detect()?, 0)),
        #[cfg(target_arch = "aarch64")]
        Kernel::Crc32 => Digest(State::Crc32(hw_aarch64::Token::detect()?, 0)),
    })
}

/// [Crc32c::digest]'s accumulator; [Digest::update] may be called more than once.
pub struct Digest(State);

enum State {
    Table(crc::Digest<'static, u32, crc::Table<16>>),
    #[cfg(target_arch = "x86_64")]
    Sse42(hw_x86_64::Token, u32),
    #[cfg(target_arch = "aarch64")]
    Crc32(hw_aarch64::Token, u32),
}

impl Digest {
    /// Feed more bytes into the accumulator.
    pub fn update(&mut self, bytes: &[u8]) {
        match &mut self.0 {
            State::Table(d) => d.update(bytes),
            #[cfg(target_arch = "x86_64")]
            State::Sse42(token, crc) => *crc = hw_x86_64::append(*token, *crc, bytes),
            #[cfg(target_arch = "aarch64")]
            State::Crc32(token, crc) => *crc = hw_aarch64::append(*token, *crc, bytes),
        }
    }

    /// The CRC-32C of every byte fed in.
    pub fn finalize(self) -> u32 {
        match self.0 {
            State::Table(d) => d.finalize(),
            #[cfg(target_arch = "x86_64")]
            State::Sse42(_, crc) => crc,
            #[cfg(target_arch = "aarch64")]
            State::Crc32(_, crc) => crc,
        }
    }
}
