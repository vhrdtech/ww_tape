//! Raw device formats: little and big endian, values packed at any width LSB first or MSB first, floats of another
//! layout than their scalar's.
//!
//! An LSB-first column is the [bitpack] layout (horizontal, LSB first), so it packs and unpacks with the shared
//! primitive and its vector kernels; MSB-first columns have their own loop.
//!
//! The decoders take a column whose length [Samples::new](super::Samples::new) checked against its count; a range
//! past its bytes is still an error, never a panic.

use super::float::FloatLayout;
use super::{Sample, SampleError};
use crate::bitpack::{self, Width};

/// Widest MSB-first packing: a value is read from one 8-byte load at any bit offset within a byte.
pub(super) const MSB_MAX: u32 = 56;

/// How a packed column lays out its values: the [bitpack] layout at a width, or MSB first at a width.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum Packing {
    /// LSB first, 1 to 64 bits.
    Lsb(u32),
    /// MSB first, 1 to [MSB_MAX] bits.
    Msb(u32),
}

impl Packing {
    /// Bits per value.
    pub(super) const fn bits(self) -> u32 {
        match self {
            Self::Lsb(bits) | Self::Msb(bits) => bits,
        }
    }
}

/// Bytes of one value of `T`: its size, 1 to 8 for the scalars the byte-wide layouts code.
#[inline(always)]
const fn width<T: Sample>() -> usize {
    size_of::<T>()
}

/// The low `bits` bits set: none for 0, all for 64 and up.
#[inline(always)]
pub(super) const fn mask(bits: u32) -> u64 {
    // a shift by 64 (bits 0) is None: no bit set
    match u64::MAX.checked_shr(u64::BITS.saturating_sub(bits)) {
        Some(m) => m,
        None => 0,
    }
}

/// A little-endian number of up to 8 bytes.
#[inline(always)]
fn load_le(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    for (o, i) in w.iter_mut().zip(b) {
        *o = *i;
    }
    u64::from_le_bytes(w)
}

/// A big-endian number of up to 8 bytes.
#[inline(always)]
fn load_be(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    for (o, i) in w.iter_mut().rev().zip(b.iter().rev()) {
        *o = *i;
    }
    u64::from_be_bytes(w)
}

/// Up to 8 bytes from the start of `b`, the missing ones zero, as the first bytes of a big-endian word.
#[inline(always)]
fn load_be_front(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    for (o, i) in w.iter_mut().zip(b) {
        *o = *i;
    }
    u64::from_be_bytes(w)
}

/// Code `values` little endian into `out`, which is their bytes exactly; returns its length.
pub(super) fn encode_le<T: Sample>(values: &[T], out: &mut [u8]) -> usize {
    for (v, o) in values.iter().zip(out.chunks_exact_mut(width::<T>())) {
        for (d, s) in o.iter_mut().zip(v.to_bits().to_le_bytes()) {
            *d = s;
        }
    }
    out.len()
}

/// Code `values` big endian into `out`, which is their bytes exactly; returns its length.
pub(super) fn encode_be<T: Sample>(values: &[T], out: &mut [u8]) -> usize {
    for (v, o) in values.iter().zip(out.chunks_exact_mut(width::<T>())) {
        // the low bytes, the lowest last
        for (d, s) in o.iter_mut().rev().zip(v.to_bits().to_le_bytes()) {
            *d = s;
        }
    }
    out.len()
}

/// The bytes of `n` values of `T` from value `first` on.
fn values_bytes<T: Sample>(bytes: &[u8], first: usize, n: usize) -> Result<&[u8], SampleError> {
    let w = width::<T>();
    let from = first.checked_mul(w).ok_or(SampleError::OutOfRange)?;
    let len = n.checked_mul(w).ok_or(SampleError::OutOfRange)?;
    bytes
        .get(from..)
        .and_then(|b| b.get(..len))
        .ok_or(SampleError::OutOfRange)
}

pub(super) fn decode_le<T: Sample>(
    bytes: &[u8],
    first: usize,
    out: &mut [T],
) -> Result<(), SampleError> {
    let src = values_bytes::<T>(bytes, first, out.len())?;
    for (o, b) in out.iter_mut().zip(src.chunks_exact(width::<T>())) {
        *o = T::from_bits(load_le(b));
    }
    Ok(())
}

pub(super) fn decode_be<T: Sample>(
    bytes: &[u8],
    first: usize,
    out: &mut [T],
) -> Result<(), SampleError> {
    let src = values_bytes::<T>(bytes, first, out.len())?;
    for (o, b) in out.iter_mut().zip(src.chunks_exact(width::<T>())) {
        *o = T::from_bits(load_be(b));
    }
    Ok(())
}

/// Sign-extend the low `bits` of `v`, `bits` 1 to 64.
#[inline(always)]
pub(super) const fn sign_extend(v: u64, bits: u32) -> i64 {
    let sh = u64::BITS.wrapping_sub(bits);
    v.wrapping_shl(sh).cast_signed().wrapping_shr(sh)
}

pub(super) fn encode_packed<T: Sample>(
    values: &[T],
    packing: Packing,
    out: &mut [u8],
) -> Result<usize, SampleError> {
    let bits = packing.bits();
    let scalar_bits = T::SCALAR.bits();
    let signed = T::SCALAR.is_signed();
    let m = mask(bits);
    for (i, v) in values.iter().enumerate() {
        let raw = v.to_bits();
        let fits = if signed {
            sign_extend(raw, bits) == sign_extend(raw, scalar_bits)
        } else {
            raw <= m
        };
        if !fits {
            return Err(SampleError::ValueTooWide { index: i });
        }
    }
    let needed = out.len();
    if let Packing::Lsb(bits) = packing {
        Width::new(bits)
            .map_err(SampleError::Bound)?
            .pack(out, values.iter().map(|v| v.to_bits()))
            .or(Err(SampleError::Full { needed }))?;
        return Ok(needed);
    }
    pack_msb(out, values.iter().map(|v| v.to_bits() & m), bits)
        .ok_or(SampleError::Full { needed })?;
    Ok(needed)
}

/// Pack `vals` (each under `2^bits`, `bits` at most 56) MSB first into `out`, the last byte's spare bits zero;
/// None when `out` is too short.
fn pack_msb(out: &mut [u8], vals: impl Iterator<Item = u64>, bits: u32) -> Option<()> {
    // `pending` holds the `n` bits not yet written, n under 8 between values
    let mut pending = 0u64;
    let mut n = 0u32;
    let mut bytes = out.iter_mut();
    for v in vals {
        pending = pending.checked_shl(bits)? | v;
        n = n.checked_add(bits)?;
        while let Some(rest) = n.checked_sub(8) {
            let [byte, ..] = pending.checked_shr(rest)?.to_le_bytes();
            *bytes.next()? = byte;
            n = rest;
        }
        pending &= mask(n);
    }
    if n > 0 {
        let [byte, ..] = pending.checked_shl(8u32.checked_sub(n)?)?.to_le_bytes();
        *bytes.next()? = byte;
    }
    Some(())
}

pub(super) fn decode_packed<T: Sample>(
    bytes: &[u8],
    packing: Packing,
    signed: bool,
    first: usize,
    out: &mut [T],
) -> Result<(), SampleError> {
    // the values' bits end within the bytes
    let end_bits = first
        .checked_add(out.len())
        .zip(usize::try_from(packing.bits()).ok())
        .and_then(|(end, bits)| end.checked_mul(bits));
    let len_bits = bytes.len().checked_mul(8);
    if !matches!((end_bits, len_bits), (Some(e), Some(l)) if e <= l) {
        return Err(SampleError::OutOfRange);
    }
    let done = match (packing, signed) {
        (Packing::Lsb(bits), true) => unpack_lsb::<true, T>(bytes, bits, first, out),
        (Packing::Lsb(bits), false) => unpack_lsb::<false, T>(bytes, bits, first, out),
        // the device widths get their own code, the width a constant in it
        (Packing::Msb(10), false) => unpack_msb::<10, false, T>(bytes, first, out),
        (Packing::Msb(10), true) => unpack_msb::<10, true, T>(bytes, first, out),
        (Packing::Msb(12), false) => unpack_msb::<12, false, T>(bytes, first, out),
        (Packing::Msb(12), true) => unpack_msb::<12, true, T>(bytes, first, out),
        (Packing::Msb(24), false) => unpack_msb::<24, false, T>(bytes, first, out),
        (Packing::Msb(24), true) => unpack_msb::<24, true, T>(bytes, first, out),
        (Packing::Msb(bits), signed) => unpack_msb_at(bytes, bits, signed, first, out),
    };
    done.ok_or(SampleError::OutOfRange)
}

/// Code `values`, floats of `layout` stored as their own bits, LSB or MSB first, into `out` (their bytes exactly);
/// returns its length. A value the layout has no exact representation of is [SampleError::ValueTooWide].
pub(super) fn encode_float<T: Sample>(
    values: &[T],
    layout: FloatLayout,
    msb_first: bool,
    out: &mut [u8],
) -> Result<usize, SampleError> {
    let from = T::SCALAR;
    if let Some(index) = values
        .iter()
        .position(|v| layout.narrow(v.to_bits(), from).is_none())
    {
        return Err(SampleError::ValueTooWide { index });
    }
    let needed = out.len();
    let raws = values
        .iter()
        .map(|v| layout.narrow(v.to_bits(), from).unwrap_or(0));
    if msb_first {
        pack_msb(out, raws, layout.bits).ok_or(SampleError::Full { needed })?;
    } else {
        Width::new(layout.bits)
            .map_err(SampleError::Bound)?
            .pack(out, raws)
            .or(Err(SampleError::Full { needed }))?;
    }
    Ok(needed)
}

/// Decode floats of `layout` from value `first` on into `out`, each widened into `T` (f32 or f64) exactly.
pub(super) fn decode_float<T: Sample>(
    bytes: &[u8],
    layout: FloatLayout,
    msb_first: bool,
    first: usize,
    out: &mut [T],
) -> Result<(), SampleError> {
    let packing = if msb_first {
        Packing::Msb(layout.bits)
    } else {
        Packing::Lsb(layout.bits)
    };
    let to = T::SCALAR;
    // a valid lane's scalar holds every value of the layout: None never comes, and is still an error, not a value
    let unsupported = SampleError::Unsupported {
        codec: crate::Codec::RAW,
        lane: super::Lane {
            scalar: to,
            bits: layout.bits,
            msb_first,
            exponent_bits: layout.exponent_bits,
        },
    };
    let mut raw = [0u64; bitpack::GROUP];
    let mut at = first;
    for chunk in out.chunks_mut(bitpack::GROUP) {
        let raw = raw.get_mut(..chunk.len()).ok_or(SampleError::OutOfRange)?;
        decode_packed::<u64>(bytes, packing, false, at, raw)?;
        for (o, r) in chunk.iter_mut().zip(raw.iter()) {
            *o = T::from_bits(layout.widen(*r, to).ok_or(unsupported)?);
        }
        at = at.checked_add(chunk.len()).ok_or(SampleError::OutOfRange)?;
    }
    Ok(())
}

/// Unpack `out.len()` values of `bits` bits (1 to 24) from value `first` on in the [bitpack] layout: single reads up
/// to the next group of [bitpack::GROUP] values, whole groups with the vector kernels (a group starts on a byte:
/// `GROUP · bits` is a multiple of 8), single reads for the rest. None when the values overflow an offset.
fn unpack_lsb<const SIGNED: bool, T: Sample>(
    bytes: &[u8],
    bits: u32,
    first: usize,
    out: &mut [T],
) -> Option<()> {
    let g = bitpack::GROUP;
    let width = Width::new(bits).ok()?;
    let value = |raw: u64| {
        T::from_bits(if SIGNED {
            sign_extend(raw, bits).cast_unsigned()
        } else {
            raw
        })
    };
    let head = first
        .checked_next_multiple_of(g)?
        .checked_sub(first)?
        .min(out.len());
    let (head_out, rest) = out.split_at_mut(head);
    for (k, o) in head_out.iter_mut().enumerate() {
        *o = value(width.read(bytes, first.checked_add(k)?));
    }
    let mut at = first.checked_add(head)?;
    if bits > 32 {
        // a wide value: one read each, no vector kernel
        for (k, o) in rest.iter_mut().enumerate() {
            *o = value(width.read(bytes, at.checked_add(k)?));
        }
        return Some(());
    }
    let bits_usize = usize::try_from(bits).ok()?;
    // sign extension in 32 bits, which vectorises
    let sh = 32u32.checked_sub(bits)?;
    let mut group = [0u32; bitpack::GROUP];
    let mut chunks = rest.chunks_exact_mut(g);
    for chunk in &mut chunks {
        let from = at.checked_mul(bits_usize)? / 8;
        width.unpack_u32(bytes.get(from..)?, &mut group).ok()?;
        for (o, raw) in chunk.iter_mut().zip(group) {
            let v = if SIGNED {
                i64::from(raw.wrapping_shl(sh).cast_signed().wrapping_shr(sh)).cast_unsigned()
            } else {
                u64::from(raw)
            };
            *o = T::from_bits(v);
        }
        at = at.checked_add(g)?;
    }
    for (k, o) in chunks.into_remainder().iter_mut().enumerate() {
        *o = value(width.read(bytes, at.checked_add(k)?));
    }
    Some(())
}

/// Unpack `out.len()` values of `W` bits (at most [MSB_MAX]), MSB first, from value `first` on, the width a constant.
#[inline(always)]
fn unpack_msb<const W: u32, const SIGNED: bool, T: Sample>(
    bytes: &[u8],
    first: usize,
    out: &mut [T],
) -> Option<()> {
    unpack_msb_at(bytes, W, SIGNED, first, out)
}

/// Unpack `out.len()` values of `w` bits (at most [MSB_MAX]), MSB first, from value `first` on: an 8-byte load per
/// value where 8 bytes are left, a padded load in the last bytes. None past the end of `bytes`.
#[inline(always)]
fn unpack_msb_at<T: Sample>(
    bytes: &[u8],
    w: u32,
    signed: bool,
    first: usize,
    out: &mut [T],
) -> Option<()> {
    if w > MSB_MAX {
        return None;
    }
    let width = usize::try_from(w).ok()?;
    let drop = u64::BITS.checked_sub(w)?;
    let mut bit = first.checked_mul(width)?;
    for o in out.iter_mut() {
        let rest = bytes.get(bit / 8..)?;
        let word = match rest.first_chunk::<8>() {
            Some(b) => u64::from_be_bytes(*b),
            None => load_be_front(rest),
        };
        let sh = u32::try_from(bit % 8).ok()?;
        let v = word.wrapping_shl(sh).wrapping_shr(drop);
        *o = T::from_bits(if signed {
            sign_extend(v, w).cast_unsigned()
        } else {
            v
        });
        bit = bit.checked_add(width)?;
    }
    Some(())
}
