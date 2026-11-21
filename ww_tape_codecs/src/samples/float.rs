//! Floats of a layout other than f32 and f64: f16, bfloat16, fp8, a device's own. A value is stored as its own bits
//! and read into the lane's scalar (f32 or f64) exactly; a value written must be exactly representable in the layout.

use super::Scalar;

/// A float layout: a sign bit, `exponent_bits` of exponent, the mantissa in the rest of `bits`. The exponent's bias
/// is `2^(exponent_bits - 1) - 1`, all ones is infinity or NaN and all zeros a subnormal, as in IEEE 754.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct FloatLayout {
    /// Stored bits, 4 to 32, or 64 for the f64 layout.
    pub bits: u32,
    /// Exponent bits, 2 to 11, at most `bits - 2`.
    pub exponent_bits: u32,
}

/// The f32 layout.
const F32: FloatLayout = FloatLayout {
    bits: 32,
    exponent_bits: 8,
};

/// The f64 layout.
const F64: FloatLayout = FloatLayout {
    bits: 64,
    exponent_bits: 11,
};

/// The low `bits` bits set.
const fn mask(bits: u32) -> u64 {
    match u64::MAX.checked_shr(u64::BITS.saturating_sub(bits)) {
        Some(m) => m,
        None => 0,
    }
}

/// `v`, a number of `from` bits, as a number of `to` bits: shifted up, or down when the bits dropped are zero.
const fn shift_exact(v: u64, from: u32, to: u32) -> Option<u64> {
    if to >= from {
        v.checked_shl(to.wrapping_sub(from))
    } else {
        let dropped = from.wrapping_sub(to);
        if v & mask(dropped) != 0 {
            None
        } else {
            v.checked_shr(dropped)
        }
    }
}

impl FloatLayout {
    /// The layout of `scalar`, None for an integer.
    const fn of(scalar: Scalar) -> Option<Self> {
        match scalar {
            Scalar::F32 => Some(F32),
            Scalar::F64 => Some(F64),
            _ => None,
        }
    }

    /// Whether the layout is one this module converts exactly: 2 to 11 exponent bits, a mantissa bit, at most 32
    /// bits; or the f64 layout itself.
    pub(super) const fn is_valid(self) -> bool {
        self.exponent_bits >= 2
            && self.exponent_bits <= F64.exponent_bits
            && self.bits >= self.exponent_bits.saturating_add(2)
            && (self.bits <= 32 || (self.bits == 64 && self.exponent_bits == F64.exponent_bits))
    }

    /// The scalar a value is read into: f32 when f32 holds every value exactly, else f64.
    pub(super) const fn scalar(self) -> Scalar {
        if self.exponent_bits <= F32.exponent_bits && self.mantissa_bits() <= F32.mantissa_bits() {
            Scalar::F32
        } else {
            Scalar::F64
        }
    }

    const fn mantissa_bits(self) -> u32 {
        self.bits
            .saturating_sub(1)
            .saturating_sub(self.exponent_bits)
    }

    const fn bias(self) -> i64 {
        // exponent_bits is at most 11: the shift fits
        match 1i64.checked_shl(self.exponent_bits.saturating_sub(1)) {
            Some(b) => b.saturating_sub(1),
            None => 0,
        }
    }

    /// The bits of `raw`'s value (its low `bits` bits) in `to`, exact for the layout's [scalar](Self::scalar) and
    /// for f64; None for a scalar that doesn't hold the value, which no valid lane asks for.
    pub(super) fn widen(self, raw: u64, to: Scalar) -> Option<u64> {
        convert(raw, self, Self::of(to)?)
    }

    /// The low `bits` bits of the value `value` (the bits of a `from`, f32 or f64) in this layout; None when the
    /// layout has no exact representation of it.
    pub(super) fn narrow(self, value: u64, from: Scalar) -> Option<u64> {
        convert(value, Self::of(from)?, self)
    }
}

/// The bits of the value `raw` holds in `from` as a value of `to`; None when `to` has no exact representation of it.
/// Infinities stay, a NaN keeps its payload when `to` holds every bit of it, a subnormal of either layout is exact.
fn convert(raw: u64, from: FloatLayout, to: FloatLayout) -> Option<u64> {
    let (fm, fe) = (from.mantissa_bits(), from.exponent_bits);
    let (tm, te) = (to.mantissa_bits(), to.exponent_bits);
    let sign = (raw >> from.bits.saturating_sub(1)) & 1;
    let exp = (raw >> fm) & mask(fe);
    let mant = raw & mask(fm);
    let tsign = sign << to.bits.saturating_sub(1);
    let tinf = mask(te) << tm;
    if exp == mask(fe) {
        if mant == 0 {
            return Some(tsign | tinf);
        }
        let payload = shift_exact(mant, fm, tm)?;
        return (payload != 0).then_some(tsign | tinf | payload);
    }
    if exp == 0 && mant == 0 {
        return Some(tsign);
    }
    // a finite value: `sig · 2^(e - fm)`, the significand's leading bit explicit for a normal
    let (sig, e) = if exp == 0 {
        (mant, 1i64.saturating_sub(from.bias()))
    } else {
        let exp = i64::try_from(exp).ok()?;
        (mant | (1u64 << fm), exp.saturating_sub(from.bias()))
    };
    // normalised: `1.frac · 2^e2`, the top bit of `sig` at position `t`
    let t = i64::from(
        u64::BITS
            .saturating_sub(1)
            .saturating_sub(sig.leading_zeros()),
    );
    let e2 = e.saturating_add(t).saturating_sub(i64::from(fm));
    let tbias = to.bias();
    let emin = 1i64.saturating_sub(tbias);
    if e2 > tbias {
        return None;
    }
    if e2 >= emin {
        let t_bits = u32::try_from(t).ok()?;
        let frac = sig & mask(t_bits);
        let frac_t = shift_exact(frac, t_bits, tm)?;
        let field = u64::try_from(e2.saturating_add(tbias)).ok()?;
        return Some(tsign | (field << tm) | frac_t);
    }
    // a subnormal of `to`: the value as a multiple of `2^(emin - tm)`
    let shift = e
        .saturating_sub(i64::from(fm))
        .saturating_sub(emin.saturating_sub(i64::from(tm)));
    let m = if shift >= 0 {
        sig.checked_shl(u32::try_from(shift).ok()?)?
    } else {
        let down = u32::try_from(shift.saturating_neg()).ok()?;
        if down >= u64::BITS || sig & mask(down) != 0 {
            return None;
        }
        sig >> down
    };
    (m <= mask(tm)).then_some(tsign | m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const F16: FloatLayout = FloatLayout {
        bits: 16,
        exponent_bits: 5,
    };
    const BF16: FloatLayout = FloatLayout {
        bits: 16,
        exponent_bits: 8,
    };
    const E4M3: FloatLayout = FloatLayout {
        bits: 8,
        exponent_bits: 4,
    };
    const E5M2: FloatLayout = FloatLayout {
        bits: 8,
        exponent_bits: 5,
    };

    fn f32_of(l: FloatLayout, raw: u64) -> f32 {
        f32::from_bits(l.widen(raw, Scalar::F32).unwrap() as u32)
    }

    fn raw_of(l: FloatLayout, v: f32) -> Option<u64> {
        l.narrow(u64::from(v.to_bits()), Scalar::F32)
    }

    #[test]
    fn known_values_of_f16_widen() {
        assert_eq!(f32_of(F16, 0x3C00), 1.0);
        assert_eq!(f32_of(F16, 0xC100), -2.5);
        assert_eq!(f32_of(F16, 0x7BFF), 65504.0);
        assert_eq!(f32_of(F16, 0x0001), 2f32.powi(-24));
        assert_eq!(f32_of(F16, 0x03FF), 2f32.powi(-14) - 2f32.powi(-24));
        assert_eq!(f32_of(F16, 0x0400), 2f32.powi(-14));
        assert_eq!(f32_of(F16, 0x7C00), f32::INFINITY);
        assert_eq!(f32_of(F16, 0xFC00), f32::NEG_INFINITY);
        assert!(f32_of(F16, 0x7E00).is_nan());
        assert_eq!(
            f32_of(F16, 0x7C01).to_bits(),
            0x7F80_2000,
            "the NaN payload moves up"
        );
        assert_eq!(f32_of(F16, 0x8000).to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn known_values_of_bfloat16_and_fp8_widen() {
        assert_eq!(f32_of(BF16, 0x3F80), 1.0);
        assert_eq!(f32_of(BF16, 0x4049).to_bits(), 0x4049_0000);
        assert_eq!(
            f32_of(BF16, 0x0001).to_bits(),
            0x0001_0000,
            "a bfloat16 subnormal is an f32 subnormal"
        );
        assert_eq!(f32_of(E4M3, 0x38), 1.0);
        assert_eq!(f32_of(E4M3, 0x77), 240.0, "the largest finite value");
        assert!(f32_of(E4M3, 0x7E).is_nan(), "all exponent bits set is NaN");
        assert_eq!(f32_of(E4M3, 0x01), 2f32.powi(-9));
        assert_eq!(f32_of(E5M2, 0x3C), 1.0);
        assert_eq!(f32_of(E5M2, 0x7B), 57344.0);
    }

    #[test]
    fn known_values_narrow() {
        assert_eq!(raw_of(F16, 1.0), Some(0x3C00));
        assert_eq!(raw_of(F16, -2.5), Some(0xC100));
        assert_eq!(raw_of(F16, 65504.0), Some(0x7BFF));
        assert_eq!(raw_of(F16, 65536.0), None, "past the largest f16");
        assert_eq!(raw_of(F16, 2f32.powi(-24)), Some(0x0001));
        assert_eq!(raw_of(F16, 2f32.powi(-25)), None, "below the smallest f16");
        assert_eq!(
            raw_of(F16, 1.0 + 2f32.powi(-11)),
            None,
            "needs 11 mantissa bits"
        );
        assert_eq!(raw_of(F16, 1.0 + 2f32.powi(-10)), Some(0x3C01));
        assert_eq!(raw_of(F16, f32::INFINITY), Some(0x7C00));
        assert_eq!(raw_of(F16, -0.0), Some(0x8000));
        assert_eq!(
            raw_of(F16, f32::from_bits(0x7F80_0001)),
            None,
            "a NaN payload below the f16 bits"
        );
        assert_eq!(raw_of(BF16, 1.0), Some(0x3F80));
        assert_eq!(
            raw_of(BF16, f32::from_bits(0x4049_0FDB)),
            None,
            "pi needs the low bits"
        );
        assert_eq!(raw_of(E4M3, 240.0), Some(0x77));
        assert_eq!(raw_of(E4M3, 256.0), None);
        assert_eq!(raw_of(E4M3, 2f32.powi(-9)), Some(0x01));
        assert_eq!(raw_of(E4M3, 2f32.powi(-10)), None);
    }

    /// Every bit pattern of a layout widens to a value that narrows back to the same bits, NaN payloads included,
    /// through f32 and through f64; and the f32 and the f64 agree.
    #[test]
    fn every_pattern_round_trips() {
        for l in [F16, BF16, E4M3, E5M2] {
            for raw in 0..(1u64 << l.bits) {
                let f32_bits = l.widen(raw, Scalar::F32).unwrap();
                assert_eq!(
                    l.narrow(f32_bits, Scalar::F32),
                    Some(raw),
                    "{l:?} {raw:#x} via f32"
                );
                let f64_bits = l.widen(raw, Scalar::F64).unwrap();
                assert_eq!(
                    l.narrow(f64_bits, Scalar::F64),
                    Some(raw),
                    "{l:?} {raw:#x} via f64"
                );
                assert_eq!(
                    convert(f32_bits, F32, F64),
                    Some(f64_bits),
                    "{l:?} {raw:#x}: the f32 and the f64 agree"
                );
                let v = f32::from_bits(f32_bits as u32);
                if !v.is_nan() {
                    assert_eq!(
                        f64::from(v).to_bits(),
                        f64_bits,
                        "{l:?} {raw:#x} as Rust widens it"
                    );
                }
            }
        }
    }

    /// A layout f32 doesn't hold exactly reads into f64; invalid layouts are told apart.
    #[test]
    fn wide_layouts_take_f64() {
        let wide_exp = FloatLayout {
            bits: 32,
            exponent_bits: 10,
        };
        let wide_mant = FloatLayout {
            bits: 32,
            exponent_bits: 2,
        };
        assert_eq!(wide_exp.scalar(), Scalar::F64);
        assert_eq!(wide_mant.scalar(), Scalar::F64);
        assert_eq!(F16.scalar(), Scalar::F32);
        assert_eq!(BF16.scalar(), Scalar::F32);
        for l in [wide_exp, wide_mant] {
            for raw in [
                0u64,
                1,
                0x7FFF_FFFF,
                0x8000_0001,
                0x4000_0000,
                0xFFFF_FFFF,
                0x0010_0000,
            ] {
                let bits = l.widen(raw, Scalar::F64).unwrap();
                assert_eq!(l.narrow(bits, Scalar::F64), Some(raw), "{l:?} {raw:#x}");
            }
        }
        assert!(wide_exp.is_valid());
        assert!(F64.is_valid());
        assert!(
            !FloatLayout {
                bits: 48,
                exponent_bits: 8
            }
            .is_valid()
        );
        assert!(
            !FloatLayout {
                bits: 64,
                exponent_bits: 8
            }
            .is_valid()
        );
        assert!(
            !FloatLayout {
                bits: 16,
                exponent_bits: 15
            }
            .is_valid()
        );
        assert!(
            !FloatLayout {
                bits: 16,
                exponent_bits: 1
            }
            .is_valid()
        );
    }
}
